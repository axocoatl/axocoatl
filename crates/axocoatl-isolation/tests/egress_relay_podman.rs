//! Podman checks of the supervisor's relay, peer identity and hardening modes
//! through a real egress sidecar built from the embedded supervisor.
//!
//! The decision point is a fake that relays `relay.test` and answers the
//! client itself, as the daemon's TLS terminator will. A client container
//! runs the bundled bridge as PID 1, with the proxy's ordinary socket on
//! `127.0.0.1:3129` and its identity socket on `127.0.0.1:3128` (with
//! `--peer-identity`). Everything a case creates carries
//! `io.axocoatl.test=<AXO_TEST_LABEL, default egress-relay-<pid>>` and is
//! removed at the end. Run after the embedded supervisors are rebuilt:
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-isolation --test egress_relay_podman -- --ignored --test-threads=1
//! ```
//!
//! `AXO_RELAY_CLIENT_IMAGE` names a local glibc image with `curl`, `bash`,
//! `sha256sum`, `git` and `python3` (default: the supervisor build image).
#![cfg(unix)]

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::protocol::{Control, ExecRequest, ProcessOutcome, ServerMessage};
use axocoatl_isolation::egress::{
    CloseReport, Decision, EgressAuthority, EgressGrant, GrantSpec, OpenRequest, PeerIdentity,
    RelayOpen, RelayStream, SidecarEvent,
};
use axocoatl_isolation::egress_sidecar::{
    EgressSidecar, RestartPolicy, SidecarOptions, SidecarSpec,
};
use axocoatl_isolation::supervisor_program::SupervisorProgram;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const CLIENT_IMAGE: &str = "localhost/axocoatl-supervisor-build:20260914";
const NODE_IMAGE: &str = "docker.io/library/node:22-alpine";
const ALPINE_GIT_IMAGE: &str = "localhost/axocoatl-supervisor-test-root:20260914";

fn image(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_string())
}

/// One opened connection as the authority saw it.
#[derive(Debug, Clone)]
struct Opened {
    id: u64,
    host: String,
    peer: Option<PeerIdentity>,
}

/// Relays `relay.test`, refuses everything else, and answers a relayed
/// client with one HTTP response naming the request it read.
#[derive(Debug, Default)]
struct RelayingAuthority {
    opened: Mutex<Vec<Opened>>,
    closed: Mutex<Vec<CloseReport>>,
    relayed: Mutex<Vec<u64>>,
    /// Where `tunnel.test` is allowed to, for the throughput comparison.
    tunnel: Mutex<Option<std::net::IpAddr>>,
}

#[async_trait::async_trait]
impl EgressAuthority for RelayingAuthority {
    async fn grant(&self, _: GrantSpec) -> Result<EgressGrant, String> {
        Err("not used".into())
    }

    async fn decide(&self, open: OpenRequest) -> Decision {
        self.opened.lock().unwrap().push(Opened {
            id: open.id,
            host: open.host.clone(),
            peer: open.peer.clone(),
        });
        let tunnel = *self.tunnel.lock().unwrap();
        if open.host == "relay.test" {
            Decision::Relay
        } else if let (Some(address), "tunnel.test") = (tunnel, open.host.as_str()) {
            Decision::Allow {
                addrs: vec![address],
            }
        } else {
            Decision::deny(403, "not_allowed", format!("{} is not allowed", open.host))
        }
    }

    async fn closed(&self, report: CloseReport) {
        self.closed.lock().unwrap().push(report);
    }

    async fn sidecar_event(&self, _: SidecarEvent) {}

    async fn relay(&self, open: RelayOpen, stream: RelayStream) {
        self.relayed.lock().unwrap().push(open.id);
        let mut stream = tokio::io::BufReader::new(stream);
        let mut request_line = String::new();
        if stream.read_line(&mut request_line).await.is_err() {
            return;
        }
        loop {
            let mut header = String::new();
            match stream.read_line(&mut header).await {
                Ok(0) | Err(_) => return,
                Ok(_) if header == "\r\n" => break,
                Ok(_) => {}
            }
        }
        let mut stream = stream.into_inner();
        // `GET /bytes/<n>` answers n zero bytes; anything else names itself.
        let size = request_line
            .split_whitespace()
            .nth(1)
            .and_then(|path| path.strip_prefix("/bytes/"))
            .and_then(|count| count.parse::<usize>().ok());
        if let Some(size) = size {
            let head =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(head.as_bytes()).await;
            let chunk = vec![0u8; 64 * 1024];
            let mut left = size;
            while left > 0 {
                let take = left.min(chunk.len());
                if stream.write_all(&chunk[..take]).await.is_err() {
                    return;
                }
                left -= take;
            }
        } else {
            let body = format!("relayed {}\n", request_line.trim_end());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        let _ = stream.shutdown().await;
        // Wait for the client's end before dropping the stream.
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest).await;
    }
}

