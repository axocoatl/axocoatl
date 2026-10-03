//! Podman checks of `network: egress` through a real Session container, its
//! egress sidecar and the bundled supervisor's proxy and bridge modes.
//!
//! The decision point here is a fake that maps names to the fixture's
//! upstream and records every event; the daemon's own decision point is
//! tested against the proxy in its crate. Everything a case creates carries
//! `io.axocoatl.test=egress-<pid>` or a name derived from it and is removed
//! at the end. Run with:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1
//! ```
//!
//! `AXO_EGRESS_TEST_IMAGE` (root) and `AXO_EGRESS_TEST_NONROOT_IMAGE` (uid
//! 1024) name local images that already have Axocoatl's repository commands,
//! so readiness needs no package download. `AXO_EGRESS_UPSTREAM_IMAGE` is a
//! local image with `node`.
#![cfg(unix)]

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::egress::protocol::{credential_hash, credential_tag};
use axocoatl_exec::protocol::{ExecRequest, ServerMessage, PROTOCOL_VERSION};
use axocoatl_isolation::egress::{
    CloseReport, Decision, EgressAttachment, EgressAuthority, EgressGrant, GrantKind, GrantSpec,
    OpenRequest, ProcessEnv, SidecarEvent,
};
use axocoatl_isolation::{Sandbox, SandboxNetwork, SandboxPolicy, SessionSandbox};
use sha2::{Digest, Sha256};

const ROOT_IMAGE: &str = "localhost/axocoatl-supervisor-test-root:20260914";
const NONROOT_IMAGE: &str = "localhost/axocoatl-supervisor-test-nonroot:20260914";
const UPSTREAM_IMAGE: &str = "docker.io/library/node:22-alpine";
const UPSTREAM_SERVER: &str = "require('http').createServer((q,r)=>{console.log('ACCESS '+q.method+' '+q.url);r.end('hello '+q.method+' '+q.url+' '+(q.headers['proxy-authorization']?'LEAK':'clean')+'\\n')}).listen(8000)";

fn image(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Bind {
        tag: String,
        kind: GrantKind,
    },
    Unbind {
        tag: String,
    },
    Open {
        conn: String,
        host: String,
        port: u16,
        status: Option<u16>,
        reason: Option<String>,
        tag: Option<String>,
    },
    Close {
        conn: String,
        up: u64,
        down: u64,
        outcome: String,
    },
    Sidecar(SidecarEvent),
}

#[derive(Debug, Default)]
struct FakeState {
    bindings: HashMap<String, String>,
    events: Vec<Event>,
}

/// Allows the listed host:port pairs, mapping names to fixed addresses, and
/// records everything in order.
#[derive(Debug)]
struct FakeAuthority {
    env_dir: PathBuf,
    names: HashMap<String, IpAddr>,
    allowed: Vec<(String, u16)>,
    state: Mutex<FakeState>,
    this: Weak<FakeAuthority>,
    /// Refuse every grant, as a decision point whose record is full does.
    refuse_grants: std::sync::atomic::AtomicBool,
}

struct Unbind {
    authority: Weak<FakeAuthority>,
    hash: String,
    file: PathBuf,
}

impl Drop for Unbind {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        if let Some(authority) = self.authority.upgrade() {
            let mut state = authority.state.lock().unwrap();
            if let Some(tag) = state.bindings.remove(&self.hash) {
                state.events.push(Event::Unbind { tag });
            }
        }
    }
}

impl FakeAuthority {
    fn new(
        env_dir: PathBuf,
        names: HashMap<String, IpAddr>,
        allowed: Vec<(String, u16)>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            env_dir,
            names,
            allowed,
            state: Mutex::new(FakeState::default()),
            this: this.clone(),
            refuse_grants: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn events(&self) -> Vec<Event> {
        self.state.lock().unwrap().events.clone()
    }

    fn push(&self, event: Event) {
        self.state.lock().unwrap().events.push(event);
    }
}

#[async_trait::async_trait]
impl EgressAuthority for FakeAuthority {
    async fn grant(&self, spec: GrantSpec) -> Result<EgressGrant, String> {
        if self.refuse_grants.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("recording the egress binding failed: Full".into());
        }
        let token = format!("axe_{}", uuid::Uuid::new_v4().simple());
        let hash = credential_hash(&token);
        let tag = credential_tag(&hash);
        let file = self.env_dir.join(format!("egress-{tag}.env"));
        let url = format!("http://axo:{token}@127.0.0.1:3128");
        let mut contents = String::new();
        for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            contents.push_str(&format!("{name}={url}\n"));
        }
        contents.push_str("NO_PROXY=localhost,127.0.0.1,::1\nno_proxy=localhost,127.0.0.1,::1\n");
        std::fs::write(&file, contents).map_err(|error| error.to_string())?;
        {
            let mut state = self.state.lock().unwrap();
            state.bindings.insert(hash.clone(), tag.clone());
            state.events.push(Event::Bind {
                tag: tag.clone(),
                kind: spec.kind,
            });
        }
        let guard = Unbind {
            authority: self.this.clone(),
            hash,
            file: file.clone(),
        };
        Ok(EgressGrant::new(Some(file), tag, None, Box::new(guard)))
    }

    async fn decide(&self, open: OpenRequest) -> Decision {
        let tag = open.auth.as_deref().map(credential_tag);
        let known = open
            .auth
            .as_ref()
            .is_some_and(|hash| self.state.lock().unwrap().bindings.contains_key(hash));
        let decision = if open.auth.is_none() {
            Decision::deny(407, "no_credential", "no credential")
        } else if !known {
            Decision::deny(407, "unknown_credential", "unknown credential")
        } else if self
            .allowed
            .iter()
            .any(|(host, port)| *host == open.host && *port == open.port)
        {
            match self
                .names
                .get(&open.host)
                .copied()
                .or_else(|| open.host.parse().ok())
            {
                Some(address) => Decision::Allow {
                    addrs: vec![address],
                },
                None => Decision::deny(502, "resolve_failed", "no address"),
            }
        } else {
            Decision::deny(
                403,
                "not_allowed",
                format!("{}:{} is not allowed", open.host, open.port),
            )
        };
        let (status, reason) = match &decision {
            Decision::Allow { .. } => (None, None),
            Decision::Deny { status, reason, .. } => (Some(*status), Some(reason.clone())),
        };
        self.push(Event::Open {
            conn: format!("g{}:{}", open.generation, open.id),
            host: open.host,
            port: open.port,
            status,
            reason,
            tag,
        });
        decision
    }

    async fn closed(&self, report: CloseReport) {
        self.push(Event::Close {
            conn: format!("g{}:{}", report.generation, report.id),
            up: report.up,
            down: report.down,
            outcome: format!("{:?}", report.outcome),
        });
    }

    async fn sidecar_event(&self, event: SidecarEvent) {
        self.push(Event::Sidecar(event));
    }
}

