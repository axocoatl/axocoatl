//! Podman checks of the workload layout: port sockets outside the Session
//! container under `network: egress`, hardened workload users (a non-root
//! writer and a separate helper, neither with capabilities), the program
//! behind each connection of a hardened egress Session, the seccomp filter on
//! hardened workload users' commands, and Ways attempts that use their
//! Session's egress proxy.
//!
//! The decision point is a fake that maps names to the fixture's upstream and
//! records every event. Objects the fixture creates directly (network,
//! upstream, probe containers) and the egress sidecar, Preview, service
//! forwarder and their volumes carry `io.axocoatl.test=<prefix>-<pid>`, with
//! the prefix from `AXO_TEST_LABEL_PREFIX` (default `workload`). Session
//! containers carry a per-case runtime authority and are removed by exact
//! name. Run with:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1
//! ```
//!
//! `AXO_WORKLOAD_TEST_IMAGE` names a local image that already has Axocoatl's
//! repository commands plus `wget` and `nc` (default: the supervisor's root
//! test image), and `AXO_WORKLOAD_UPSTREAM_IMAGE` one with `node`. The
//! seccomp case runs in `AXO_WORKLOAD_CURATED_IMAGE` (default
//! `docker.io/library/rust:bookworm`, which has Git, Python and Cargo) with
//! Node and npm copied from `docker.io/library/node:22-bookworm-slim`. The
//! Unix-socket case copies that image's `/usr/local/bin/node`, `libstdc++` and
//! `libgcc_s` into a Session container, so both must use the same C library
//! (musl with the defaults).
#![cfg(unix)]

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::egress::protocol::{credential_hash, credential_tag};
use axocoatl_exec::protocol::{
    ExecRequest, ProcessOutcome, ServerMessage, WriteRestriction, PROTOCOL_VERSION,
};
use axocoatl_isolation::egress::{
    CloseReport, Decision, EgressAttachment, EgressAuthority, EgressGrant, GrantKind, GrantSpec,
    OpenRequest, PeerIdentity, ProcessEnv, SidecarEvent,
};
use axocoatl_isolation::{
    ExecIdentity, HelperWorkspaceAccess, Sandbox, SandboxNetwork, SandboxPolicy, SessionSandbox,
    WorkloadUsers,
};
use sha2::{Digest, Sha256};

const TEST_IMAGE: &str = "localhost/axocoatl-supervisor-test-root:20260914";
const UPSTREAM_IMAGE: &str = "docker.io/library/node:22-alpine";
/// A curated image with Git, Python, Perl, Bash and Cargo.
const CURATED_IMAGE: &str = "docker.io/library/rust:bookworm";
/// Node and npm for the curated image (both glibc).
const NODE_IMAGE: &str = "docker.io/library/node:22-bookworm-slim";
const UPSTREAM_SERVER: &str = "require('http').createServer((q,r)=>{console.log('ACCESS '+q.method+' '+q.url);r.end('hello '+q.method+' '+q.url+'\\n')}).listen(8000)";
const USERS: WorkloadUsers = WorkloadUsers {
    writer: (1000, 1000),
    helper: (1001, 1001),
};

fn image(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_string())
}

fn test_label() -> String {
    let prefix = std::env::var("AXO_TEST_LABEL_PREFIX").unwrap_or_else(|_| "workload".into());
    format!("io.axocoatl.test={prefix}-{}", std::process::id())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Bind {
        tag: String,
        kind: GrantKind,
        attempt: Option<String>,
    },
    Open {
        host: String,
        status: Option<u16>,
        tag: Option<String>,
        /// The program behind the connection, as PID 1 named it.
        peer: Option<PeerIdentity>,
    },
}

#[derive(Debug, Default)]
struct FakeState {
    bindings: HashMap<String, String>,
    events: Vec<Event>,
}

/// Allows `upstream.test:8000`, maps it to the fixture's upstream, and
/// records every grant (with its attempt) and decision.
#[derive(Debug)]
struct FakeAuthority {
    env_dir: PathBuf,
    upstream: IpAddr,
    state: Mutex<FakeState>,
    this: Weak<FakeAuthority>,
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
            authority.state.lock().unwrap().bindings.remove(&self.hash);
        }
    }
}

impl FakeAuthority {
    fn events(&self) -> Vec<Event> {
        self.state.lock().unwrap().events.clone()
    }
}

#[async_trait::async_trait]
impl EgressAuthority for FakeAuthority {
    async fn grant(&self, spec: GrantSpec) -> Result<EgressGrant, String> {
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
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        {
            let mut state = self.state.lock().unwrap();
            state.bindings.insert(hash.clone(), tag.clone());
            state.events.push(Event::Bind {
                tag: tag.clone(),
                kind: spec.kind,
                attempt: spec.attempt_id.clone(),
            });
        }
        Ok(EgressGrant::new(
            Some(file.clone()),
            tag,
            None,
            Box::new(Unbind {
                authority: self.this.clone(),
                hash,
                file,
            }),
        ))
    }

    async fn decide(&self, open: OpenRequest) -> Decision {
        let tag = open.auth.as_deref().map(credential_tag);
        let known = open
            .auth
            .as_ref()
            .is_some_and(|hash| self.state.lock().unwrap().bindings.contains_key(hash));
        let decision = if !known {
            Decision::deny(407, "unknown_credential", "no valid credential")
        } else if open.host == "upstream.test" && open.port == 8000 {
            Decision::Allow {
                addrs: vec![self.upstream],
            }
        } else {
            Decision::deny(403, "not_allowed", "not allowed")
        };
        let status = match &decision {
            Decision::Allow { .. } | Decision::Relay => None,
            Decision::Deny { status, .. } => Some(*status),
        };
        self.state.lock().unwrap().events.push(Event::Open {
            host: open.host,
            status,
            tag,
            peer: open.peer,
        });
        decision
    }

    async fn closed(&self, _report: CloseReport) {}

    async fn sidecar_event(&self, _event: SidecarEvent) {}
}

async fn podman(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("podman")
        .args(args)
        .output()
        .await
        .expect("podman runs")
}

async fn podman_ok(args: &[&str]) -> String {
    let output = podman(args).await;
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
    upstream: String,
    _directory: tempfile::TempDir,
    root: SecureDir,
    installation: SecureDir,
    authority: Arc<FakeAuthority>,
    sessions: Mutex<Vec<String>>,
    sandboxes: Mutex<Vec<Arc<SessionSandbox>>>,
}

