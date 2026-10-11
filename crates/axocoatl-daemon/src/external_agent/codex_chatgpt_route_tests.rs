//! The route Axocoatl adds for a Codex writer with `credential:
//! codex-chatgpt`, through a Session's decision point and route broker: the
//! container's placeholder is replaced by the broker's current access token
//! and the account id, a `401` refreshes the sign-in for the next request,
//! nothing but the Responses API is allowed, `auth.openai.com` is not
//! reachable at all, and no token reaches the record.

use super::*;
use crate::egress_broker::UpstreamConnector;
use crate::session_egress::route_tests::{loopback_is_public, RelaySidecar};
use crate::session_egress::tests::{FakeRecord, FakeResolver};
use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
use axocoatl_config::loadout::AgentRuntime;
use axocoatl_core::SecureDir;
use axocoatl_isolation::egress::{EgressAuthority, GrantKind, GrantSpec, RequestKind};
use axocoatl_session::network_record::{Decision, NetworkEvent};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use std::sync::Mutex as StdMutex;

/// One request the fake ChatGPT backend received.
#[derive(Debug, Clone)]
struct BackendRequest {
    method: String,
    path: String,
    authorization: Vec<String>,
    account: Vec<String>,
}

/// A local HTTPS server for `chatgpt.com` (or another host) that answers
/// `401` to a request with a token it was told to reject and `200`
/// otherwise.
struct FakeBackend {
    port: u16,
    ca: crate::egress_broker::SessionCa,
    seen: Arc<StdMutex<Vec<BackendRequest>>>,
    reject: Arc<StdMutex<Vec<String>>>,
}

impl FakeBackend {
    async fn start() -> Self {
        Self::start_for("chatgpt.com").await
    }