fn podman(args: &[&str]) -> std::process::Output {
    std::process::Command::new("podman")
        .args(args)
        .output()
        .expect("podman runs")
}

fn podman_ok(args: &[&str]) -> String {
    let output = podman(args);
    assert!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct Fixture {
    label: String,
    network: String,
    upstream_ip: IpAddr,
    upstream: String,
    _directory: tempfile::TempDir,
    root: SecureDir,
    installation: SecureDir,
    authority: Arc<FakeAuthority>,
    sessions: Mutex<Vec<String>>,
    sandboxes: Mutex<Vec<Arc<SessionSandbox>>>,
    images: Mutex<Vec<String>>,
}

impl Fixture {
    fn new(case: &str) -> Self {
        let pid = std::process::id();
        let label = format!("io.axocoatl.test=egress-{pid}");
        let network = format!("axo-egress-test-{pid}-{case}");
        let octet = 100 + (pid % 100) as u8;
        let third = match case {
            "a" => 0u8,
            "b" => 1,
            "c" => 2,
            "d" => 3,
            _ => 4,
        };
        let subnet = format!("10.{}.{octet}.0/24", 90 + third);
        let upstream_ip: IpAddr = format!("10.{}.{octet}.10", 90 + third).parse().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("axo-egress-podman-")
            .tempdir()
            .unwrap();
        let root = SecureDir::open(directory.path().canonicalize().unwrap()).unwrap();
        let installation = root.child("supervisor-programs").unwrap();
        let env_dir = root.child("egress-env").unwrap().path().to_owned();
        let names = HashMap::from([("upstream.test".to_string(), upstream_ip)]);
        let allowed = vec![
            ("upstream.test".to_string(), 8000),
            (upstream_ip.to_string(), 8000),
        ];
        let authority = FakeAuthority::new(env_dir, names, allowed);
        let upstream = format!("axo-egress-up-{pid}-{case}");
        // A failed earlier run with this pid may have left these behind.
        let _ = podman(&["rm", "--force", "--time", "0", "--ignore", &upstream]);
        let _ = podman(&["network", "rm", "--force", &network]);
        podman_ok(&[
            "network", "create", "--label", &label, "--subnet", &subnet, &network,
        ]);
        podman_ok(&[
            "run",
            "-d",
            "--name",
            &upstream,
            "--label",
            &label,
            "--network",
            &network,
            "--ip",
            &upstream_ip.to_string(),
            &image("AXO_EGRESS_UPSTREAM_IMAGE", UPSTREAM_IMAGE),
            "node",
            "-e",
            UPSTREAM_SERVER,
        ]);
        Self {
            label,
            network,
            upstream_ip,
            upstream,
            _directory: directory,
            root,
            installation,
            authority,
            sessions: Mutex::new(Vec::new()),
            sandboxes: Mutex::new(Vec::new()),
            images: Mutex::new(Vec::new()),
        }
    }

    fn policy(&self) -> SandboxPolicy {
        let identity = uuid::Uuid::new_v4().simple().to_string();
        SandboxPolicy {
            allow_untrusted_image: true,
            network: SandboxNetwork::Egress,
            runtime_authority: Some(format!("{:x}", Sha256::digest(identity.as_bytes()))),
            supervisor_installation: Some(self.installation.clone()),
            egress: Some(EgressAttachment {
                authority: self.authority.clone(),
                sidecar_network: Some(self.network.clone()),
                max_connections: 128,
                labels: vec![self.label.clone()],
            }),
            ..SandboxPolicy::default()
        }
    }

    async fn start(&self, image: &str, ports: &[u16]) -> (String, Arc<SessionSandbox>) {
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        self.sessions.lock().unwrap().push(session.clone());
        let workspace = self.root.child(&session).unwrap();
        // The non-root image's user must be able to use its Workspace.
        std::fs::set_permissions(
            workspace.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o777),
        )
        .unwrap();
        let sandbox = Arc::new(
            SessionSandbox::start(
                &session,
                workspace.path(),
                Some(image),
                ports,
                &[],
                &self.policy(),
            )
            .await
            .expect("an egress Session starts"),
        );
        self.sandboxes.lock().unwrap().push(sandbox.clone());
        (session, sandbox)
    }

    async fn grant(&self) -> (EgressGrant, String) {
        let grant = self
            .authority
            .grant(GrantSpec::new(GrantKind::Agent))
            .await
            .unwrap();
        let contents = std::fs::read_to_string(grant.env_file.as_ref().unwrap()).unwrap();
        let token = contents
            .lines()
            .next()
            .and_then(|line| line.split("axo:").nth(1))
            .and_then(|rest| rest.split('@').next())
            .unwrap()
            .to_string();
        (grant, token)
    }

    async fn cleanup(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        let sandboxes: Vec<_> = self.sandboxes.lock().unwrap().drain(..).collect();
        for sandbox in sandboxes {
            if let Err(error) = sandbox.stop_checked().await {
                errors.push(error.to_string());
            }
        }
        let sessions = self.sessions.lock().unwrap().clone();
        for session in &sessions {
            if let Err(error) = SessionSandbox::remove_named_with_dependencies(session).await {
                errors.push(error.to_string());
            }
        }
        let labelled = podman_ok(&["ps", "-aq", "--filter", &format!("label={}", self.label)]);
        let labelled: Vec<&str> = labelled.split_whitespace().collect();
        if !labelled.is_empty() {
            let mut args = vec!["rm", "--force", "--time", "0"];
            args.extend(labelled);
            podman(&args);
        }
        let _ = podman(&["network", "rm", "--force", &self.network]);
        for image in self.images.lock().unwrap().iter() {
            let _ = podman(&["rmi", "--force", image]);
        }
        for session in &sessions {
            for name in [format!("axo-ses-{session}"), format!("axo-egr-{session}")] {
                if podman(&["container", "exists", &name]).status.success() {
                    errors.push(format!("{name} is still present"));
                }
            }
            for volume in [format!("axo-egr-{session}"), format!("axo-svc-{session}")] {
                if podman(&["volume", "exists", &volume]).status.success() {
                    errors.push(format!("volume {volume} is still present"));
                }
            }
        }
        let left = podman_ok(&["ps", "-aq", "--filter", &format!("label={}", self.label)]);
        if !left.trim().is_empty() {
            errors.push(format!("labelled containers left: {left}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

async fn with_fixture<F, Fut>(name: &str, case: F)
where
    F: FnOnce(Arc<Fixture>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let fixture = Arc::new(Fixture::new(name));
    let body = fixture.clone();
    let outcome = tokio::spawn(async move { case(body).await }).await;
    let cleanup = fixture.cleanup().await;
    assert!(
        cleanup.is_ok(),
        "cleanup failed: {cleanup:?}; case: {outcome:?}"
    );
    if let Err(error) = outcome {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("case cancelled: {error}");
    }
}

/// `podman exec` in the Session container: (exit code, stdout + stderr).
/// Asynchronous, so no runtime worker blocks while the proxy decides.
async fn exec(
    container: &str,
    env_file: Option<&Path>,
    user: Option<&str>,
    script: &str,
) -> (i32, String) {
    let mut args: Vec<String> = vec!["exec".into()];
    if let Some(env_file) = env_file {
        args.push("--env-file".into());
        args.push(env_file.to_string_lossy().into_owned());
    }
    if let Some(user) = user {
        args.push("--user".into());
        args.push(user.into());
    }
    args.extend([container.into(), "sh".into(), "-c".into(), script.into()]);
    let output = tokio::process::Command::new("podman")
        .args(&args)
        .output()
        .await
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn basic(token: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(format!("axo:{token}"))
}

async fn raw(container: &str, request: &str) -> String {
    exec(
        container,
        None,
        None,
        &format!("printf '{request}' | nc 127.0.0.1 3128"),
    )
    .await
    .1
}

/// `podman` from inside a case, without blocking a runtime worker.
async fn podman_async(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("podman")
        .args(args)
        .output()
        .await
        .expect("podman runs")
}

async fn podman_async_ok(args: &[&str]) -> String {
    let output = podman_async(args).await;
    assert!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn allowed_unlisted_and_tokenless_requests_are_decided_and_recorded() {
    with_fixture("a", |fixture| async move {
        let (session, sandbox) = fixture.start(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE), &[]).await;
        let container = format!("axo-ses-{session}");
        let (grant, token) = fixture.grant().await;
        let env = grant.env_file.clone().unwrap();

        // Allowed, plain HTTP through the proxy variables.
        let (code, output) = exec(
            &container,
            Some(&env),
            None,
            "wget -q -O - http://upstream.test:8000/ok?secret=1",
        ).await;
        assert_eq!(code, 0, "{output}");
        assert_eq!(output.trim(), "hello GET /ok?secret=1 clean");

        // Allowed, CONNECT tunnel.
        let tunnel = raw(
            &container,
            &format!(
                "CONNECT upstream.test:8000 HTTP/1.1\\r\\nProxy-Authorization: Basic {}\\r\\n\\r\\nGET /tunnel HTTP/1.0\\r\\n\\r\\n",
                basic(&token)
            ),
        ).await;
        assert!(tunnel.starts_with("HTTP/1.1 200 Connection Established"), "{tunnel}");
        assert!(tunnel.contains("hello GET /tunnel clean"), "{tunnel}");

        // Unlisted host: 403 with the JSON body and hint.
        let refused = raw(
            &container,
            &format!(
                "GET http://evil.test/ HTTP/1.1\\r\\nHost: evil.test\\r\\nProxy-Authorization: Basic {}\\r\\n\\r\\n",
                basic(&token)
            ),
        ).await;
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(refused.contains("X-Axocoatl-Egress: denied; reason=not_allowed"), "{refused}");
        assert!(refused.contains("\"error\":\"egress_denied\""), "{refused}");
        assert!(refused.contains("\"reason\":\"not_allowed\""), "{refused}");
        assert!(refused.contains("evil.test:80 is not allowed"), "{refused}");

        // No credential: 407 with a challenge.
        let tokenless = raw(
            &container,
            "CONNECT upstream.test:8000 HTTP/1.1\\r\\n\\r\\n",
        ).await;
        assert!(tokenless.starts_with("HTTP/1.1 407"), "{tokenless}");
        assert!(tokenless.contains("Proxy-Authenticate: Basic realm=\"axocoatl-egress\""), "{tokenless}");

        // The container itself has no route, no resolver and only loopback.
        let (_, links) = exec(&container, None, None, "ip -o link | awk -F': ' '{print $2}'").await;
        assert_eq!(links.trim(), "lo", "{links}");
        let (code, direct) = exec(
            &container,
            None,
            None,
            &format!("wget -q -T 3 -O - http://{}:8000/direct", fixture.upstream_ip),
        ).await;
        assert_ne!(code, 0, "{direct}");
        assert!(direct.contains("unreachable"), "{direct}");
        let (code, _) = exec(&container, None, None, "nslookup upstream.test >/dev/null 2>&1").await;
        assert_ne!(code, 0);

        // A process that ignores the proxy variables does not get out.
        let (code, bypass) = exec(
            &container,
            Some(&env),
            None,
            "wget -q -T 3 -Y off -O - http://upstream.test:8000/bypass",
        ).await;
        assert_ne!(code, 0, "{bypass}");

        // The supervised path (the one Agent tools use) with the env file.
        let request = ExecRequest {
            protocol: PROTOCOL_VERSION,
            stdin: None,
            invocation_id: "egress-supervised:0".into(),
            argv: vec![
                "sh".into(),
                "-c".into(),
                "wget -q -O - http://upstream.test:8000/supervised".into(),
            ],
            timeout_ms: 20_000,
            stdout_bytes: 4096,
            stderr_bytes: 4096,
            write_restriction: None,
        };
        let prepared = Sandbox::prepare_supervised_command_with_env(
            sandbox.as_ref(),
            request,
            None,
            ProcessEnv {
                env_file: Some(&env),
            },
        )
        .await
        .unwrap();
        let execution = prepared.dispatch().unwrap().finish().await.unwrap();
        match execution.result() {
            ServerMessage::Finished { stdout, .. } => {
                let bytes = stdout.retained_bytes(4096).unwrap();
                assert_eq!(String::from_utf8_lossy(&bytes).trim(), "hello GET /supervised clean");
            }
            other => panic!("{other:?}"),
        }

        let tag = grant.token_tag.clone();
        drop(grant);
        // Close frames follow the responses by a moment.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = fixture.authority.events();
        let opens: Vec<&Event> = events
            .iter()
            .filter(|event| matches!(event, Event::Open { .. }))
            .collect();
        assert!(matches!(
            opens[0],
            Event::Open { host, port: 8000, status: None, tag: Some(found), .. }
                if host == "upstream.test" && *found == tag
        ));
        assert!(opens.iter().any(|event| matches!(event,
            Event::Open { host, status: Some(403), reason: Some(reason), .. }
                if host == "evil.test" && reason == "not_allowed")));
        assert!(opens.iter().any(|event| matches!(event,
            Event::Open { status: Some(407), reason: Some(reason), tag: None, .. }
                if reason == "no_credential")));
        // Every allowed connection closed with its byte counts.
        for open in &opens {
            if let Event::Open { conn, status: None, .. } = open {
                assert!(events.iter().any(|event| matches!(event,
                    Event::Close { conn: closed, down, outcome, .. }
                        if closed == conn && *down > 0 && outcome == "Closed")),
                    "{conn} has no close: {events:?}");
            }
        }
        assert!(events.iter().any(|event| matches!(event, Event::Unbind { tag: unbound } if *unbound == tag)));
        assert!(matches!(
            events.iter().find(|event| matches!(event, Event::Sidecar(_))),
            Some(Event::Sidecar(SidecarEvent::Starting { generation: 1, .. }))
        ));
        let log = podman_async_ok(&["logs", &fixture.upstream]).await;
        assert!(!log.contains("LEAK"));
        assert_eq!(log.matches("ACCESS ").count(), 3, "{log}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn non_root_users_reach_the_proxy_and_serve_port_sockets() {
    with_fixture("b", |fixture| async move {
        let (session, sandbox) = fixture
            .start(&image("AXO_EGRESS_TEST_NONROOT_IMAGE", NONROOT_IMAGE), &[3000])
            .await;
        let container = format!("axo-ses-{session}");
        let (_, id) = exec(&container, None, None, "id -u").await;
        assert_eq!(id.trim(), "1024");
        let (grant, _token) = fixture.grant().await;
        let env = grant.env_file.clone().unwrap();
        let (code, output) = exec(
            &container,
            Some(&env),
            None,
            "wget -q -O - http://upstream.test:8000/nonroot",
        ).await;
        assert_eq!(code, 0, "{output}");
        assert_eq!(output.trim(), "hello GET /nonroot clean");
        // Another uid in the same container uses the same read-only socket.
        let (code, output) = exec(
            &container,
            Some(&env),
            Some("2000:2000"),
            "wget -q -O - http://upstream.test:8000/other-user",
        ).await;
        assert_eq!(code, 0, "{output}");

        // PID 1 (uid 1024) serves port 3000 as a socket in the shared volume;
        // another container that mounts it read-only reaches the app.
        exec(
            &container,
            None,
            None,
            "nohup sh -c 'while :; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nsvc-ok\" | nc -l -p 3000 >/dev/null; done' >/dev/null 2>&1 &",
        ).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let (_, sockets) = exec(&container, None, None, "ls -ln /run/axocoatl-svc").await;
        assert!(sockets.contains("3000.sock"), "{sockets}");
        let reader = podman_async_ok(&[
            "run",
            "--rm",
            "--label",
            &fixture.label,
            "--network",
            "none",
            "--user",
            "3000:3000",
            "--mount",
            &format!("type=volume,source=axo-svc-{session},destination=/run/axocoatl-svc,ro=true"),
            "--entrypoint",
            "/bin/sh",
            &image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE),
            "-c",
            "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc local:/run/axocoatl-svc/3000.sock",
        ]).await;
        assert!(reader.contains("svc-ok"), "{reader}");

        // The egress Preview container publishes the port on host loopback;
        // the Session container itself still has no network.
        let host_port = sandbox.published_host_port(3000).expect("port 3000 is published");
        let preview = format!("axo-pvw-{session}");
        let inspect = podman_async_ok(&[
            "inspect",
            "--format",
            "{{.HostConfig.ReadonlyRootfs}} {{json .Config.Env}} {{json .Mounts}}",
            &preview,
        ])
        .await;
        assert!(inspect.starts_with("true"), "{inspect}");
        assert!(!inspect.contains("\"bind\""), "{inspect}");
        let mut body = String::new();
        for _ in 0..20 {
            if let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", host_port)).await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
                body.clear();
                let _ = stream.read_to_string(&mut body).await;
                if body.contains("svc-ok") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(body.contains("svc-ok"), "Preview through 127.0.0.1:{host_port}: {body:?}");
        sandbox.stop_checked().await.unwrap();
        assert!(!podman_async(&["container", "exists", &preview]).await.status.success());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn the_bridge_is_pid_one_even_when_the_image_has_an_entrypoint() {
    with_fixture("c", |fixture| async move {
        let pid = std::process::id();
        let tag = format!("localhost/axocoatl-test-entrypoint:egress-{pid}");
        let context = fixture.root.child("entrypoint-image").unwrap();
        std::fs::write(
            context.path().join("Containerfile"),
            format!(
                "FROM {}\nENTRYPOINT [\"/bin/sh\",\"-c\"]\nCMD [\"echo started-by-image-entrypoint; sleep infinity\"]\n",
                image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE)
            ),
        )
        .unwrap();
        podman_async_ok(&[
            "build",
            "--pull=never",
            "--label",
            &fixture.label,
            "--tag",
            &tag,
            &context.path().to_string_lossy(),
        ]).await;
        fixture.images.lock().unwrap().push(tag.clone());
        let (session, _sandbox) = fixture.start(&tag, &[]).await;
        let container = format!("axo-ses-{session}");
        let (_, cmdline) = exec(&container, None, None, "tr '\\0' ' ' < /proc/1/cmdline").await;
        assert!(
            cmdline.starts_with("/axocoatl-exec-supervisor --bridge --tcp-to-unix 127.0.0.1:3128=/run/axocoatl-egress/proxy.sock --http-errors"),
            "{cmdline}"
        );
        let logs = podman_async_ok(&["logs", &container]).await;
        assert!(!logs.contains("started-by-image-entrypoint"), "{logs}");
        // PID 1 survives kill attempts from inside its namespace.
        exec(&container, None, Some("0"), "kill -9 1; pkill -9 -f axocoatl-exec-supervisor; true").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (_, after) = exec(&container, None, None, "tr '\\0' ' ' < /proc/1/cmdline").await;
        assert!(after.contains("--bridge"), "{after}");
        let (grant, _) = fixture.grant().await;
        let (code, output) = exec(
            &container,
            grant.env_file.as_deref(),
            None,
            "wget -q -O - http://upstream.test:8000/after-kill",
        ).await;
        assert_eq!(code, 0, "{output}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn a_lost_proxy_fails_closed_restarts_and_stops_with_its_session() {
    with_fixture("d", |fixture| async move {
        let (session, sandbox) = fixture.start(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE), &[]).await;
        let container = format!("axo-ses-{session}");
        let sidecar = format!("axo-egr-{session}");
        // The sidecar carries no environment, secret or bind mount.
        let inspect = podman_async_ok(&[
            "inspect",
            "--format",
            "{{json .Config.Env}} {{json .Mounts}} {{.HostConfig.ReadonlyRootfs}} {{json .HostConfig.CapDrop}}",
            &sidecar,
        ]).await;
        assert!(!inspect.contains("axe_"), "{inspect}");
        assert!(!inspect.contains("\"bind\""), "{inspect}");
        assert!(inspect.contains(" true "), "{inspect}");
        let (grant, _) = fixture.grant().await;
        let env = grant.env_file.clone().unwrap();
        // Kill the proxy from outside.
        podman_async_ok(&["kill", &sidecar]).await;
        let (code, output) = exec(
            &container,
            Some(&env),
            None,
            "wget -S -q -T 5 -O - http://upstream.test:8000/during 2>&1",
        ).await;
        // Either the bridge's 502 or a refused connect; never the upstream.
        assert!(!output.contains("hello"), "{code} {output}");
        // The supervisor restarts it within its first backoff.
        let mut recovered = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if sandbox.egress_status().is_some_and(|status| {
                status.generation == 2
                    && status.phase == axocoatl_isolation::egress_sidecar::SidecarPhase::Ready
            }) {
                recovered = true;
                break;
            }
        }
        assert!(recovered, "{:?}", fixture.authority.events());
        let (code, output) = exec(
            &container,
            Some(&env),
            None,
            "wget -q -O - http://upstream.test:8000/after-restart",
        ).await;
        assert_eq!(code, 0, "{output}");
        let sidecar_events: Vec<SidecarEvent> = fixture
            .authority
            .events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Sidecar(event) => Some(event),
                _ => None,
            })
            .collect();
        assert!(matches!(sidecar_events[0], SidecarEvent::Starting { generation: 1, .. }));
        assert!(matches!(sidecar_events[1], SidecarEvent::Ready { generation: 1, .. }));
        assert!(matches!(sidecar_events[2], SidecarEvent::ChannelLost { generation: 1, .. }));
        assert!(matches!(sidecar_events[3], SidecarEvent::Restarting { generation: 2 }));
        assert!(matches!(sidecar_events[4], SidecarEvent::Starting { generation: 2, .. }));
        assert!(matches!(sidecar_events[5], SidecarEvent::Ready { generation: 2, .. }));

        // Removing the Session by name also stops supervision: no restart.
        SessionSandbox::remove_named(&session).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!podman_async(&["container", "exists", &sidecar]).await.status.success());
        assert!(matches!(
            fixture.authority.events().last(),
            Some(Event::Sidecar(SidecarEvent::Stopped { generation: 2 }))
        ));
        drop(grant);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn bridge_mode_service_sockets_start_on_demand_and_come_back() {
    with_fixture("f", |fixture| async move {
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        let identity = uuid::Uuid::new_v4().simple().to_string();
        let policy = SandboxPolicy {
            allow_untrusted_image: true,
            network: SandboxNetwork::Bridge,
            service_sockets: true,
            runtime_authority: Some(format!("{:x}", Sha256::digest(identity.as_bytes()))),
            supervisor_installation: Some(fixture.installation.clone()),
            ..SandboxPolicy::default()
        };
        let sandbox = Arc::new(
            SessionSandbox::start(
                &session,
                workspace.path(),
                Some(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE)),
                &[3000],
                &[],
                &policy,
            )
            .await
            .unwrap(),
        );
        fixture.sandboxes.lock().unwrap().push(sandbox.clone());
        let container = format!("axo-ses-{session}");
        exec(
            &container,
            None,
            None,
            "nohup sh -c 'while :; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nsvc-ok\" | nc -l -p 3000 >/dev/null; done' >/dev/null 2>&1 &",
        )
        .await;
        let read = |label: String, session: String| async move {
            podman_async_ok(&[
                "run",
                "--rm",
                "--label",
                &label,
                "--network",
                "none",
                "--mount",
                &format!("type=volume,source=axo-svc-{session},destination=/run/axocoatl-svc,ro=true"),
                "--entrypoint",
                "/bin/sh",
                &image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE),
                "-c",
                "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc local:/run/axocoatl-svc/3000.sock",
            ])
            .await
        };
        let sockets = Sandbox::ensure_service_sockets(sandbox.as_ref()).await.unwrap();
        assert_eq!(sockets.volume, format!("axo-svc-{session}"));
        assert_eq!(sockets.ports, [3000]);
        assert!(!sockets.served_by_pid_one);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(read(fixture.label.clone(), session.clone()).await.contains("svc-ok"));
        // Code in the container removes the socket; the next call notices and
        // starts the forwarder again.
        exec(&container, None, Some("0"), "rm -f /run/axocoatl-svc/3000.sock").await;
        Sandbox::ensure_service_sockets(sandbox.as_ref()).await.unwrap();
        assert!(read(fixture.label.clone(), session.clone()).await.contains("svc-ok"));
        // The bridge-mode Session keeps its network and published port.
        let (_, links) = exec(&container, None, None, "ip -o link | awk -F': ' '{print $2}'").await;
        assert!(links.lines().count() > 1, "{links}");
        assert!(sandbox.published_host_port(3000).is_some());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn setup_provisioning_and_terminals_get_credentials_of_their_own() {
    with_fixture("g", |fixture| async move {
        let kinds = |events: &[Event]| -> Vec<GrantKind> {
            events
                .iter()
                .filter_map(|event| match event {
                    Event::Bind { kind, .. } => Some(*kind),
                    _ => None,
                })
                .collect()
        };

        // Provisioning: plain Alpine lacks Git, so readiness runs apk with a
        // provisioning credential. The fake decision point lists no mirror,
        // so the download is refused, recorded, and the start fails with
        // the egress explanation.
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        let error = SessionSandbox::start(
            &session,
            workspace.path(),
            Some("docker.io/library/alpine:3.20"),
            &[],
            &[],
            &fixture.policy(),
        )
        .await
        .err()
        .expect("provisioning cannot reach a mirror");
        assert!(
            error
                .to_string()
                .contains("Under network: egress, provisioning reaches only"),
            "{error}"
        );
        let events = fixture.authority.events();
        assert_eq!(kinds(&events), [GrantKind::Provisioning], "{events:?}");
        assert!(events.iter().any(|event| matches!(event,
            Event::Open { host, status: Some(403), .. } if host == "dl-cdn.alpinelinux.org")),
            "{events:?}");
        assert!(events.iter().any(|event| matches!(event, Event::Unbind { .. })));

        // Setup: an approved post-create command gets a setup credential.
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        let policy = SandboxPolicy {
            allow_post_create: true,
            ..fixture.policy()
        };
        let before = fixture.authority.events().len();
        let sandbox = Arc::new(
            SessionSandbox::start(
                &session,
                workspace.path(),
                Some(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE)),
                &[],
                &["wget -q -O /tmp/from-setup http://upstream.test:8000/setup".to_string()],
                &policy,
            )
            .await
            .unwrap(),
        );
        fixture.sandboxes.lock().unwrap().push(sandbox.clone());
        let container = format!("axo-ses-{session}");
        let (_, fetched) = exec(&container, None, None, "cat /tmp/from-setup").await;
        assert_eq!(fetched.trim(), "hello GET /setup clean");
        let events = fixture.authority.events()[before..].to_vec();
        assert_eq!(kinds(&events), [GrantKind::Setup], "{events:?}");
        let setup_tag = events
            .iter()
            .find_map(|event| match event {
                Event::Bind { tag, .. } => Some(tag.clone()),
                _ => None,
            })
            .unwrap();
        assert!(events.iter().any(|event| matches!(event,
            Event::Open { host, status: None, tag: Some(tag), .. } if host == "upstream.test" && *tag == setup_tag)));
        assert!(events.iter().any(|event| matches!(event, Event::Unbind { tag } if *tag == setup_tag)));

        // A terminal gets its own credential for as long as it lives.
        let before = fixture.authority.events().len();
        let terminal = Sandbox::spawn_terminal(sandbox.as_ref(), "sh", 24, 120)
            .await
            .unwrap();
        let tag = terminal.egress_token_tag().expect("the terminal has a credential");
        terminal
            .input_tx
            .send(b"wget -q -O - http://upstream.test:8000/terminal\n".to_vec())
            .unwrap();
        let mut seen = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if String::from_utf8_lossy(&terminal.snapshot()).contains("hello GET /terminal clean") {
                seen = true;
                break;
            }
        }
        assert!(seen, "{}", String::from_utf8_lossy(&terminal.snapshot()));
        assert!(Sandbox::kill_terminal(sandbox.as_ref(), &terminal.id));
        let events = fixture.authority.events()[before..].to_vec();
        assert_eq!(kinds(&events), [GrantKind::Terminal], "{events:?}");
        assert!(events.iter().any(|event| matches!(event, Event::Unbind { tag: unbound } if *unbound == tag)),
            "killing the terminal ends its credential: {events:?}");
        // After that, the terminal's credential is refused.
        terminal
            .input_tx
            .send(b"wget -q -O - http://upstream.test:8000/after-kill || echo refused-after-kill\n".to_vec())
            .unwrap();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if String::from_utf8_lossy(&terminal.snapshot()).contains("refused-after-kill") {
                break;
            }
        }
        assert!(!String::from_utf8_lossy(&terminal.snapshot()).contains("hello GET /after-kill"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn documented_residuals_a_lingering_setup_process_and_a_borrowed_credential() {
    with_fixture("h", |fixture| async move {
        // A setup command leaves a loop behind. Its credential ends with the
        // setup step, so each later attempt is refused as unknown and
        // recorded with the old credential's tag.
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        let sandbox = Arc::new(
            SessionSandbox::start(
                &session,
                workspace.path(),
                Some(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE)),
                &[],
                &["nohup sh -c 'while :; do wget -q -O /dev/null http://upstream.test:8000/loop; sleep 1; done' >/dev/null 2>&1 &".to_string()],
                &SandboxPolicy {
                    allow_post_create: true,
                    ..fixture.policy()
                },
            )
            .await
            .unwrap(),
        );
        fixture.sandboxes.lock().unwrap().push(sandbox.clone());
        tokio::time::sleep(Duration::from_secs(3)).await;
        let events = fixture.authority.events();
        let setup_tag = events
            .iter()
            .find_map(|event| match event {
                Event::Bind { tag, kind: GrantKind::Setup } => Some(tag.clone()),
                _ => None,
            })
            .unwrap();
        let unbound = events
            .iter()
            .position(|event| matches!(event, Event::Unbind { tag } if *tag == setup_tag))
            .unwrap();
        let refused_after = events[unbound..]
            .iter()
            .filter(|event| matches!(event,
                Event::Open { host, status: Some(407), reason: Some(reason), tag: Some(tag), .. }
                    if host == "upstream.test" && reason == "unknown_credential" && *tag == setup_tag))
            .count();
        assert!(refused_after >= 1, "{events:?}");

        // Agents share the container: a process without a credential can
        // read a running process's credential from /proc. This pins the
        // documented residual; it must not start failing silently.
        let container = format!("axo-ses-{session}");
        let (grant, token) = fixture.grant().await;
        let mut writer = tokio::process::Command::new("podman");
        writer.args(["exec", "-d", "--env-file"]).arg(grant.env_file.as_ref().unwrap());
        writer.args([container.as_str(), "sh", "-c", "exec sleep 30"]);
        writer.stdout(std::process::Stdio::null());
        assert!(writer.status().await.unwrap().success());
        let (code, borrowed) = exec(
            &container,
            None,
            Some("0"),
            "for f in /proc/[0-9]*/environ; do tr '\\0' '\\n' < $f 2>/dev/null; done | grep '^HTTPS_PROXY=' | sort -u",
        )
        .await;
        assert_eq!(code, 0);
        assert!(borrowed.contains(&token), "the residual changed: {borrowed}");
        drop(grant);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn a_sidecar_that_keeps_failing_stays_down_once_its_restart_budget_is_spent() {
    use axocoatl_isolation::egress_sidecar::{
        EgressSidecar, RestartPolicy, SidecarPhase, SidecarSpec,
    };
    use axocoatl_isolation::supervisor_program::SupervisorProgram;
    with_fixture("i", |fixture| async move {
        let arch = podman_async_ok(&["info", "--format", "{{.Host.Arch}}"]).await;
        let arch = match arch.trim() {
            "arm64" | "aarch64" => "aarch64",
            _ => "x86_64",
        };
        let program = SupervisorProgram::install_embedded(arch, &fixture.installation).unwrap();
        let image = axocoatl_isolation::egress_image::ensure_egress_image(&program)
            .await
            .unwrap();
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let sidecar = EgressSidecar::start_with_restarts(
            SidecarSpec {
                session_id: session.clone(),
                runtime_authority: None,
                image,
                network: Some(fixture.network.clone()),
                max_connections: 8,
                require_resource_limits: false,
                labels: vec![fixture.label.clone()],
            },
            fixture.authority.clone(),
            axocoatl_isolation::egress_control::ControlTiming::default(),
            RestartPolicy {
                backoff: [Duration::from_millis(100); 3],
                budget: 2,
                window: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        let container = sidecar.container();
        for lost in 1..=3u32 {
            let wanted = if lost <= 2 {
                SidecarPhase::Ready
            } else {
                SidecarPhase::Failed
            };
            let _ = podman_async(&["kill", &container]).await;
            let mut reached = false;
            for _ in 0..80 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let status = sidecar.status();
                if status.phase == wanted
                    && (wanted == SidecarPhase::Failed || status.generation == lost + 1)
                {
                    reached = true;
                    break;
                }
            }
            assert!(
                reached,
                "loss {lost}: {:?} {:?}",
                sidecar.status(),
                fixture.authority.events()
            );
        }
        let status = sidecar.status();
        assert_eq!((status.phase, status.restarts), (SidecarPhase::Failed, 2));
        assert!(
            fixture.authority.events().iter().any(|event| matches!(
                event,
                Event::Sidecar(SidecarEvent::BudgetSpent { generation: 3, .. })
            )),
            "{:?}",
            fixture.authority.events()
        );
        // It stays down: no container runs for it any more.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!podman_async(&["container", "exists", &container])
            .await
            .status
            .success());
        sidecar.stop().await;
        assert!(matches!(
            fixture.authority.events().last(),
            Some(Event::Sidecar(SidecarEvent::Stopped { .. }))
        ));

        // A stop while a restart waits out its backoff ends the wait at once
        // and is recorded.
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let sidecar = EgressSidecar::start_with_restarts(
            SidecarSpec {
                session_id: session.clone(),
                runtime_authority: None,
                image: axocoatl_isolation::egress_image::ensure_egress_image(&program)
                    .await
                    .unwrap(),
                network: Some(fixture.network.clone()),
                max_connections: 8,
                require_resource_limits: false,
                labels: vec![fixture.label.clone()],
            },
            fixture.authority.clone(),
            axocoatl_isolation::egress_control::ControlTiming::default(),
            RestartPolicy {
                backoff: [Duration::from_secs(30); 3],
                budget: 5,
                window: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        let _ = podman_async(&["kill", &sidecar.container()]).await;
        for _ in 0..50 {
            if sidecar.status().phase == SidecarPhase::Restarting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(sidecar.status().phase, SidecarPhase::Restarting);
        let started = std::time::Instant::now();
        sidecar.stop().await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(sidecar.status().phase, SidecarPhase::Stopped);
        assert!(
            matches!(
                fixture.authority.events().last(),
                Some(Event::Sidecar(SidecarEvent::Stopped { generation: 2 }))
            ),
            "{:?}",
            fixture.authority.events()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn without_a_credential_setup_provisioning_and_terminals_still_run() {
    with_fixture("j", |fixture| async move {
        // A decision point whose record is full grants nothing. Setup and
        // terminals go ahead without a credential, and the proxy refuses
        // their connections; they no longer fail or remove the sandbox.
        fixture
            .authority
            .refuse_grants
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        let sandbox = Arc::new(
            SessionSandbox::start(
                &session,
                workspace.path(),
                Some(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE)),
                &[],
                &["wget -q -O /tmp/from-setup http://upstream.test:8000/setup 2>/tmp/setup-error || echo refused > /tmp/setup-result".to_string()],
                &SandboxPolicy {
                    allow_post_create: true,
                    ..fixture.policy()
                },
            )
            .await
            .expect("setup runs without a credential"),
        );
        fixture.sandboxes.lock().unwrap().push(sandbox.clone());
        let container = format!("axo-ses-{session}");
        // Without an env file the command has no proxy variables either, so
        // it finds no resolver and no route.
        let (_, result) = exec(&container, None, None, "cat /tmp/setup-result /tmp/setup-error").await;
        assert!(result.starts_with("refused"), "{result}");
        let terminal = Sandbox::spawn_terminal(sandbox.as_ref(), "sh", 24, 120)
            .await
            .expect("a terminal opens without a credential");
        assert_eq!(terminal.egress_token_tag(), None);
        assert!(Sandbox::kill_terminal(sandbox.as_ref(), &terminal.id));
        let events = fixture.authority.events();
        assert!(!events.iter().any(|event| matches!(event, Event::Bind { .. })), "{events:?}");
        // A process that still finds the proxy gets nothing without a credential.
        let tokenless = raw(&container, "CONNECT upstream.test:8000 HTTP/1.1\\r\\n\\r\\n").await;
        assert!(tokenless.starts_with("HTTP/1.1 407"), "{tokenless}");

        // Provisioning also runs; its download is refused, and the start
        // fails with the egress explanation, not a grant error.
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        let error = SessionSandbox::start(
            &session,
            workspace.path(),
            Some("docker.io/library/alpine:3.20"),
            &[],
            &[],
            &fixture.policy(),
        )
        .await
        .err()
        .expect("provisioning cannot download without a credential")
        .to_string();
        assert!(!error.contains("granting"), "{error}");
        assert!(error.contains("Under network: egress, provisioning reaches only"), "{error}");
    })
    .await;
}

/// A minimal TLS ClientHello that names `server_name`.
fn client_hello(server_name: &str) -> Vec<u8> {
    let name = server_name.as_bytes();
    let list = 3 + name.len();
    let mut extension = vec![0x00, 0x00];
    extension.extend_from_slice(&((list + 2) as u16).to_be_bytes());
    extension.extend_from_slice(&(list as u16).to_be_bytes());
    extension.push(0x00);
    extension.extend_from_slice(&(name.len() as u16).to_be_bytes());
    extension.extend_from_slice(name);
    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[7u8; 32]);
    body.push(0);
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01, 0x01, 0x00]);
    body.extend_from_slice(&(extension.len() as u16).to_be_bytes());
    body.extend_from_slice(&extension);
    let mut handshake = vec![0x01];
    handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend_from_slice(&body);
    let mut record = vec![0x16, 0x03, 0x01];
    record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
    record.extend_from_slice(&handshake);
    record
}

/// SPEC adversarial cases 1, 4, 7 and 11: no name resolution and no UDP
/// inside the container, no credential in the host's process list during a
/// tool call, and the domain-fronting residual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn names_udp_host_argv_and_the_fronting_residual() {
    with_fixture("k", |fixture| async move {
        let (session, sandbox) = fixture.start(&image("AXO_EGRESS_TEST_IMAGE", ROOT_IMAGE), &[]).await;
        let container = format!("axo-ses-{session}");
        let (grant, token) = fixture.grant().await;
        let env = grant.env_file.clone().unwrap();

        // 1. Nothing resolves inside the container, not through the image's
        // resolver and not through a public server (the curated image has no
        // `dig`; busybox nslookup with an explicit server is the same query).
        let (code, output) = exec(&container, None, None, "getent hosts x.attacker.test").await;
        assert_ne!(code, 0, "{output}");
        let (code, output) =
            exec(&container, None, None, "nslookup x.attacker.test 1.1.1.1 2>&1").await;
        assert_ne!(code, 0, "{output}");
        assert!(output.contains("unreachable"), "{output}");
        let refused = raw(
            &container,
            &format!(
                "CONNECT data.attacker.test:443 HTTP/1.1\\r\\nProxy-Authorization: Basic {}\\r\\n\\r\\n",
                basic(&token)
            ),
        )
        .await;
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(refused.contains("reason=not_allowed"), "{refused}");

        // 4. A UDP datagram to a public resolver has no route (the query
        // above is UDP too; this one goes to 8.8.8.8:53 directly).
        let (code, output) = exec(&container, None, None, "nslookup example.com 8.8.8.8 2>&1; echo x | nc -u -w1 8.8.8.8 53 2>&1").await;
        assert_ne!(code, 0, "{output}");
        assert!(output.contains("Network unreachable"), "{output}");
        let (code, output) = exec(&container, None, None, "ping -c1 -W1 8.8.8.8 2>&1").await;
        assert_ne!(code, 0, "{output}");
        assert!(output.contains("sendto: Network unreachable"), "{output}");

        // 7. While a supervised tool call runs with the credential, the
        // host's process list does not show it.
        let request = ExecRequest {
            protocol: PROTOCOL_VERSION,
            stdin: None,
            invocation_id: "egress-argv:0".into(),
            argv: vec!["sh".into(), "-c".into(), "sleep 3".into()],
            timeout_ms: 20_000,
            stdout_bytes: 4096,
            stderr_bytes: 4096,
            write_restriction: None,
        };
        let prepared = Sandbox::prepare_supervised_command_with_env(
            sandbox.as_ref(),
            request,
            None,
            ProcessEnv {
                env_file: Some(&env),
            },
        )
        .await
        .unwrap();
        let running = prepared.dispatch().unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let listing = tokio::process::Command::new("ps")
            .args(["-A", "-ww", "-o", "args="])
            .output()
            .await
            .unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
        assert!(listing.contains("podman"), "the tool call is not running yet");
        assert!(!listing.contains(&token), "the credential is in a host process's argv");
        assert!(!listing.contains("axe_"), "a credential is in a host process's argv");
        running.finish().await.unwrap();

        // 11. Domain fronting is not detected: a tunnel to an allowed host
        // carries a TLS ClientHello that names another host. This pins the
        // documented residual; it must not assert a block.
        use base64::Engine;
        let hello = base64::engine::general_purpose::STANDARD.encode(client_hello("evil.test"));
        let (_, fronted) = exec(
            &container,
            None,
            None,
            &format!(
                "{{ printf 'CONNECT upstream.test:8000 HTTP/1.1\\r\\nProxy-Authorization: Basic {}\\r\\n\\r\\n'; sleep 1; echo {hello} | base64 -d; sleep 2; }} | nc 127.0.0.1 3128",
                basic(&token)
            ),
        )
        .await;
        assert!(fronted.starts_with("HTTP/1.1 200 Connection Established"), "{fronted}");
        // The upstream received the bytes and answered them.
        assert!(fronted.contains("400 Bad Request"), "{fronted}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let events = fixture.authority.events();
        let fronted_open = events.iter().rev().find_map(|event| match event {
            Event::Open { host, port: 8000, status, .. } if host == "upstream.test" => Some(*status),
            _ => None,
        });
        assert_eq!(fronted_open, Some(None), "{events:?}");
        assert!(!events.iter().any(|event| matches!(event, Event::Open { host, .. } if host == "evil.test")));
        drop(grant);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_podman -- --ignored --test-threads=1"]
async fn the_bridge_mode_forwarder_runs_as_the_image_user() {
    with_fixture("l", |fixture| async move {
        let session = format!("egress-test-{}", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.root.child(&session).unwrap();
        std::fs::set_permissions(
            workspace.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o777),
        )
        .unwrap();
        let identity = uuid::Uuid::new_v4().simple().to_string();
        let policy = SandboxPolicy {
            allow_untrusted_image: true,
            network: SandboxNetwork::Bridge,
            service_sockets: true,
            runtime_authority: Some(format!("{:x}", Sha256::digest(identity.as_bytes()))),
            supervisor_installation: Some(fixture.installation.clone()),
            ..SandboxPolicy::default()
        };
        let sandbox = Arc::new(
            SessionSandbox::start(
                &session,
                workspace.path(),
                Some(&image("AXO_EGRESS_TEST_NONROOT_IMAGE", NONROOT_IMAGE)),
                &[3000],
                &[],
                &policy,
            )
            .await
            .unwrap(),
        );
        fixture.sandboxes.lock().unwrap().push(sandbox.clone());
        let container = format!("axo-ses-{session}");
        Sandbox::ensure_service_sockets(sandbox.as_ref()).await.unwrap();
        // The socket is created connectable by everyone and belongs to the
        // image user, and the forwarder runs as that user, not as root.
        let (_, socket) = exec(&container, None, None, "stat -c '%u %a %F' /run/axocoatl-svc/3000.sock").await;
        assert_eq!(socket.trim(), "1024 666 socket", "{socket}");
        let (_, users) = exec(
            &container,
            None,
            None,
            "for p in /proc/[0-9]*; do if tr '\\0' ' ' < $p/cmdline 2>/dev/null | grep -q '^/axocoatl-exec-supervisor --bridge --unix-to-tcp'; then awk '/^Uid:/{print $2}' $p/status; fi; done",
        )
        .await;
        assert_eq!(users.trim(), "1024", "{users}");
    })
    .await;
}