impl Fixture {
    async fn new(case: u8) -> Self {
        let pid = std::process::id();
        let label = test_label();
        let network = format!("axo-workload-test-{pid}-{case}");
        let octet = 100 + (pid % 100) as u8;
        let subnet = format!("10.{}.{octet}.0/24", 80 + case);
        let upstream_ip: IpAddr = format!("10.{}.{octet}.10", 80 + case).parse().unwrap();
        let directory = tempfile::Builder::new()
            .prefix("axo-workload-podman-")
            .tempdir()
            .unwrap();
        let root = SecureDir::open(directory.path().canonicalize().unwrap()).unwrap();
        let installation = root.child("supervisor-programs").unwrap();
        let env_dir = root.child("egress-env").unwrap().path().to_owned();
        let authority = Arc::new_cyclic(|this| FakeAuthority {
            env_dir,
            upstream: upstream_ip,
            state: Mutex::new(FakeState::default()),
            this: this.clone(),
        });
        let upstream = format!("axo-workload-up-{pid}-{case}");
        let _ = podman(&["rm", "--force", "--time", "0", "--ignore", &upstream]).await;
        let _ = podman(&["network", "rm", "--force", &network]).await;
        podman_ok(&[
            "network", "create", "--label", &label, "--subnet", &subnet, &network,
        ])
        .await;
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
            &image("AXO_WORKLOAD_UPSTREAM_IMAGE", UPSTREAM_IMAGE),
            "node",
            "-e",
            UPSTREAM_SERVER,
        ])
        .await;
        Self {
            label,
            network,
            upstream,
            _directory: directory,
            root,
            installation,
            authority,
            sessions: Mutex::new(Vec::new()),
            sandboxes: Mutex::new(Vec::new()),
        }
    }

    fn policy(&self, workload: Option<WorkloadUsers>) -> SandboxPolicy {
        let identity = uuid::Uuid::new_v4().simple().to_string();
        SandboxPolicy {
            allow_untrusted_image: true,
            network: SandboxNetwork::Egress,
            runtime_authority: Some(format!("{:x}", Sha256::digest(identity.as_bytes()))),
            supervisor_installation: Some(self.installation.clone()),
            egress: Some(EgressAttachment {
                authority: self.authority.clone(),
                sidecar_network: Some(self.network.clone()),
                max_connections: 64,
                labels: vec![self.label.clone()],
                trust_files: None,
            }),
            workload,
            ..SandboxPolicy::default()
        }
    }

    /// A Workspace the image's and the workload users can use, with one
    /// world-readable and one owner-only file.
    fn workspace(&self, session: &str) -> PathBuf {
        let workspace = self.root.child(session).unwrap().path().to_path_buf();
        std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(workspace.join("public.txt"), "public\n").unwrap();
        std::fs::write(workspace.join("private.txt"), "private\n").unwrap();
        std::fs::set_permissions(
            workspace.join("private.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        workspace
    }

    async fn start_with(
        &self,
        session: &str,
        ports: &[u16],
        setup: &[String],
        policy: SandboxPolicy,
    ) -> Arc<SessionSandbox> {
        self.start_image(
            session,
            &image("AXO_WORKLOAD_TEST_IMAGE", TEST_IMAGE),
            ports,
            setup,
            policy,
        )
        .await
    }

    async fn start_image(
        &self,
        session: &str,
        session_image: &str,
        ports: &[u16],
        setup: &[String],
        policy: SandboxPolicy,
    ) -> Arc<SessionSandbox> {
        self.sessions.lock().unwrap().push(session.to_string());
        let workspace = self.workspace(session);
        let sandbox = Arc::new(
            SessionSandbox::start(
                session,
                &workspace,
                Some(session_image),
                ports,
                setup,
                &policy,
            )
            .await
            .expect("the Session starts"),
        );
        self.sandboxes.lock().unwrap().push(sandbox.clone());
        sandbox
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
        // Attempts first: a Session's egress volume cannot go while an
        // attempt still mounts it.
        let mut ordered = sessions.clone();
        ordered.sort_by_key(|session| !session.starts_with("attempt-"));
        for session in &ordered {
            if let Err(error) = SessionSandbox::remove_named_with_dependencies(session).await {
                errors.push(error.to_string());
            }
        }
        let labelled =
            podman_ok(&["ps", "-aq", "--filter", &format!("label={}", self.label)]).await;
        let labelled: Vec<&str> = labelled.split_whitespace().collect();
        if !labelled.is_empty() {
            let mut args = vec!["rm", "--force", "--time", "0", "--depend"];
            args.extend(labelled);
            podman(&args).await;
        }
        let _ = podman(&["network", "rm", "--force", &self.network]).await;
        for session in &sessions {
            for name in ["axo-ses-", "axo-egr-", "axo-svc-", "axo-pvw-"]
                .map(|prefix| format!("{prefix}{session}"))
            {
                if podman(&["container", "exists", &name])
                    .await
                    .status
                    .success()
                {
                    errors.push(format!("{name} is still present"));
                }
            }
            for volume in [
                format!("axo-egr-{session}"),
                format!("axo-egi-{session}"),
                format!("axo-svc-{session}"),
                format!("axo-ses-{session}-node-modules"),
            ] {
                if podman(&["volume", "exists", &volume])
                    .await
                    .status
                    .success()
                {
                    errors.push(format!("volume {volume} is still present"));
                }
            }
        }
        let left = podman_ok(&["ps", "-aq", "--filter", &format!("label={}", self.label)]).await;
        if !left.trim().is_empty() {
            errors.push(format!("labelled containers left: {left}"));
        }
        let volumes = podman_ok(&[
            "volume",
            "ls",
            "-q",
            "--filter",
            &format!("label={}", self.label),
        ])
        .await;
        if !volumes.trim().is_empty() {
            errors.push(format!("labelled volumes left: {volumes}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

async fn with_fixture<F, Fut>(case: u8, body: F)
where
    F: FnOnce(Arc<Fixture>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let fixture = Arc::new(Fixture::new(case).await);
    let inner = fixture.clone();
    let outcome = tokio::spawn(async move { body(inner).await }).await;
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

fn session_name() -> String {
    format!("workload-test-{}", uuid::Uuid::new_v4().simple())
}

/// A read-only helper's shell restriction, as the daemon builds it.
fn helper_restriction(workspace: &Path) -> WriteRestriction {
    WriteRestriction {
        writable: vec!["/tmp".into(), "/var/tmp".into(), "/dev".into()],
        protected: vec![workspace.to_string_lossy().into_owned()],
        deny_network: true,
    }
}

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run `script` through the supervisor as `identity`, the way repository
/// tools do.
async fn supervised(
    sandbox: &dyn Sandbox,
    identity: ExecIdentity,
    script: &str,
    env_file: Option<&Path>,
    restriction: Option<WriteRestriction>,
) -> Ran {
    let request = ExecRequest {
        protocol: PROTOCOL_VERSION,
        stdin: None,
        invocation_id: format!("workload-{}", uuid::Uuid::new_v4().simple()),
        argv: vec!["sh".into(), "-c".into(), script.into()],
        timeout_ms: 30_000,
        stdout_bytes: 16 * 1024,
        stderr_bytes: 16 * 1024,
        write_restriction: restriction,
    };
    let prepared = sandbox
        .prepare_supervised_command_as(request, None, ProcessEnv { env_file }, identity)
        .await
        .unwrap();
    let execution = prepared.dispatch().unwrap().finish().await.unwrap();
    let ServerMessage::Finished {
        outcome,
        stdout,
        stderr,
        ..
    } = execution.result()
    else {
        panic!("no terminal result");
    };
    let text = |captured: &axocoatl_exec::protocol::CapturedOutput| {
        String::from_utf8_lossy(&captured.retained_bytes(16 * 1024).unwrap()).into_owned()
    };
    let code = match outcome {
        ProcessOutcome::Exited { code } => *code,
        other => panic!("{other:?}: {}", text(stderr)),
    };
    Ran {
        code,
        stdout: text(stdout),
        stderr: text(stderr),
    }
}

/// Status fields of the current process, as `name value` lines.
const STATUS: &str = "id -u; id -g; echo HOME=$HOME; \
                      grep -E '^(CapInh|CapPrm|CapEff|CapAmb|NoNewPrivs):' /proc/self/status";

/// The upstream image's node, copied into a Session container's `/tmp`.
const NODE: &str = "LD_LIBRARY_PATH=/tmp/node-lib /tmp/node -e";

/// Apps that listen on Unix sockets of their own: an abstract one, a socket
/// file anyone may connect to (as PostgreSQL's default) and an owner-only one.
const UNIX_APPS: &str = "const net=require('net'),fs=require('fs');\
    const serve=(name,address,mode)=>{const server=net.createServer(c=>c.end(name+'-ok'));\
    server.listen(address,()=>{if(mode)fs.chmodSync(address,mode)})};\
    serve('abstract',String.fromCharCode(0)+'axo-workload-app');\
    serve('open','/tmp/app-open.sock',0o777);\
    serve('private','/tmp/app-private.sock',0o700)";

/// Connects to each of `UNIX_APPS`, printing `name=reply` or `name=ERRNO`.
const UNIX_PROBE: &str = "const net=require('net');\
    const reach=address=>new Promise(done=>{let got='';const socket=net.connect(address);\
    socket.on('data',data=>got+=data);socket.on('end',()=>done(got));\
    socket.on('error',error=>done(error.code))});\
    (async()=>{for(const [name,address] of [['abstract',String.fromCharCode(0)+'axo-workload-app'],\
    ['open','/tmp/app-open.sock'],['private','/tmp/app-private.sock']])\
    console.log(name+'='+await reach(address))})()";

/// One capability set (`CapEff`, `CapBnd`, ...) of process `pid` in
/// `container`, read as root.
async fn capability_set(container: &str, pid: &str, field: &str) -> u64 {
    let status = podman_ok(&[
        "exec",
        "--user",
        "0",
        container,
        "cat",
        &format!("/proc/{pid}/status"),
    ])
    .await;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}:")))
        .unwrap_or_else(|| panic!("no {field} in {status}"));
    u64::from_str_radix(value.trim(), 16).unwrap()
}

/// How many seccomp filters a process `podman exec --user <user>` starts in
/// `container` has: Podman's own profile, without Axocoatl's.
async fn seccomp_filters(container: &str, user: &str) -> u32 {
    let line = podman_ok(&[
        "exec",
        "--user",
        user,
        container,
        "grep",
        "^Seccomp_filters:",
        "/proc/self/status",
    ])
    .await;
    line.trim()
        .strip_prefix("Seccomp_filters:")
        .unwrap_or_else(|| panic!("{line}"))
        .trim()
        .parse()
        .unwrap()
}

fn assert_unprivileged(ran: &Ran, uid: u32, home: &str) {
    let lines: Vec<&str> = ran.stdout.lines().collect();
    assert_eq!(lines[0], uid.to_string(), "{}", ran.stdout);
    assert_eq!(lines[1], uid.to_string(), "{}", ran.stdout);
    assert_eq!(lines[2], format!("HOME={home}"), "{}", ran.stdout);
    for field in ["CapInh", "CapPrm", "CapEff", "CapAmb"] {
        assert!(
            ran.stdout.contains(&format!("{field}:\t0000000000000000")),
            "{field}: {}",
            ran.stdout
        );
    }
    assert!(ran.stdout.contains("NoNewPrivs:\t1"), "{}", ran.stdout);
}

/// Gap 1: under egress the Session container holds no port socket, so a
/// read-only helper's restricted shell cannot reach the Session's apps over TCP
/// or through the port sockets, while Preview and the forwarder's sockets reach
/// them. Landlock does not cover Unix sockets, so an app's own abstract socket,
/// and a socket file whose mode lets the helper's user connect, stay reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn helpers_cannot_reach_the_sessions_apps_but_preview_and_the_forwarder_can() {
    with_fixture(1, |fixture| async move {
        let session = session_name();
        let sandbox = fixture
            .start_with(&session, &[5173], &[], fixture.policy(Some(USERS)))
            .await;
        let workspace = sandbox.root().to_path_buf();
        sandbox.spawn_background(
            "while :; do printf 'HTTP/1.0 200 OK\\r\\n\\r\\napp-ok' | nc -l -p 5173 >/dev/null; done",
        );
        // The writer reaches its own app on loopback.
        let mut reached = false;
        for _ in 0..30 {
            let ran = supervised(
                sandbox.as_ref(),
                ExecIdentity::Writer,
                "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc -w 2 127.0.0.1 5173",
                None,
                None,
            )
            .await;
            if ran.stdout.contains("app-ok") {
                reached = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(reached, "the app did not start");

        // The helper's shell: no socket directory, and TCP is refused.
        let ran = supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            "ls /run/axocoatl-svc; echo ls=$?; \
             printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc -w 2 127.0.0.1 5173; echo nc=$?; \
             wget -q -T 2 -O - http://127.0.0.1:5173/; echo wget=$?; \
             echo sockets-begin; find / -xdev -type s 2>/dev/null; echo sockets-end",
            None,
            Some(helper_restriction(&workspace)),
        )
        .await;
        assert_eq!(ran.code, 0, "{} {}", ran.stdout, ran.stderr);
        assert!(ran.stdout.contains("ls=1") || ran.stdout.contains("ls=2"), "{}", ran.stdout);
        assert!(ran.stderr.contains("/run/axocoatl-svc"), "{}", ran.stderr);
        assert!(!ran.stdout.contains("app-ok"), "{}", ran.stdout);
        assert!(ran.stdout.contains("nc=1"), "{}", ran.stdout);
        assert!(ran.stdout.contains("wget=1"), "{}", ran.stdout);
        assert!(
            ran.stderr.contains("(127.0.0.1): Permission denied"),
            "{}",
            ran.stderr
        );
        // Axocoatl puts no socket file in the helper's view of the container.
        // (`find` cannot list abstract sockets; see below.)
        let sockets = ran
            .stdout
            .split("sockets-begin")
            .nth(1)
            .and_then(|rest| rest.split("sockets-end").next())
            .expect("the socket listing ran");
        assert!(sockets.trim().is_empty(), "sockets in the container: {sockets}");

        // An app that listens on a Unix socket of its own is still within the
        // helper's reach: Landlock does not cover Unix sockets, an abstract
        // socket has no file mode, and only its mode keeps the helper from a
        // socket file. Busybox's nc takes no abstract names, so the apps and
        // the probe use node, copied from the upstream container.
        let container = format!("axo-ses-{session}");
        podman_ok(&["exec", "--user", "0", &container, "mkdir", "-p", "/tmp/node-lib"]).await;
        for (from, to) in [
            ("/usr/local/bin/node", "/tmp/node"),
            ("/usr/lib/libstdc++.so.6", "/tmp/node-lib/libstdc++.so.6"),
            ("/usr/lib/libgcc_s.so.1", "/tmp/node-lib/libgcc_s.so.1"),
        ] {
            podman_ok(&[
                "cp",
                &format!("{}:{from}", fixture.upstream),
                &format!("{container}:{to}"),
            ])
            .await;
        }
        sandbox.spawn_background(&format!("{NODE} \"{UNIX_APPS}\""));
        let probe = format!(
            "{NODE} \"{UNIX_PROBE}\"; stat -c '%n %a %u' /tmp/app-open.sock /tmp/app-private.sock"
        );
        let expected = [
            "abstract=abstract-ok",
            "open=open-ok",
            "private=private-ok",
            "/tmp/app-open.sock 777 1000",
            "/tmp/app-private.sock 700 1000",
        ];
        let mut writer = None;
        for _ in 0..30 {
            let ran = supervised(sandbox.as_ref(), ExecIdentity::Writer, &probe, None, None).await;
            let ready = expected
                .iter()
                .all(|line| ran.stdout.lines().any(|seen| seen == *line));
            writer = Some(ran);
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let writer = writer.unwrap();
        assert_eq!(
            writer.stdout.lines().collect::<Vec<_>>(),
            expected,
            "writer: {}",
            writer.stderr
        );
        let helper = supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            &probe,
            None,
            Some(helper_restriction(&workspace)),
        )
        .await;
        assert_eq!(
            helper.stdout.lines().collect::<Vec<_>>(),
            [
                "abstract=abstract-ok",
                "open=open-ok",
                "private=EACCES",
                "/tmp/app-open.sock 777 1000",
                "/tmp/app-private.sock 700 1000",
            ],
            "helper: {}",
            helper.stderr
        );

        // The forwarder's socket, from a container that mounts it read-only.
        let probe = podman_ok(&[
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
            &image("AXO_WORKLOAD_TEST_IMAGE", TEST_IMAGE),
            "-c",
            "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc local:/run/axocoatl-svc/5173.sock",
        ])
        .await;
        assert!(probe.contains("app-ok"), "{probe}");

        // Preview through axo-pvw-{session} on the host's loopback.
        let host_port = sandbox.published_host_port(5173).expect("Preview port");
        let mut body = String::new();
        for _ in 0..20 {
            if let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", host_port)).await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
                body.clear();
                let _ = stream.read_to_string(&mut body).await;
                if body.contains("app-ok") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(body.contains("app-ok"), "Preview: {body:?}");
    })
    .await;
}

/// Gap 4: in a hardened egress Session, Agents' processes run as the writer
/// and helpers as a second user, neither with capabilities; the helper cannot
/// read the writer's credential or signal it, neither reaches the proxy's
/// socket, and the writer reaches allowed hosts with its own credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn hardened_writers_and_helpers_are_separate_users_without_capabilities() {
    with_fixture(2, |fixture| async move {
        let session = session_name();
        let sandbox = fixture
            .start_with(
                &session,
                &[],
                &["id -u > /tmp/setup-uid; echo $HOME > /tmp/setup-home".to_string()],
                SandboxPolicy {
                    allow_post_create: true,
                    ..fixture.policy(Some(USERS))
                },
            )
            .await;
        let workspace = sandbox.root().to_path_buf();
        let container = format!("axo-ses-{session}");

        // PID 1 (the bridge) is root and holds CAP_SYS_PTRACE, to name the
        // program behind each connection; the workload users have nothing.
        let pid1 = podman_ok(&["exec", "--user", "0", &container, "grep", "^Uid:", "/proc/1/status"]).await;
        assert!(pid1.starts_with("Uid:\t0\t0"), "{pid1}");
        assert!(capability_set(&container, "1", "CapEff").await & (1 << 19) != 0);
        let writer = supervised(sandbox.as_ref(), ExecIdentity::Writer, STATUS, None, None).await;
        assert_unprivileged(&writer, 1000, "/home/axocoatl");
        let helper = supervised(sandbox.as_ref(), ExecIdentity::Helper, STATUS, None, None).await;
        assert_unprivileged(&helper, 1001, "/tmp");
        let names = supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            "id -un; getent passwd 1000 | cut -d: -f6; stat -c '%u %a' /home/axocoatl",
            None,
            None,
        )
        .await;
        assert_eq!(
            names.stdout.lines().collect::<Vec<_>>(),
            ["axocoatl-helper", "/home/axocoatl", "1000 700"],
            "{}",
            names.stderr
        );
        // Setup ran as the writer, and so do terminals.
        let setup = podman_ok(&["exec", "--user", "0", &container, "cat", "/tmp/setup-uid", "/tmp/setup-home"]).await;
        assert_eq!(setup, "1000\n/home/axocoatl\n");
        let terminal = Sandbox::spawn_terminal(sandbox.as_ref(), "id -u; sleep 5", 24, 80)
            .await
            .unwrap();
        let mut seen = String::new();
        for _ in 0..50 {
            seen = String::from_utf8_lossy(&terminal.snapshot()).into_owned();
            if seen.contains("1000") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(seen.contains("1000"), "terminal: {seen:?}");
        assert!(Sandbox::kill_terminal(sandbox.as_ref(), &terminal.id));
        // The compatibility paths run as the writer too.
        let compat = sandbox.exec(&["id", "-u"], Duration::from_secs(20)).await.unwrap();
        assert_eq!(compat.stdout.trim(), "1000");

        // The writer writes and edits the Workspace; the host sees the files
        // as its own user's.
        let wrote = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "echo one > written.txt && echo two >> written.txt && \
             sed -i 's/one/uno/' written.txt && cat written.txt",
            None,
            None,
        )
        .await;
        assert_eq!(wrote.code, 0, "{}", wrote.stderr);
        let written = workspace.join("written.txt");
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "uno\ntwo\n");
        assert_eq!(
            std::fs::metadata(&written).unwrap().uid(),
            rustix_uid(),
            "the host owner of a file the writer made"
        );

        // The helper reads world-readable files and cannot write, whatever
        // the file modes: Landlock refuses it. Landlock does not cover
        // permission bits or timestamps; only the owner may change those.
        let reads = supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            "cat public.txt; stat -c %u private.txt; cat private.txt >/dev/null 2>&1; echo private=$?; \
             echo nope > helper.txt; echo write=$?; echo nope >> public.txt; echo append=$?; \
             chmod 0644 private.txt; echo chmod=$?; \
             touch -d '2001-01-01 00:00:00' public.txt; echo touch=$?",
            None,
            Some(helper_restriction(&workspace)),
        )
        .await;
        let lines: Vec<&str> = reads.stdout.lines().collect();
        assert_eq!(lines[0], "public", "{}", reads.stderr);
        assert_eq!(lines[3], "write=1", "{}", reads.stdout);
        assert_eq!(lines[4], "append=1", "{}", reads.stdout);
        assert!(!workspace.join("helper.txt").exists());
        assert_eq!(std::fs::read_to_string(workspace.join("public.txt")).unwrap(), "public\n");
        let private_mode = std::fs::metadata(workspace.join("private.txt")).unwrap().mode() & 0o7777;
        let public_mtime = std::fs::metadata(workspace.join("public.txt")).unwrap().mtime();
        // On a macOS Podman machine the shared folder reports every file as
        // owned by whoever asks, so file modes do not separate the users
        // there and the helper, as the apparent owner, can change permission
        // bits and timestamps (documented). Where it reports the real owner
        // (the writer), the helper can do neither and cannot read an
        // owner-only file.
        if lines[1] == "1001" {
            assert_eq!(lines[2], "private=0", "{}", reads.stdout);
            assert_eq!(lines[5], "chmod=0", "{}", reads.stdout);
            assert_eq!(lines[6], "touch=0", "{}", reads.stdout);
            assert_eq!(private_mode, 0o644);
            assert_eq!(public_mtime, 978_307_200);
        } else {
            assert_eq!(lines[1], "1000", "{}", reads.stdout);
            assert_eq!(lines[2], "private=1", "{}", reads.stdout);
            assert_eq!(lines[5], "chmod=1", "{}", reads.stdout);
            assert_eq!(lines[6], "touch=1", "{}", reads.stdout);
            assert!(reads.stderr.contains("Operation not permitted"), "{}", reads.stderr);
            assert_eq!(private_mode, 0o600);
            assert!(public_mtime > 978_307_200);
        }

        // The helper can enter and list this Workspace (0755), so read-only
        // Agents can read it. Where the file modes separate the users, one
        // other users may not enter (a mkdtemp directory, 0700) is closed to
        // it; a macOS shared folder reports the helper as its owner and lets
        // it in. One that nobody may list is closed to it everywhere.
        assert_eq!(
            sandbox.helper_workspace_access().await.unwrap(),
            Some(HelperWorkspaceAccess::Readable)
        );
        let shared_folder = lines[1] == "1001";
        for (mode, readable) in [(0o700, shared_folder), (0o000, false)] {
            std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(mode)).unwrap();
            let access = Sandbox::helper_workspace_access(sandbox.as_ref()).await;
            std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o755)).unwrap();
            match access.unwrap() {
                Some(HelperWorkspaceAccess::Readable) => assert!(readable, "mode {mode:o}"),
                Some(HelperWorkspaceAccess::Unreadable(printed)) => {
                    assert!(!readable, "mode {mode:o}: {printed}");
                    // What the image's shell and `ls` print, e.g. busybox's
                    // "can't cd to …: Permission denied".
                    assert!(!printed.is_empty(), "mode {mode:o}");
                }
                None => panic!("a hardened container has a helper user to probe"),
            }
        }

        // The writer reaches an allowed host through PID 1's listener with its
        // own credential.
        let grant = fixture
            .authority
            .grant(GrantSpec::new(GrantKind::Agent))
            .await
            .unwrap();
        let env = grant.env_file.clone().unwrap();
        let fetched = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "wget -q -O - http://upstream.test:8000/writer",
            Some(&env),
            None,
        )
        .await;
        assert_eq!(fetched.stdout.trim(), "hello GET /writer", "{}", fetched.stderr);
        assert!(fixture.authority.events().iter().any(|event| matches!(event,
            Event::Open { host, status: None, tag: Some(tag), .. } if host == "upstream.test" && *tag == grant.token_tag)));

        // Neither user can reach the proxy's socket: the directory that holds
        // it is root's alone, and it holds only the identity socket, where
        // every connection starts with PID 1's line.
        let listed = podman_ok(&["exec", "--user", "0", &container, "ls", "/run/axocoatl/egress"]).await;
        assert_eq!(listed.trim(), "identity.sock");
        for identity in [ExecIdentity::Writer, ExecIdentity::Helper] {
            let ran = supervised(
                sandbox.as_ref(),
                identity,
                "ls /run/axocoatl 2>&1; echo ls=$?; \
                 printf 'CONNECT upstream.test:8000 HTTP/1.1\\r\\n\\r\\n' | \
                 nc -w 2 local:/run/axocoatl/egress/identity.sock 2>&1; echo nc=$?",
                None,
                None,
            )
            .await;
            assert!(ran.stdout.contains("Permission denied"), "{identity:?}: {}", ran.stdout);
            assert!(ran.stdout.contains("ls=1") || ran.stdout.contains("ls=2"), "{}", ran.stdout);
            assert!(!ran.stdout.contains("HTTP/1.1"), "{identity:?}: {}", ran.stdout);
        }

        // A running writer process holds the credential. The helper can read
        // neither its environment nor signal it (SPEC A.8 adversarial #8,
        // which succeeds when everything runs as one user).
        let request = ExecRequest {
            protocol: PROTOCOL_VERSION,
            stdin: None,
            invocation_id: "workload-writer-sleep".into(),
            argv: vec!["sh".into(), "-c".into(), "exec sleep 30".into()],
            timeout_ms: 60_000,
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            write_restriction: None,
        };
        let running = Sandbox::prepare_supervised_command_as(
            sandbox.as_ref(),
            request,
            None,
            ProcessEnv { env_file: Some(&env) },
            ExecIdentity::Writer,
        )
        .await
        .unwrap()
        .dispatch()
        .unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let token = std::fs::read_to_string(&env)
            .unwrap()
            .lines()
            .next()
            .and_then(|line| line.split("axo:").nth(1))
            .and_then(|rest| rest.split('@').next())
            .unwrap()
            .to_string();
        let probe = supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            "pid=$(pgrep -u 1000 -x sleep | head -1); echo pid=$pid; \
             cat /proc/$pid/environ >/dev/null 2>/tmp/environ-error; echo environ=$?; cat /tmp/environ-error; \
             kill -TERM $pid 2>&1; echo kill=$?; \
             for f in /proc/[0-9]*/environ; do tr '\\0' '\\n' < $f 2>/dev/null; done | grep -c '^HTTPS_PROXY=' ; \
             readlink /proc/$pid/exe >/dev/null 2>&1; echo exe=$?",
            None,
            None,
        )
        .await;
        assert!(!probe.stdout.contains("pid=\n"), "no writer process: {}", probe.stdout);
        assert!(probe.stdout.contains("environ=1"), "{}", probe.stdout);
        assert!(probe.stdout.contains("Permission denied"), "{}", probe.stdout);
        assert!(probe.stdout.contains("kill=1"), "{}", probe.stdout);
        assert!(probe.stdout.contains("Operation not permitted"), "{}", probe.stdout);
        assert!(probe.stdout.lines().any(|line| line == "0"), "{}", probe.stdout);
        assert!(probe.stdout.contains("exe=1"), "{}", probe.stdout);
        assert!(!probe.stdout.contains(&token) && !probe.stderr.contains(&token));
        // Another writer command cannot read it either: each hardened command
        // runs in a Landlock domain of its own, which refuses ptrace access,
        // and with it `/proc/<pid>/environ`, to processes outside it.
        let other = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "pid=$(pgrep -u 1000 -x sleep | head -1); echo pid=$pid; \
             cat /proc/$pid/environ >/dev/null 2>/tmp/writer-environ-error; echo environ=$?; \
             cat /tmp/writer-environ-error",
            None,
            None,
        )
        .await;
        assert!(!other.stdout.contains("pid=\n"), "{}", other.stdout);
        assert!(other.stdout.contains("environ=1"), "{}", other.stdout);
        assert!(other.stdout.contains("Permission denied"), "{}", other.stdout);
        // A command still sees its own credential: the separation is between
        // processes, not a broken environment.
        let own = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "cat /proc/self/environ | tr '\\0' '\\n' | grep -c '^HTTPS_PROXY='",
            Some(&env),
            None,
        )
        .await;
        assert_eq!(own.stdout.trim(), "1", "{}", own.stderr);
        running.cancellation().cancel();
        let _ = running.finish().await;
        drop(grant);
    })
    .await;
}