    async fn start_for(host: &str) -> Self {
        let ca = crate::egress_broker::SessionCa::new("chatgpt-backend-test").unwrap();
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let reject: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let (recorded, rejected) = (seen.clone(), reject.clone());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let (recorded, rejected) = (recorded.clone(), rejected.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service =
                        hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                            let (recorded, rejected) = (recorded.clone(), rejected.clone());
                            async move {
                                let values = |name: &str| -> Vec<String> {
                                    request
                                        .headers()
                                        .get_all(name)
                                        .iter()
                                        .map(|value| {
                                            String::from_utf8_lossy(value.as_bytes()).into()
                                        })
                                        .collect()
                                };
                                let seen = BackendRequest {
                                    method: request.method().to_string(),
                                    path: request.uri().path().to_string(),
                                    authorization: values("authorization"),
                                    account: values("chatgpt-account-id"),
                                };
                                let refused =
                                    rejected.lock().unwrap().iter().any(|token| {
                                        seen.authorization == [format!("Bearer {token}")]
                                    });
                                recorded.lock().unwrap().push(seen);
                                let _ = request.into_body().collect().await;
                                let mut response = hyper::Response::new(Full::new(
                                    bytes::Bytes::from_static(b"{\"ok\":true}"),
                                ));
                                if refused {
                                    *response.status_mut() = hyper::StatusCode::UNAUTHORIZED;
                                }
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self {
            port,
            ca,
            seen,
            reject,
        }
    }

    fn verifier(&self) -> Arc<dyn rustls::client::danger::ServerCertVerifier> {
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

/// Send `method path` with `headers` over `pipe` (a relayed connection to
/// `chatgpt.com:port`), trusting the Session's authority; the status.
async fn send(
    egress: &SessionEgress,
    pipe: tokio::io::DuplexStream,
    port: u16,
    request_method: &str,
    request_path: &str,
    headers: &[(&str, &str)],
) -> u16 {
    send_to(
        egress,
        pipe,
        "chatgpt.com",
        port,
        request_method,
        request_path,
        headers,
    )
    .await
    .0
}

/// [`send`] to `host`; the status and the response body.
async fn send_to(
    egress: &SessionEgress,
    pipe: tokio::io::DuplexStream,
    host: &str,
    port: u16,
    request_method: &str,
    request_path: &str,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(egress.authority_der().unwrap()).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let stream = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(
            rustls::pki_types::ServerName::try_from(host.to_string()).unwrap(),
            pipe,
        )
        .await
        .unwrap();
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let connection = tokio::spawn(connection);
    let mut request = hyper::Request::builder()
        .method(request_method)
        .uri(request_path)
        .header(hyper::header::HOST, format!("{host}:{port}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(
            request
                .body(Full::new(bytes::Bytes::from_static(
                    b"{\"model\":\"gpt-5.5\"}",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
        .unwrap_or_default();
    drop(sender);
    let _ = connection.await;
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chatgpt_route_sends_the_brokers_current_token_and_refreshes_after_a_401() {
    let root = data_root();
    let (first, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&first, &refresh, "acct-1"));
    let (second, rotated) = (access_token("acct-1", 3600), refresh_token());
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(refresh_request(&refresh)))
        .respond_with(refreshed(&second, Some(&rotated)))
        .expect(1)
        .mount(&auth)
        .await;
    let backend = FakeBackend::start().await;
    backend.reject.lock().unwrap().push(first.clone());

    // The route a Codex writer with `credential: codex-chatgpt` gets, on
    // the fake's port, its credential the stored sign-in.
    let mut route = crate::external_agent::routes_for_credential(
        AgentRuntime::Codex,
        crate::external_agent::ExternalCredential::CodexChatgpt,
    )
    .unwrap()
    .remove(0);
    assert_eq!(route.host, "chatgpt.com");
    assert_eq!(route.credential.as_deref(), Some(CODEX_CHATGPT_SECRET));
    route.ports = Some(vec![backend.port]);
    let mut credentials = std::collections::BTreeMap::new();
    credentials.insert(
        CODEX_CHATGPT_SECRET.to_string(),
        crate::secret_store::credential_source(root.path(), CODEX_CHATGPT_SECRET)
            .unwrap()
            .unwrap(),
    );
    let mut managed = crate::egress_broker::rules::ManagedCredentials::default();
    managed.add(Arc::new(broker(root.path(), &auth)));
    let env_dir = tempfile::tempdir().unwrap();
    let record = Arc::new(FakeRecord::default());
    let egress = SessionEgress::open_session(
        "ses-codex-chatgpt",
        EgressPolicyConfig {
            routes: vec![route],
            credentials,
            ..EgressPolicyConfig::default()
        },
        record.clone(),
        FakeResolver::with(&[
            ("chatgpt.com", &["127.0.0.1"]),
            ("auth.openai.com", &["127.0.0.1"]),
        ]),
        Some(SecureDir::open(env_dir.path()).unwrap()),
        loopback_is_public,
        RouteSettings {
            upstream: Arc::new(UpstreamConnector::with_verifier(
                backend.verifier(),
                Arc::new(|_| false),
            )),
            managed: Arc::new(managed),
            ..RouteSettings::default()
        },
    )
    .await
    .unwrap();
    let grant = egress
        .grant(GrantSpec {
            activation_id: Some("act-codex".into()),
            agent: Some("writer".into()),
            ..GrantSpec::new(GrantKind::Agent)
        })
        .await
        .unwrap();
    // The container's environment holds the placeholder only.
    let env = std::fs::read_to_string(grant.env_file.as_ref().unwrap()).unwrap();
    assert!(
        env.contains("CODEX_CHATGPT_ACCESS_TOKEN=axocoatl-route:chatgpt.com"),
        "{env}"
    );
    for value in [&first, &second, &refresh, &rotated] {
        assert!(!env.contains(value.as_str()), "{env}");
    }
    let token = env
        .lines()
        .find_map(|line| line.strip_prefix("HTTPS_PROXY=http://axo:"))
        .unwrap()
        .trim_end_matches("@127.0.0.1:3128")
        .to_string();
    let hash = axocoatl_exec::egress::protocol::credential_hash(&token);
    let sidecar = RelaySidecar::attach(&egress, 1).await;
    let placeholder = [
        ("authorization", "Bearer axocoatl-route:chatgpt.com"),
        // A client's own account header is replaced by the route's.
        ("ChatGPT-Account-ID", "someone-else"),
        ("chatgpt_account_id", "someone-else-too"),
    ];
    let responses = "/backend-api/codex/responses";

    // 1: the stored token, which the backend rejects; the broker refreshes.
    let pipe = sidecar
        .open(1, RequestKind::Connect, "chatgpt.com", backend.port, &hash)
        .await
        .unwrap();
    let status = send(&egress, pipe, backend.port, "POST", responses, &placeholder).await;
    assert_eq!(status, 401);
    assert_eq!(stored(root.path()).access_token(), second);
    assert_eq!(stored(root.path()).refresh_token(), rotated);
    // 2: the refreshed token.
    let pipe = sidecar
        .open(2, RequestKind::Connect, "chatgpt.com", backend.port, &hash)
        .await
        .unwrap();
    let status = send(&egress, pipe, backend.port, "POST", responses, &placeholder).await;
    assert_eq!(status, 200);
    // 3: a refresh sent from the container to the route is refused, and so
    // is anything else but the Responses API.
    let pipe = sidecar
        .open(3, RequestKind::Connect, "chatgpt.com", backend.port, &hash)
        .await
        .unwrap();
    let status = send(
        &egress,
        pipe,
        backend.port,
        "POST",
        "/oauth/token",
        &placeholder,
    )
    .await;
    assert_eq!(status, 403);
    // 4: and auth.openai.com, where Codex would refresh, is not reachable.
    assert!(sidecar
        .open(4, RequestKind::Connect, "auth.openai.com", 443, &hash)
        .await
        .is_err());
    auth.verify().await;

    let seen = backend.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    for (request, token) in seen.iter().zip([&first, &second]) {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, responses);
        assert_eq!(request.authorization, [format!("Bearer {token}")]);
        assert_eq!(request.account, ["acct-1"]);
    }
    let events = record.events();
    let allowed: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(event, NetworkEvent::Request { decision: Decision::Allow, credential: Some(name), path, .. }
                if name == CODEX_CHATGPT_SECRET && path == responses)
        })
        .collect();
    assert_eq!(allowed.len(), 2, "{events:#?}");
    assert!(
        events.iter().any(|event| matches!(event,
            NetworkEvent::Request { decision: Decision::Deny, path, reason: Some(reason), .. }
                if path == "/oauth/token" && reason == "route_denied")),
        "{events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(event,
            NetworkEvent::Open { host, decision: Decision::Deny, .. } if host == "auth.openai.com")),
        "{events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::Response { status: 401, .. })),
        "{events:#?}"
    );
    for event in &events {
        let text = serde_json::to_string(event).unwrap();
        for value in [&first, &second, &refresh, &rotated] {
            assert!(!text.contains(value.as_str()), "{text}");
        }
    }
    drop(grant);
}

/// A Session whose only route is `route` (its credential the sign-in stored
/// under `root`, its port the fake's), with the broker refreshing at
/// `auth`; the egress, its grant, the attached relay and its credential.
async fn session_with(
    root: &Path,
    mut route: axocoatl_config::EgressRouteYaml,
    backend: &FakeBackend,
    auth: &MockServer,
    record: Arc<FakeRecord>,
    env_dir: &Path,
) -> (
    Arc<SessionEgress>,
    axocoatl_isolation::egress::EgressGrant,
    RelaySidecar,
    String,
) {
    route.ports = Some(vec![backend.port]);
    let mut credentials = std::collections::BTreeMap::new();
    credentials.insert(
        CODEX_CHATGPT_SECRET.to_string(),
        axocoatl_config::CredentialSourceYaml {
            env: None,
            file: Some(
                crate::secret_store::secret_path(root, CODEX_CHATGPT_SECRET)
                    .display()
                    .to_string(),
            ),
        },
    );
    let mut managed = crate::egress_broker::rules::ManagedCredentials::default();
    managed.add(Arc::new(broker(root, auth)));
    let host = route.host.clone();
    let egress = SessionEgress::open_session(
        "ses-codex-chatgpt-host",
        EgressPolicyConfig {
            routes: vec![route],
            credentials,
            ..EgressPolicyConfig::default()
        },
        record,
        FakeResolver::with(&[(host.as_str(), &["127.0.0.1"])]),
        Some(SecureDir::open(env_dir).unwrap()),
        loopback_is_public,
        RouteSettings {
            upstream: Arc::new(UpstreamConnector::with_verifier(
                backend.verifier(),
                Arc::new(|_| false),
            )),
            managed: Arc::new(managed),
            ..RouteSettings::default()
        },
    )
    .await
    .unwrap();
    let grant = egress
        .grant(GrantSpec {
            activation_id: Some("act-codex".into()),
            agent: Some("writer".into()),
            ..GrantSpec::new(GrantKind::Agent)
        })
        .await
        .unwrap();
    let env = std::fs::read_to_string(grant.env_file.as_ref().unwrap()).unwrap();
    let token = env
        .lines()
        .find_map(|line| line.strip_prefix("HTTPS_PROXY=http://axo:"))
        .unwrap()
        .trim_end_matches("@127.0.0.1:3128")
        .to_string();
    let hash = axocoatl_exec::egress::protocol::credential_hash(&token);
    let sidecar = RelaySidecar::attach(&egress, 1).await;
    (egress, grant, sidecar, hash)
}

/// A route to any host but `chatgpt.com` that names `codex-chatgpt` (from
/// a loadout or the configuration) never gets the sign-in: the broker is
/// not asked, the plain read refuses the stored value, the request is
/// refused with 503 and nothing reaches that host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_to_another_host_naming_codex_chatgpt_never_sends_the_sign_in() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 60), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&auth)
        .await;
    let backend = FakeBackend::start_for("evil.test").await;
    let mut route = crate::external_agent::routes_for_credential(
        AgentRuntime::Codex,
        crate::external_agent::ExternalCredential::CodexChatgpt,
    )
    .unwrap()
    .remove(0);
    route.host = "evil.test".into();
    let record = Arc::new(FakeRecord::default());
    let env_dir = tempfile::tempdir().unwrap();
    let (egress, grant, sidecar, hash) = session_with(
        root.path(),
        route,
        &backend,
        &auth,
        record.clone(),
        env_dir.path(),
    )
    .await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, "evil.test", backend.port, &hash)
        .await
        .unwrap();
    let (status, body) = send_to(
        &egress,
        pipe,
        "evil.test",
        backend.port,
        "POST",
        "/backend-api/codex/responses",
        &[("authorization", "Bearer axocoatl-route:evil.test")],
    )
    .await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("credential_unavailable"), "{body}");
    assert!(backend.seen.lock().unwrap().is_empty());
    auth.verify().await;
    for value in [&access, &refresh] {
        assert!(!body.contains(value.as_str()), "{body}");
        for event in record.events() {
            let text = serde_json::to_string(&event).unwrap();
            assert!(!text.contains(value.as_str()), "{text}");
        }
    }
    // The sign-in is still stored as it was.
    assert_eq!(stored(root.path()).refresh_token(), refresh);
    drop(grant);
}