fn podman(args: &[&str]) -> std::process::Output {
    std::process::Command::new("podman")
        .args(args)
        .output()
        .expect("podman runs")
}

async fn podman_async(args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("podman")
        .args(args)
        .output()
        .await
        .expect("podman runs")
}

async fn podman_ok(args: &[&str]) -> String {
    let output = podman_async(args).await;
    assert!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `podman exec` as `user`: (exit code, stdout and stderr).
async fn exec(container: &str, user: &str, script: &str) -> (i32, String) {
    let output = podman_async(&["exec", "--user", user, container, "bash", "-c", script]).await;
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

struct Fixture {
    label: String,
    session: String,
    _directory: tempfile::TempDir,
    _program: SupervisorProgram,
    egress_image: String,
    authority: Arc<RelayingAuthority>,
    sidecar: Mutex<Option<EgressSidecar>>,
    containers: Mutex<Vec<String>>,
}

impl Fixture {
    async fn new() -> Self {
        let label = format!(
            "io.axocoatl.test={}",
            std::env::var("AXO_TEST_LABEL")
                .unwrap_or_else(|_| format!("egress-relay-{}", std::process::id()))
        );
        let directory = tempfile::Builder::new()
            .prefix("axo-relay-podman-")
            .tempdir()
            .unwrap();
        let root = SecureDir::open(directory.path().canonicalize().unwrap()).unwrap();
        let installation = root.child("supervisor-programs").unwrap();
        let arch = podman_ok(&["info", "--format", "{{.Host.Arch}}"]).await;
        let arch = match arch.trim() {
            "arm64" | "aarch64" => "aarch64",
            _ => "x86_64",
        };
        let program = SupervisorProgram::install_embedded(arch, &installation)
            .expect("the embedded supervisor matches this source; rebuild it first");
        let egress_image = axocoatl_isolation::egress_image::ensure_egress_image(&program)
            .await
            .unwrap();
        Self {
            label,
            session: format!("relay-test-{}", uuid::Uuid::new_v4().simple()),
            _directory: directory,
            _program: program,
            egress_image,
            authority: Arc::new(RelayingAuthority::default()),
            sidecar: Mutex::new(None),
            containers: Mutex::new(Vec::new()),
        }
    }

    async fn start_sidecar(&self) {
        let sidecar = EgressSidecar::start_with_options(
            SidecarSpec {
                session_id: self.session.clone(),
                runtime_authority: None,
                image: self.egress_image.clone(),
                network: None,
                max_connections: 64,
                require_resource_limits: false,
                labels: vec![self.label.clone()],
            },
            SidecarOptions {
                identity_socket: true,
            },
            self.authority.clone(),
            axocoatl_isolation::egress_control::ControlTiming::default(),
            RestartPolicy::default(),
        )
        .await
        .expect("the sidecar says hello");
        *self.sidecar.lock().unwrap() = Some(sidecar);
    }

    /// A client container whose PID 1 is the bundled bridge: identity socket
    /// on 3128 (with `--peer-identity`), ordinary socket on 3129.
    async fn client(&self, name: &str, ptrace: bool) -> String {
        let container = format!("axo-relay-{name}-{}", std::process::id());
        self.containers.lock().unwrap().push(container.clone());
        let _ = podman_async(&["rm", "--force", "--time", "0", "--ignore", &container]).await;
        let egress = format!(
            "type=volume,source=axo-egr-{},destination=/run/axocoatl-egress,ro=true",
            self.session
        );
        let identity = format!(
            "type=volume,source=axo-egi-{},destination=/run/axocoatl-egress-id,ro=true",
            self.session
        );
        let supervisor = format!(
            "type=image,source={},destination=/opt/axo",
            self.egress_image
        );
        let mut args = vec![
            "run",
            "-d",
            "--name",
            &container,
            "--label",
            &self.label,
            "--network",
            "none",
            "--user",
            "0:0",
        ];
        if ptrace {
            args.extend(["--cap-add", "SYS_PTRACE"]);
        }
        let client_image = image("AXO_RELAY_CLIENT_IMAGE", CLIENT_IMAGE);
        args.extend([
            "--mount",
            &egress,
            "--mount",
            &identity,
            "--mount",
            &supervisor,
            "--entrypoint",
            "/opt/axo/axocoatl-exec-supervisor",
            &client_image,
            "--bridge",
            "--tcp-to-unix",
            "127.0.0.1:3128=/run/axocoatl-egress-id/identity.sock",
            "--http-errors",
            "--peer-identity",
            "--tcp-to-unix",
            "127.0.0.1:3129=/run/axocoatl-egress/proxy.sock",
            "--http-errors",
        ]);
        podman_ok(&args).await;
        container
    }

    fn opened(&self) -> Vec<Opened> {
        self.authority.opened.lock().unwrap().clone()
    }

    async fn closed_for(&self, id: u64) -> CloseReport {
        for _ in 0..200 {
            if let Some(report) = self
                .authority
                .closed
                .lock()
                .unwrap()
                .iter()
                .find(|report| report.id == id)
            {
                return report.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no close for connection {id}");
    }

    async fn cleanup(&self) -> Result<(), String> {
        let sidecar = self.sidecar.lock().unwrap().take();
        if let Some(sidecar) = sidecar {
            sidecar.stop().await;
        }
        let labelled = podman(&["ps", "-aq", "--filter", &format!("label={}", self.label)]);
        let labelled = String::from_utf8_lossy(&labelled.stdout).into_owned();
        let labelled: Vec<&str> = labelled.split_whitespace().collect();
        if !labelled.is_empty() {
            let mut args = vec!["rm", "--force", "--time", "0"];
            args.extend(labelled);
            podman(&args);
        }
        let mut errors = Vec::new();
        for volume in [
            format!("axo-egr-{}", self.session),
            format!("axo-svc-{}", self.session),
            format!("axo-egi-{}", self.session),
        ] {
            let _ = podman(&["volume", "rm", "--force", &volume]);
            if podman(&["volume", "exists", &volume]).status.success() {
                errors.push(format!("volume {volume} is still present"));
            }
        }
        let left = podman(&["ps", "-aq", "--filter", &format!("label={}", self.label)]);
        if !String::from_utf8_lossy(&left.stdout).trim().is_empty() {
            errors.push("labelled containers are left".into());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

async fn with_fixture<F, Fut>(case: F)
where
    F: FnOnce(Arc<Fixture>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let fixture = Arc::new(Fixture::new().await);
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

/// A CONNECT through the real sidecar reaches the daemon's side of the
/// relay; the client reads the daemon's answer, and the close carries the
/// bytes both ways with no upstream address.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_relay_podman -- --ignored --test-threads=1"]
async fn a_relayed_connect_reaches_the_daemon_and_is_closed_with_its_bytes() {
    with_fixture(|fixture| async move {
        fixture.start_sidecar().await;
        let client = fixture.client("plain", false).await;
        let (code, output) = exec(
            &client,
            "0",
            "curl -sS --max-time 20 -p -x http://axo:axe_relaytoken@127.0.0.1:3129 http://relay.test/hello",
        )
        .await;
        assert_eq!(code, 0, "{output}");
        assert_eq!(output, "relayed GET /hello HTTP/1.1\n");
        let opened = fixture.opened();
        let relayed = opened
            .iter()
            .find(|open| open.host == "relay.test")
            .expect("the relay was decided");
        // The ordinary socket carries no identity.
        assert_eq!(relayed.peer, None);
        let close = fixture.closed_for(relayed.id).await;
        assert_eq!(close.ip, None, "{close:?}");
        assert!(close.up > 0 && close.down > 0, "{close:?}");
        assert_eq!(*fixture.authority.relayed.lock().unwrap(), vec![relayed.id]);
        // A refused host still gets the proxy's refusal.
        let (_, refused) = exec(
            &client,
            "0",
            "curl -sS --max-time 20 -p -x http://127.0.0.1:3129 http://other.test/ -o /dev/null -w '%{http_connect}'",
        )
        .await;
        assert!(refused.contains("403"), "{refused}");
    })
    .await;
}

/// Through the identity socket, the record names the program and user that
/// opened the connection, as the container's PID 1 found them; a process
/// cannot forge that line on either socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_relay_podman -- --ignored --test-threads=1"]
async fn the_identity_socket_names_the_program_and_refuses_forged_lines() {
    with_fixture(|fixture| async move {
        fixture.start_sidecar().await;
        let client = fixture.client("identity", true).await;
        let (code, output) = exec(
            &client,
            "1000:1000",
            "curl -sS --max-time 20 -p -x http://127.0.0.1:3128 http://relay.test/identity",
        )
        .await;
        assert_eq!(code, 0, "{output}");
        assert!(output.starts_with("relayed GET /identity"), "{output}");
        let opened = fixture.opened();
        let peer = opened
            .iter()
            .find(|open| open.host == "relay.test")
            .and_then(|open| open.peer.clone())
            .expect("an identity came with the connection");
        assert_eq!(peer.error, None, "{peer:?}");
        assert_eq!((peer.uid, peer.gid), (Some(1000), Some(1000)), "{peer:?}");
        let exe = peer.exe.clone().unwrap();
        assert!(exe.ends_with("/curl"), "{peer:?}");
        let (_, digest) = exec(&client, "0", &format!("sha256sum {exe}")).await;
        assert_eq!(
            Some(digest.split_whitespace().next().unwrap()),
            peer.exe_sha256.as_deref(),
            "{peer:?}"
        );
        // A program started through a shell names the shell as its parent.
        let (code, output) = exec(
            &client,
            "1000:1000",
            "bash -c 'curl -sS --max-time 20 -p -x http://127.0.0.1:3128 http://relay.test/child; true'",
        )
        .await;
        assert_eq!(code, 0, "{output}");
        let peer = fixture
            .opened()
            .into_iter()
            .rev()
            .find(|open| open.host == "relay.test")
            .and_then(|open| open.peer)
            .unwrap();
        assert!(peer.exe.as_deref().unwrap().ends_with("/curl"), "{peer:?}");
        assert!(
            peer.ancestors.first().is_some_and(|parent| parent.ends_with("/bash")),
            "{peer:?}"
        );

        // A program run from a private mount namespace, with a copy of curl
        // bound over /usr/bin/git, is not named git: the path and parents
        // are left out, and the SHA-256 is curl's.
        let (code, output) = exec(
            &client,
            "1000:1000",
            "cp /usr/bin/curl /tmp/not-git && unshare -Urm sh -c 'mount --bind /tmp/not-git /usr/bin/git && exec /usr/bin/git -sS --max-time 20 -p -x http://127.0.0.1:3128 http://relay.test/namespace'",
        )
        .await;
        assert_eq!(code, 0, "{output}");
        assert!(output.starts_with("relayed GET /namespace"), "{output}");
        let peer = fixture
            .opened()
            .into_iter()
            .rev()
            .find(|open| open.host == "relay.test")
            .and_then(|open| open.peer)
            .unwrap();
        assert_eq!(peer.error.as_deref(), Some("foreign_namespace"), "{peer:?}");
        assert_eq!(peer.exe, None, "{peer:?}");
        assert!(peer.ancestors.is_empty(), "{peer:?}");
        assert_eq!(peer.uid, Some(1000), "{peer:?}");
        let (_, digest) = exec(&client, "0", "sha256sum /usr/bin/curl").await;
        assert_eq!(
            Some(digest.split_whitespace().next().unwrap()),
            peer.exe_sha256.as_deref(),
            "{peer:?}"
        );

        // A forged line on the ordinary socket is refused before the
        // daemon hears of it.
        let before = fixture.opened().len();
        let forge = "exec 3<>/dev/tcp/127.0.0.1/{port}; printf 'AXO-PEER/1 {{\"exe\":\"/usr/bin/git\",\"uid\":0}}\\r\\nCONNECT relay.test:443 HTTP/1.1\\r\\n\\r\\n' >&3; timeout 10 cat <&3";
        let (_, refused) = exec(&client, "1000:1000", &forge.replace("{port}", "3129")).await;
        assert!(refused.starts_with("HTTP/1.1 400"), "{refused}");
        assert!(refused.contains("identity_not_accepted"), "{refused}");
        // On the identity socket the bridge's own line comes first, so a
        // second one is refused the same way.
        let (_, refused) = exec(&client, "1000:1000", &forge.replace("{port}", "3128")).await;
        assert!(refused.contains("identity_not_accepted"), "{refused}");
        assert_eq!(fixture.opened().len(), before);

        // Without CAP_SYS_PTRACE, PID 1 cannot look into another user's
        // process: the record keeps the socket's user and says why the
        // program is missing.
        let restricted = fixture.client("noptrace", false).await;
        let (code, output) = exec(
            &restricted,
            "1000:1000",
            "curl -sS --max-time 20 -p -x http://127.0.0.1:3128 http://relay.test/restricted",
        )
        .await;
        assert_eq!(code, 0, "{output}");
        let peer = fixture
            .opened()
            .into_iter()
            .rev()
            .find(|open| open.host == "relay.test")
            .and_then(|open| open.peer)
            .unwrap();
        assert_eq!(peer.uid, Some(1000), "{peer:?}");
        assert_eq!(peer.error.as_deref(), Some("no_access"), "{peer:?}");
        assert_eq!(peer.exe, None, "{peer:?}");
    })
    .await;
}

/// The embedded supervisor's `--serve --harden` in real images: ordinary
/// programs run (musl and glibc, Node and Python, threads and child
/// processes) while the filter refuses what it lists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_relay_podman -- --ignored --test-threads=1"]
async fn hardened_commands_run_in_real_images() {
    with_fixture(|fixture| async move {
        let client_image = image("AXO_RELAY_CLIENT_IMAGE", CLIENT_IMAGE);
        for (image, script, expect_success) in [
            (
                NODE_IMAGE.to_string(),
                "UV_USE_IO_URING=1 node -e \"const {execSync}=require('child_process'); execSync('true'); require('fs').readFileSync('/etc/hostname'); new (require('worker_threads').Worker)('1',{eval:true}).on('exit',c=>process.exit(c))\"",
                true,
            ),
            (ALPINE_GIT_IMAGE.to_string(), "git --version && sh -c true", true),
            (
                client_image.clone(),
                "git --version && python3 -c 'import threading, subprocess; t=threading.Thread(target=print); t.start(); t.join(); subprocess.run([\"true\"], check=True)'",
                true,
            ),
            (client_image.clone(), "unshare -U true", false),
            (NODE_IMAGE.to_string(), "unshare -U true", false),
        ] {
            let outcome = run_hardened(&fixture, &image, script).await;
            match (expect_success, &outcome) {
                (true, ProcessOutcome::Exited { code: 0 }) => {}
                (false, ProcessOutcome::Exited { code }) if *code != 0 => {}
                _ => panic!("{image}: {script}: {outcome:?}"),
            }
        }
    })
    .await;
}

/// Measurement, not a check: 50 MiB downloaded through a relay (bytes over
/// the sidecar's control channel, through `podman` on this computer)
/// against the same download through an ordinary tunnel to a container on
/// Podman's network. Prints the speeds; asserts only that every byte came.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; requires Podman: CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-isolation --test egress_relay_podman -- --ignored --test-threads=1"]
async fn relay_and_tunnel_throughput_measurement() {
    with_fixture(|fixture| async move {
        const SIZE: usize = 50 * 1024 * 1024;
        let upstream = format!("axo-relay-upstream-{}", std::process::id());
        fixture.containers.lock().unwrap().push(upstream.clone());
        let client_image = image("AXO_RELAY_CLIENT_IMAGE", CLIENT_IMAGE);
        let serve = format!(
            "head -c {SIZE} /dev/zero > /tmp/blob && cd /tmp && exec python3 -m http.server 8000"
        );
        podman_ok(&[
            "run",
            "-d",
            "--name",
            &upstream,
            "--label",
            &fixture.label,
            "--entrypoint",
            "sh",
            &client_image,
            "-c",
            &serve,
        ])
        .await;
        let address = podman_ok(&[
            "inspect",
            "--format",
            "{{.NetworkSettings.IPAddress}}",
            &upstream,
        ])
        .await;
        let address: std::net::IpAddr = address.trim().parse().unwrap();
        *fixture.authority.tunnel.lock().unwrap() = Some(address);
        fixture.start_sidecar().await;
        let client = fixture.client("speed", false).await;
        // The upstream needs a moment to write its file and listen.
        for _ in 0..50 {
            let (code, _) = exec(
                &client,
                "0",
                "curl -sS --max-time 5 -p -x http://127.0.0.1:3129 http://tunnel.test:8000/ -o /dev/null",
            )
            .await;
            if code == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let measure = |url: String| {
            let client = client.clone();
            async move {
                let (code, output) = exec(
                    &client,
                    "0",
                    &format!(
                        "curl -sS --max-time 300 -p -x http://127.0.0.1:3129 {url} -o /dev/null -w '%{{size_download}} %{{speed_download}} %{{time_total}}'"
                    ),
                )
                .await;
                assert_eq!(code, 0, "{output}");
                let fields: Vec<f64> = output
                    .split_whitespace()
                    .map(|field| field.parse().unwrap())
                    .collect();
                assert_eq!(fields[0] as usize, SIZE, "{output}");
                (fields[1] / (1024.0 * 1024.0), fields[2])
            }
        };
        let mut lines = Vec::new();
        for run in 1..=3 {
            let (relay, relay_time) = measure(format!("http://relay.test/bytes/{SIZE}")).await;
            let (tunnel, tunnel_time) =
                measure("http://tunnel.test:8000/blob".to_string()).await;
            lines.push(format!(
                "run {run}: relay {relay:.1} MiB/s ({relay_time:.2} s), tunnel {tunnel:.1} MiB/s ({tunnel_time:.2} s)"
            ));
        }
        for line in &lines {
            println!("throughput, 50 MiB download: {line}");
        }
    })
    .await;
}

/// Run `script` under the embedded supervisor's `--serve --harden` in a
/// container of `image`, speaking the execution protocol on its stdio.
async fn run_hardened(fixture: &Fixture, image: &str, script: &str) -> ProcessOutcome {
    let container = format!("axo-relay-harden-{}", uuid::Uuid::new_v4().simple());
    fixture.containers.lock().unwrap().push(container.clone());
    let supervisor = format!(
        "type=image,source={},destination=/opt/axo",
        fixture.egress_image
    );
    let mut child = tokio::process::Command::new("podman")
        .args([
            "run",
            "--rm",
            "-i",
            "--name",
            &container,
            "--label",
            &fixture.label,
            "--network",
            "none",
            "--mount",
            &supervisor,
            "--entrypoint",
            "/opt/axo/axocoatl-exec-supervisor",
            image,
            "--serve",
            "--harden",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let request = ExecRequest {
        protocol: axocoatl_exec::protocol::PROTOCOL_VERSION,
        invocation_id: container.clone(),
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        stdin: None,
        timeout_ms: 60_000,
        stdout_bytes: 8192,
        stderr_bytes: 8192,
        write_restriction: None,
    };
    let mut line = serde_json::to_vec(&request).unwrap();
    line.push(b'\n');
    stdin.write_all(&line).await.unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(60), stdout.read_line(&mut answer))
        .await
        .expect("ready in time")
        .unwrap();
    assert!(
        matches!(
            serde_json::from_str(&answer),
            Ok(ServerMessage::Ready { .. })
        ),
        "{answer}"
    );
    let mut dispatch = serde_json::to_vec(&Control::Dispatch).unwrap();
    dispatch.push(b'\n');
    stdin.write_all(&dispatch).await.unwrap();
    let mut finished = String::new();
    tokio::time::timeout(Duration::from_secs(90), stdout.read_line(&mut finished))
        .await
        .expect("finished in time")
        .unwrap();
    let message: ServerMessage = serde_json::from_str(&finished).unwrap();
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    match message {
        ServerMessage::Finished {
            outcome, stderr, ..
        } => {
            if !matches!(outcome, ProcessOutcome::Exited { code: 0 }) {
                eprintln!(
                    "{image}: {script}: {:?}",
                    String::from_utf8_lossy(&stderr.retained_bytes(8192).unwrap_or_default())
                );
            }
            outcome
        }
        other => panic!("{other:?}"),
    }
}