/// A Ways attempt under egress starts no proxy: it uses its Session's, with
/// credentials that name the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn an_attempt_reaches_allowed_hosts_through_its_sessions_proxy() {
    with_fixture(3, |fixture| async move {
        let session = session_name();
        // One daemon: the Session and its attempt share a runtime authority,
        // so neither start treats the other's supervisor mount as foreign.
        let policy = fixture.policy(Some(USERS));
        let parent = fixture.start_with(&session, &[], &[], policy.clone()).await;
        let attempt = format!("attempt-{}-0", uuid::Uuid::new_v4().simple());
        let sandbox = fixture
            .start_with(
                &attempt,
                &[],
                &[
                    "wget -q -O /tmp/setup-fetch http://upstream.test:8000/attempt-setup"
                        .to_string(),
                ],
                SandboxPolicy {
                    allow_post_create: true,
                    shared_sidecar_session: Some(session.clone()),
                    ..policy
                },
            )
            .await;
        // No proxy or volume of the attempt's own; it mounts the Session's.
        assert!(
            !podman(&["container", "exists", &format!("axo-egr-{attempt}")])
                .await
                .status
                .success()
        );
        assert!(
            !podman(&["volume", "exists", &format!("axo-egr-{attempt}")])
                .await
                .status
                .success()
        );
        let mounts = podman_ok(&[
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Name}}={{.Destination}} {{end}}",
            &format!("axo-ses-{attempt}"),
        ])
        .await;
        // A hardened attempt reaches its Session's proxy through the
        // identity socket, so its connections name their programs too.
        assert!(
            mounts.contains(&format!("axo-egi-{session}=/run/axocoatl/egress")),
            "{mounts}"
        );
        assert!(!mounts.contains("axo-egr-"), "{mounts}");
        assert!(sandbox.egress_status().is_none());
        assert!(parent.egress_status().is_some());

        // The setup command used an attempt credential.
        let setup = podman_ok(&[
            "exec",
            "--user",
            "0",
            &format!("axo-ses-{attempt}"),
            "cat",
            "/tmp/setup-fetch",
        ])
        .await;
        assert_eq!(setup.trim(), "hello GET /attempt-setup");
        // An Agent's credential, minted through the attempt's authority.
        let authority = Sandbox::egress_authority(sandbox.as_ref()).expect("an egress authority");
        let mut spec = GrantSpec::new(GrantKind::Agent);
        spec.invocation_id = Some("inv-attempt".into());
        let grant = authority.grant(spec).await.unwrap();
        let fetched = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "wget -q -O - http://upstream.test:8000/attempt-agent",
            grant.env_file.as_deref(),
            None,
        )
        .await;
        assert_eq!(
            fetched.stdout.trim(),
            "hello GET /attempt-agent",
            "{}",
            fetched.stderr
        );
        let events = fixture.authority.events();
        let attempt_tags: Vec<(String, GrantKind)> = events
            .iter()
            .filter_map(|event| match event {
                Event::Bind {
                    tag,
                    kind,
                    attempt: Some(found),
                } if *found == attempt => Some((tag.clone(), *kind)),
                _ => None,
            })
            .collect();
        assert_eq!(
            attempt_tags
                .iter()
                .map(|(_, kind)| *kind)
                .collect::<Vec<_>>(),
            [GrantKind::Setup, GrantKind::Agent],
            "{events:?}"
        );
        for (tag, _) in &attempt_tags {
            assert!(
                events.iter().any(|event| matches!(event,
                    Event::Open { host, status: None, tag: Some(used), peer: Some(peer) }
                        if host == "upstream.test" && used == tag
                            && peer.uid == Some(1000) && peer.exe.is_some())),
                "{tag}: {events:?}"
            );
        }
        // The Session's own credentials carry no attempt.
        let session_grant = Sandbox::egress_authority(parent.as_ref())
            .unwrap()
            .grant(GrantSpec::new(GrantKind::Agent))
            .await
            .unwrap();
        assert!(fixture
            .authority
            .events()
            .iter()
            .any(|event| matches!(event,
            Event::Bind { tag, attempt: None, .. } if *tag == session_grant.token_tag)));
        // The attempt's container also has no network of its own.
        let direct = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "wget -q -T 2 -Y off -O - http://upstream.test:8000/direct 2>&1; echo rc=$?",
            grant.env_file.as_deref(),
            None,
        )
        .await;
        assert!(!direct.stdout.contains("hello"), "{}", direct.stdout);
        // Stopping the attempt leaves the Session's proxy running.
        sandbox.stop_checked().await.unwrap();
        let after = supervised(
            parent.as_ref(),
            ExecIdentity::Writer,
            "wget -q -O - http://upstream.test:8000/after-attempt",
            session_grant.env_file.as_deref(),
            None,
        )
        .await;
        assert_eq!(
            after.stdout.trim(),
            "hello GET /after-attempt",
            "{}",
            after.stderr
        );
        let log = podman_ok(&["logs", &fixture.upstream]).await;
        for path in ["/attempt-setup", "/attempt-agent", "/after-attempt"] {
            assert!(log.contains(&format!("ACCESS GET {path}")), "{log}");
        }
        assert!(!log.contains("/direct"), "{log}");
        drop(grant);
        drop(session_grant);
    })
    .await;
}