/// When the broker has no value, the client is told so without the reason,
/// which may name a path on this computer (here: a sign-in file other users
/// could read); the reason goes to the daemon's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_managed_credential_names_no_path_on_this_computer() {
    let root = data_root();
    let (access, refresh) = (access_token("acct-1", 3600), refresh_token());
    store(root.path(), &sign_in(&access, &refresh, "acct-1"));
    let file = crate::secret_store::secret_path(root.path(), CODEX_CHATGPT_SECRET);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let auth = MockServer::start().await;
    let backend = FakeBackend::start().await;
    let route = crate::external_agent::routes_for_credential(
        AgentRuntime::Codex,
        crate::external_agent::ExternalCredential::CodexChatgpt,
    )
    .unwrap()
    .remove(0);
    let record = Arc::new(FakeRecord::default());
    let env_dir = tempfile::tempdir().unwrap();
    let (egress, grant, sidecar, hash) =
        session_with(root.path(), route, &backend, &auth, record, env_dir.path()).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, "chatgpt.com", backend.port, &hash)
        .await
        .unwrap();
    let (status, body) = send_to(
        &egress,
        pipe,
        "chatgpt.com",
        backend.port,
        "POST",
        "/backend-api/codex/responses",
        &[],
    )
    .await;
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("credential_unavailable"), "{body}");
    assert!(body.contains("axocoatl connect codex"), "{body}");
    let root_text = root.path().display().to_string();
    let canonical = root.path().canonicalize().unwrap().display().to_string();
    assert!(
        !body.contains(&root_text) && !body.contains(&canonical) && !body.contains("secrets/"),
        "{body}"
    );
    assert!(backend.seen.lock().unwrap().is_empty());
    drop(grant);
}