/// The image's user still runs everything with `workload: None`, so a root
/// helper can change Workspace permission bits and timestamps, and an attempt
/// cannot share a Session proxy that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn image_mode_keeps_the_image_user_and_an_attempt_needs_its_sessions_proxy() {
    with_fixture(4, |fixture| async move {
        let session = session_name();
        let sandbox = fixture
            .start_with(&session, &[], &[], fixture.policy(None))
            .await;
        let container = format!("axo-ses-{session}");
        let user = podman_ok(&["inspect", "--format", "{{.Config.User}}", &container]).await;
        assert!(user.trim().is_empty() || user.trim() == "root", "{user}");
        let ran = supervised(sandbox.as_ref(), ExecIdentity::Helper, "id -u", None, None).await;
        assert_eq!(
            ran.stdout.trim(),
            "0",
            "image mode runs helpers as the image user"
        );
        // That user started the container with the Workspace: no helper
        // user to probe.
        assert_eq!(sandbox.helper_workspace_access().await.unwrap(), None);
        // Landlock keeps a root helper from writing the Workspace, but not
        // from changing permission bits or timestamps (documented).
        let workspace = sandbox.root().to_path_buf();
        let ran = supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            "echo nope >> public.txt; echo append=$?; chmod 0644 private.txt; echo chmod=$?; \
             touch -d '2001-01-01 00:00:00' public.txt; echo touch=$?",
            None,
            Some(helper_restriction(&workspace)),
        )
        .await;
        assert_eq!(
            ran.stdout.lines().collect::<Vec<_>>(),
            ["append=1", "chmod=0", "touch=0"],
            "{}",
            ran.stderr
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("public.txt")).unwrap(),
            "public\n"
        );
        let private = std::fs::metadata(workspace.join("private.txt")).unwrap();
        assert_eq!(private.mode() & 0o7777, 0o644);
        let public = std::fs::metadata(workspace.join("public.txt")).unwrap();
        assert_eq!(public.mtime(), 978_307_200);
        let volumes = podman_ok(&[
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Destination}} {{end}}",
            &container,
        ])
        .await;
        assert!(volumes.contains("/run/axocoatl-egress"), "{volumes}");
        assert!(
            !volumes.contains("/run/axocoatl/egress") && !volumes.contains("/run/axocoatl-svc"),
            "{volumes}"
        );
        // Without workload users the proxy has no identity socket, nothing
        // keeps CAP_SYS_PTRACE, commands are not hardened, and connections
        // name no program.
        assert!(!podman(&["volume", "exists", &format!("axo-egi-{session}")]).await.status.success());
        assert_eq!(capability_set(&container, "1", "CapBnd").await & (1 << 19), 0);
        let grant = fixture
            .authority
            .grant(GrantSpec::new(GrantKind::Agent))
            .await
            .unwrap();
        let fetched = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "wget -q -O - http://upstream.test:8000/image-mode; grep '^Seccomp_filters:' /proc/self/status",
            grant.env_file.as_deref(),
            None,
        )
        .await;
        let baseline = seccomp_filters(&container, "0").await;
        assert_eq!(
            fetched.stdout.lines().collect::<Vec<_>>(),
            ["hello GET /image-mode".to_string(), format!("Seccomp_filters:\t{baseline}")],
            "{}",
            fetched.stderr
        );
        assert!(fixture.authority.events().iter().any(|event| matches!(event,
            Event::Open { host, status: None, tag: Some(tag), peer: None }
                if host == "upstream.test" && *tag == grant.token_tag)));
        drop(grant);

        let orphan = format!("attempt-{}-0", uuid::Uuid::new_v4().simple());
        fixture.sessions.lock().unwrap().push(orphan.clone());
        let workspace = fixture.workspace(&orphan);
        let error = SessionSandbox::start(
            &orphan,
            &workspace,
            Some(&image("AXO_WORKLOAD_TEST_IMAGE", TEST_IMAGE)),
            &[],
            &[],
            &SandboxPolicy {
                shared_sidecar_session: Some(format!("missing-{}", uuid::Uuid::new_v4().simple())),
                ..fixture.policy(None)
            },
        )
        .await
        .err()
        .expect("no Session proxy to share")
        .to_string();
        assert!(error.contains("start the Session first"), "{error}");
        assert!(
            !podman(&["container", "exists", &format!("axo-ses-{orphan}")])
                .await
                .status
                .success()
        );
    })
    .await;
}

/// Readiness provisioning runs as root in a hardened container, with a
/// provisioning credential of its own, and the Node dependency volume is
/// handed to the writer, also when an earlier image-mode start filled it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn hardened_provisioning_runs_as_root_and_dependencies_belong_to_the_writer() {
    with_fixture(5, |fixture| async move {
        // Plain Alpine lacks Git, so readiness provisions as root. The fake
        // decision point lists no mirror, so the download is refused and the
        // start fails with the egress explanation.
        let session = session_name();
        fixture.sessions.lock().unwrap().push(session.clone());
        let workspace = fixture.workspace(&session);
        let before = fixture.authority.events().len();
        let error = SessionSandbox::start(
            &session,
            &workspace,
            Some("docker.io/library/alpine:3.20"),
            &[],
            &[],
            &fixture.policy(Some(USERS)),
        )
        .await
        .err()
        .expect("provisioning cannot reach a mirror")
        .to_string();
        assert!(
            error.contains("Under network: egress, provisioning reaches only"),
            "{error}"
        );
        let events = fixture.authority.events()[before..].to_vec();
        assert!(events.iter().any(|event| matches!(event,
            Event::Bind { kind: GrantKind::Provisioning, attempt: None, .. })), "{events:?}");
        assert!(events.iter().any(|event| matches!(event,
            Event::Open { host, status: Some(403), .. } if host == "dl-cdn.alpinelinux.org")),
            "{events:?}");

        // A Node project: first started in image mode, where root fills the
        // dependency volume, then hardened.
        let session = session_name();
        let workspace = fixture.workspace(&session);
        std::fs::write(workspace.join("package.json"), "{}\n").unwrap();
        fixture.sessions.lock().unwrap().push(session.clone());
        let policy = fixture.policy(None);
        let image_mode = Arc::new(
            SessionSandbox::start(
                &session,
                &workspace,
                Some(&image("AXO_WORKLOAD_TEST_IMAGE", TEST_IMAGE)),
                &[],
                &[],
                &policy,
            )
            .await
            .unwrap(),
        );
        assert!(image_mode.uses_node_dependency_volume());
        let filled = image_mode
            .exec(
                &["sh", "-c", "mkdir -p node_modules/pkg && echo one > node_modules/pkg/index.js && stat -c %u node_modules/pkg/index.js"],
                Duration::from_secs(20),
            )
            .await
            .unwrap();
        assert_eq!(filled.stdout.trim(), "0", "{}", filled.stderr);
        image_mode.stop_checked().await.unwrap();
        // Close keeps the dependency volume for the next start.
        assert!(podman(&["volume", "exists", &format!("axo-ses-{session}-node-modules")]).await.status.success());

        let hardened = Arc::new(
            SessionSandbox::start(
                &session,
                &workspace,
                Some(&image("AXO_WORKLOAD_TEST_IMAGE", TEST_IMAGE)),
                &[],
                &[],
                &SandboxPolicy {
                    workload: Some(USERS),
                    ..policy
                },
            )
            .await
            .unwrap(),
        );
        fixture.sandboxes.lock().unwrap().push(hardened.clone());
        let ran = supervised(
            hardened.as_ref(),
            ExecIdentity::Writer,
            "echo two >> node_modules/pkg/index.js && mkdir node_modules/other && \
             cat node_modules/pkg/index.js && stat -c '%u:%g' node_modules node_modules/pkg node_modules/other",
            None,
            None,
        )
        .await;
        assert_eq!(ran.code, 0, "{}", ran.stderr);
        assert_eq!(
            ran.stdout.lines().collect::<Vec<_>>(),
            ["one", "two", "1000:1000", "1000:1000", "1000:1000"]
        );
        // The helper can read the packages but not change them.
        let helper = supervised(
            hardened.as_ref(),
            ExecIdentity::Helper,
            "cat node_modules/pkg/index.js >/dev/null && echo read; touch node_modules/x 2>/dev/null; echo touch=$?",
            None,
            None,
        )
        .await;
        assert_eq!(helper.stdout.lines().collect::<Vec<_>>(), ["read", "touch=1"]);
    })
    .await;
}

/// `hardened` applies outside egress too: under `bridge` the container keeps
/// its network and published ports, and its commands run as the writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn hardened_bridge_sessions_publish_ports_and_run_as_the_writer() {
    with_fixture(6, |fixture| async move {
        let session = session_name();
        let sandbox = fixture
            .start_with(
                &session,
                &[3000],
                &[],
                SandboxPolicy {
                    network: SandboxNetwork::Bridge,
                    egress: None,
                    ..fixture.policy(Some(USERS))
                },
            )
            .await;
        let container = format!("axo-ses-{session}");
        let mounts = podman_ok(&[
            "inspect",
            "--format",
            "{{.Config.User}} {{range .Mounts}}{{.Destination}} {{end}}",
            &container,
        ])
        .await;
        assert!(mounts.starts_with("0:0 "), "{mounts}");
        assert!(!mounts.contains("/run/axocoatl"), "{mounts}");
        sandbox.spawn_background(
            "while :; do printf 'HTTP/1.0 200 OK\\r\\n\\r\\nbridge-ok' | nc -l -p 3000 >/dev/null; done",
        );
        let host_port = sandbox.published_host_port(3000).expect("a published port");
        let mut body = String::new();
        for _ in 0..30 {
            if let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", host_port)).await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
                body.clear();
                let _ = stream.read_to_string(&mut body).await;
                if body.contains("bridge-ok") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(body.contains("bridge-ok"), "{body:?}");
        let owner = podman_ok(&["exec", "--user", "0", &container, "sh", "-c", "ps -o user,args | grep 'nc -l' | grep -v grep | head -1"]).await;
        assert!(owner.trim_start().starts_with("axocoatl") || owner.trim_start().starts_with("1000"), "{owner}");
        let writer = supervised(sandbox.as_ref(), ExecIdentity::Writer, STATUS, None, None).await;
        assert_unprivileged(&writer, 1000, "/home/axocoatl");
        // The writer's commands run under the supervisor's filter here too;
        // outside egress nothing names programs, so PID 1 keeps no
        // CAP_SYS_PTRACE.
        let filters = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "grep '^Seccomp_filters:' /proc/self/status",
            None,
            None,
        )
        .await;
        let baseline = seccomp_filters(&container, "1000:1000").await;
        assert_eq!(filters.stdout.trim(), format!("Seccomp_filters:\t{}", baseline + 1));
        assert_eq!(capability_set(&container, "1", "CapBnd").await & (1 << 19), 0);
    })
    .await;
}

/// The peer identity of one recorded connection to the fixture's upstream,
/// from the events after `before`, opened by `exe`.
fn peer_of(fixture: &Fixture, before: usize, exe: &str) -> PeerIdentity {
    let events = fixture.authority.events();
    events[before..]
        .iter()
        .find_map(|event| match event {
            Event::Open {
                host,
                peer: Some(peer),
                ..
            } if host == "upstream.test" && peer.exe.as_deref() == Some(exe) => Some(peer.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no connection by {exe}: {:#?}", &events[before..]))
}

/// Gap 4, identity (J2): in a hardened egress Session the container reaches
/// the proxy only through its identity socket, so every connection the
/// decision point hears of names the program that opened it: its path,
/// SHA-256, user and parents, as PID 1 found them. A writer's tool, the
/// program a tool started, a helper and a terminal are told apart; a line a
/// process writes itself is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn hardened_egress_names_the_program_behind_each_connection() {
    with_fixture(7, |fixture| async move {
        let session = session_name();
        let sandbox = fixture
            .start_with(&session, &[], &[], fixture.policy(Some(USERS)))
            .await;
        let container = format!("axo-ses-{session}");
        assert!(podman(&["volume", "exists", &format!("axo-egi-{session}")]).await.status.success());
        let pid1 = podman_ok(&["exec", "--user", "0", &container, "cat", "/proc/1/cmdline"]).await;
        assert!(
            pid1.contains("127.0.0.1:3128=/run/axocoatl/egress/identity.sock\0--http-errors\0--peer-identity"),
            "{pid1:?}"
        );
        let grant = fixture
            .authority
            .grant(GrantSpec::new(GrantKind::Agent))
            .await
            .unwrap();
        let env = grant.env_file.clone().unwrap();

        // A tool's own program, and a program it started (git's HTTP helper).
        let before = fixture.authority.events().len();
        let ran = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "wget -q -O - http://upstream.test:8000/wget; \
             git ls-remote http://upstream.test:8000/repo.git >/dev/null 2>&1; echo git=$?; \
             for f in \"$(readlink -f \"$(command -v wget)\")\" \"$(readlink -f /bin/sh)\" \
                      \"$(readlink -f \"$(git --exec-path)/git-remote-http\")\" \"$(readlink -f \"$(command -v git)\")\"; do \
               echo \"$f $(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
            Some(&env),
            None,
        )
        .await;
        let lines: Vec<&str> = ran.stdout.lines().collect();
        assert_eq!(lines[0], "hello GET /wget", "{}", ran.stderr);
        assert!(lines[1].starts_with("git="), "{}", ran.stdout);
        let file = |line: &str| {
            let (path, digest) = line.split_once(' ').unwrap();
            (path.to_string(), digest.to_string())
        };
        let (wget, wget_sha) = file(lines[2]);
        let (shell, _) = file(lines[3]);
        let (remote_http, remote_http_sha) = file(lines[4]);
        let (git, _) = file(lines[5]);
        let peer = peer_of(&fixture, before, &wget);
        assert_eq!((peer.uid, peer.gid, peer.error.as_deref()), (Some(1000), Some(1000), None), "{peer:?}");
        assert!(peer.pid.is_some_and(|pid| pid > 1), "{peer:?}");
        assert_eq!(peer.exe_sha256.as_deref(), Some(wget_sha.as_str()), "{peer:?}");
        assert_eq!(peer.ancestors.first(), Some(&shell), "{peer:?}");
        assert!(peer.ancestors.iter().any(|parent| parent == "/axocoatl-exec-supervisor"), "{peer:?}");
        let peer = peer_of(&fixture, before, &remote_http);
        assert_eq!((peer.uid, peer.error.as_deref()), (Some(1000), None), "{peer:?}");
        assert_eq!(peer.exe_sha256.as_deref(), Some(remote_http_sha.as_str()), "{peer:?}");
        // git runs its HTTP helper through `git remote-http`.
        assert_eq!(peer.ancestors.first(), Some(&git), "{peer:?}");
        assert!(peer.ancestors.contains(&shell), "{peer:?}");
        assert!(peer.ancestors.iter().any(|parent| parent == "/axocoatl-exec-supervisor"), "{peer:?}");

        // Every connection from the container is named, including a helper's
        // without a credential (refused) and a terminal's.
        let before = fixture.authority.events().len();
        supervised(
            sandbox.as_ref(),
            ExecIdentity::Helper,
            "http_proxy=http://127.0.0.1:3128 wget -q -T 5 -O - http://upstream.test:8000/helper 2>&1; true",
            None,
            None,
        )
        .await;
        let helper = peer_of(&fixture, before, &wget);
        assert_eq!((helper.uid, helper.gid), (Some(1001), Some(1001)), "{helper:?}");
        assert!(fixture.authority.events()[before..].iter().any(|event| matches!(event,
            Event::Open { status: Some(407), peer: Some(peer), .. } if peer.uid == Some(1001))));
        let before = fixture.authority.events().len();
        let terminal = Sandbox::spawn_terminal(
            sandbox.as_ref(),
            "wget -q -O /tmp/terminal-fetch http://upstream.test:8000/terminal; sleep 5",
            24,
            80,
        )
        .await
        .unwrap();
        let mut named = None;
        for _ in 0..50 {
            named = fixture.authority.events()[before..].iter().find_map(|event| match event {
                Event::Open { host, peer: Some(peer), status: None, .. } if host == "upstream.test" => Some(peer.clone()),
                _ => None,
            });
            if named.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(Sandbox::kill_terminal(sandbox.as_ref(), &terminal.id));
        let named = named.expect("the terminal's connection was named");
        assert_eq!((named.uid, named.exe.as_deref()), (Some(1000), Some(wget.as_str())), "{named:?}");
        assert!(!named.ancestors.iter().any(|parent| parent == "/axocoatl-exec-supervisor"), "{named:?}");

        // A process cannot stand in for another: on the TCP listener PID 1's
        // own line comes first, so a line the client writes is refused
        // before the decision point hears of the connection.
        let before = fixture.authority.events().len();
        let forged = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "printf 'AXO-PEER/1 {\"exe\":\"/usr/bin/git\",\"uid\":0,\"gid\":0}\\r\\nCONNECT upstream.test:8000 HTTP/1.1\\r\\n\\r\\n' | \
             nc -w 3 127.0.0.1 3128 2>&1; true",
            Some(&env),
            None,
        )
        .await;
        assert!(forged.stdout.starts_with("HTTP/1.1 400"), "{}", forged.stdout);
        assert!(forged.stdout.contains("identity_not_accepted"), "{}", forged.stdout);
        assert_eq!(fixture.authority.events().len(), before);
        drop(grant);
    })
    .await;
}

/// The probe for `hardened_commands_run_under_the_seccomp_filter`: each
/// call's result, or its errno's name.
const SYSCALL_PROBE: &str = r#"
import ctypes, errno, platform
libc = ctypes.CDLL(None, use_errno=True)
arm = platform.machine() == "aarch64"
numbers = {
    "ptrace": 117 if arm else 101,
    "process_vm_readv": 270 if arm else 310,
    "keyctl": 219 if arm else 250,
    "unshare_user": 97 if arm else 272,
    "clone3": 435,
    "io_uring_setup": 425,
    "userfaultfd": 282 if arm else 323,
    "bpf": 280 if arm else 321,
}
arguments = {
    "ptrace": (0, 0, 0, 0),
    "process_vm_readv": (__import__("os").getpid(), None, 0, None, 0, 0),
    "keyctl": (0, 0, 0, 0, 0),
    "unshare_user": (0x10000000,),
    "clone3": (None, 0),
    "io_uring_setup": (1, ctypes.create_string_buffer(120)),
    "userfaultfd": (0,),
    "bpf": (5, None, 0),
}
for name, number in numbers.items():
    values = [ctypes.c_long(value) if isinstance(value, int) else value for value in arguments[name]]
    result = libc.syscall(ctypes.c_long(number), *values)
    error = ctypes.get_errno()
    print(f"{name}={result if result >= 0 else errno.errorcode.get(error, error)}")
"#;

/// Gap 4, seccomp (J3): a hardened Session's writers' and helpers' commands
/// run under the supervisor's seccomp filter (one more filter than Podman's
/// own), which refuses ptrace, cross-process memory, keyrings, new user
/// namespaces, clone3 and io_uring, while Git, Python, Perl, Cargo, Node and
/// npm in a curated image still work. The same user without the supervisor
/// (as a terminal runs) is not filtered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test workload_podman -- --ignored --test-threads=1"]
async fn hardened_commands_run_under_the_seccomp_filter() {
    // Case 9: case 8's subnet would be Podman's default network's.
    with_fixture(9, |fixture| async move {
        let session = session_name();
        let sandbox = fixture
            .start_image(
                &session,
                &image("AXO_WORKLOAD_CURATED_IMAGE", CURATED_IMAGE),
                &[],
                &[],
                fixture.policy(Some(USERS)),
            )
            .await;
        let container = format!("axo-ses-{session}");
        let probe = format!(
            "probe=/tmp/probe-$(id -u).py; cat > $probe <<'PROBE'\n{SYSCALL_PROBE}\nPROBE\n\
             python3 $probe; grep -E '^(Seccomp_filters|NoNewPrivs):' /proc/self/status; \
             unshare -U true 2>&1; echo unshare=$?"
        );
        let baseline = seccomp_filters(&container, "1000:1000").await;
        for identity in [ExecIdentity::Writer, ExecIdentity::Helper] {
            let ran = supervised(sandbox.as_ref(), identity, &probe, None, None).await;
            let seen: Vec<&str> = ran.stdout.lines().collect();
            for expected in [
                "ptrace=EPERM",
                "process_vm_readv=EPERM",
                "keyctl=EPERM",
                "unshare_user=EPERM",
                "clone3=ENOSYS",
                "io_uring_setup=ENOSYS",
                "userfaultfd=EPERM",
                "bpf=EPERM",
                "NoNewPrivs:\t1",
                "unshare=1",
            ] {
                assert!(seen.contains(&expected), "{identity:?}: {expected}: {}", ran.stdout);
            }
            assert!(
                seen.contains(&format!("Seccomp_filters:\t{}", baseline + 1).as_str()),
                "{identity:?}: {}",
                ran.stdout
            );
            assert!(ran.stdout.contains("Operation not permitted"), "{}", ran.stdout);
        }
        // The same user outside the supervisor (as terminals run): only
        // Podman's profile, which allows what the filter refuses here.
        let unfiltered = podman_ok(&[
            "exec", "--user", "1000:1000", &container, "sh", "-c",
            "python3 /tmp/probe-1000.py; unshare -U true; echo unshare=$?",
        ])
        .await;
        for expected in ["ptrace=0", "process_vm_readv=0", "unshare_user=0", "clone3=EINVAL", "unshare=0"] {
            assert!(unfiltered.lines().any(|line| line == expected), "{expected}: {unfiltered}");
        }
        assert!(!unfiltered.contains("keyctl=EPERM"), "{unfiltered}");

        // Ordinary work still runs under the filter: Git in the Workspace,
        // Python with threads and a child process, Perl, Bash, Cargo, and
        // Node and npm (copied from the Node image, same C library).
        let node_from = format!("axo-workload-node-{}", std::process::id());
        podman_ok(&["create", "--name", &node_from, "--label", &fixture.label, NODE_IMAGE]).await;
        podman_ok(&["exec", "--user", "0", &container, "mkdir", "-p", "/opt/node/bin", "/opt/node/lib/node_modules"]).await;
        for (from, to) in [
            ("/usr/local/bin/node", "/opt/node/bin/node"),
            ("/usr/local/lib/node_modules/npm", "/opt/node/lib/node_modules/npm"),
        ] {
            podman_ok(&["cp", &format!("{node_from}:{from}"), &format!("{container}:{to}")]).await;
        }
        podman_ok(&["rm", &node_from]).await;
        let work = supervised(
            sandbox.as_ref(),
            ExecIdentity::Writer,
            "set -e; git init -q . && git add public.txt && git status --short public.txt; \
             python3 -c 'import subprocess, threading; t = threading.Thread(target=lambda: None); t.start(); t.join(); subprocess.run([\"true\"], check=True); print(\"python ok\")'; \
             perl -e 'print \"perl ok\\n\"'; bash -c 'echo bash ok'; cargo --version >/dev/null && echo cargo ok; \
             /opt/node/bin/node -e \"require('child_process').execSync('true'); new (require('worker_threads').Worker)('1',{eval:true}).on('exit',c=>{console.log('node ok');process.exit(c)})\"; \
             PATH=/opt/node/bin:$PATH /opt/node/bin/node /opt/node/lib/node_modules/npm/bin/npm-cli.js --version >/dev/null && echo npm ok",
            None,
            None,
        )
        .await;
        assert_eq!(work.code, 0, "{}\n{}", work.stdout, work.stderr);
        assert_eq!(
            work.stdout.lines().collect::<Vec<_>>(),
            ["A  public.txt", "python ok", "perl ok", "bash ok", "cargo ok", "node ok", "npm ok"],
            "{}",
            work.stderr
        );
        // Terminals run as the writer without the filter.
        let terminal = Sandbox::spawn_terminal(
            sandbox.as_ref(),
            "grep '^Seccomp_filters:' /proc/self/status; sleep 5",
            24,
            80,
        )
        .await
        .unwrap();
        let expected = format!("Seccomp_filters:\t{baseline}");
        let mut seen = String::new();
        for _ in 0..50 {
            seen = String::from_utf8_lossy(&terminal.snapshot()).into_owned();
            if seen.contains(&expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(Sandbox::kill_terminal(sandbox.as_ref(), &terminal.id));
        assert!(seen.contains(&expected), "terminal: {seen:?}");
    })
    .await;
}

fn rustix_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}
