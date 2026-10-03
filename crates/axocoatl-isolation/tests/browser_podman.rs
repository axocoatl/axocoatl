//! Live checks of the browser container against real rootless Podman.
//!
//! Ignored by default. They need the browser image built by
//! `axocoatl browser install` (or `AXO_BROWSER_TEST_IMAGE`) and, for the
//! Session, the curated `docker.io/library/node:20-slim`:
//!
//! ```text
//! CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-isolation \
//!     --test browser_podman -- --ignored --test-threads=1
//! ```
//!
//! Everything a test creates carries `io.axocoatl.test=browser-<pid>` and is
//! removed by name or by that label at the end.
#![cfg(unix)]

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::protocol::{
    ExecRequest, ProcessOutcome, ServerMessage, WriteRestriction, PROTOCOL_VERSION,
};
use axocoatl_isolation::browser_container::{
    browser_container_name, ensure_service_sockets, service_forwarder_name, BrowserContainer,
    BrowserLaunch, ServiceSocketsLaunch,
};
use axocoatl_isolation::egress::{
    CloseReport, Decision, EgressAuthority, EgressGrant, GrantSpec, OpenRequest, SidecarEvent,
};
use axocoatl_isolation::egress_control::ControlTiming;
use axocoatl_isolation::egress_sidecar::{EgressSidecar, SidecarSpec};
use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const DRIVER: &str = include_str!("../../axocoatl-tools/assets/browser/driver.mjs");
const CHECK: &str = include_str!("../../axocoatl-tools/assets/browser/check.mjs");
const SESSION_IMAGE: &str = "docker.io/library/node:20-slim";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

fn browser_image() -> String {
    std::env::var("AXO_BROWSER_TEST_IMAGE")
        .unwrap_or_else(|_| "localhost/axocoatl-browser:pw1.60.0".to_string())
}

fn test_label() -> (String, String) {
    (
        "io.axocoatl.test".to_string(),
        format!("browser-{}", std::process::id()),
    )
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: PathBuf,
    installation: SecureDir,
    session_id: String,
    authority: String,
    sandbox: Mutex<Option<Arc<SessionSandbox>>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("axo-browser-podman-")
            .tempdir()
            .unwrap();
        let root = SecureDir::open(directory.path().canonicalize().unwrap()).unwrap();
        let workspace = root.child("workspace").unwrap().path().to_owned();
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/one-app/workspace-template"),
            &workspace,
        );
        std::fs::create_dir_all(workspace.join("qa")).unwrap();
        let identity = uuid::Uuid::new_v4().simple().to_string();
        Self {
            installation: root.child("supervisor-programs").unwrap(),
            _directory: directory,
            workspace,
            session_id: format!("brw-test-{}", &identity[..12]),
            authority: format!("{:x}", Sha256::digest(identity.as_bytes())),
            sandbox: Mutex::new(None),
        }
    }

    async fn start_session(&self, network: SandboxNetwork, ports: &[u16]) -> Arc<SessionSandbox> {
        let policy = SandboxPolicy {
            network,
            runtime_authority: Some(self.authority.clone()),
            supervisor_installation: Some(self.installation.clone()),
            ..SandboxPolicy::default()
        };
        let sandbox = Arc::new(
            SessionSandbox::start(
                &self.session_id,
                &self.workspace,
                Some(SESSION_IMAGE),
                ports,
                &[],
                &policy,
            )
            .await
            .expect("the Session container starts"),
        );
        *self.sandbox.lock().unwrap() = Some(sandbox.clone());
        sandbox
    }

    fn launch(&self, ports: &[u16], egress: bool) -> BrowserLaunch {
        BrowserLaunch {
            session_id: self.session_id.clone(),
            runtime_authority: self.authority.clone(),
            image: browser_image(),
            exposed_ports: ports.to_vec(),
            egress,
            supervisor_installation: self.installation.clone(),
            require_resource_limits: false,
            labels: vec![test_label()],
        }
    }

    /// Serve the Session's ports as sockets through its service forwarder;
    /// returns the forwarder's container id.
    async fn service_sockets(&self, ports: &[u16]) -> String {
        let image =
            axocoatl_isolation::egress_image::ensure_egress_image_for_podman(&self.installation)
                .await
                .expect("the egress image builds");
        ensure_service_sockets(&ServiceSocketsLaunch {
            session_id: self.session_id.clone(),
            runtime_authority: self.authority.clone(),
            image,
            ports: ports.to_vec(),
            require_resource_limits: false,
            labels: vec![test_label()],
        })
        .await
        .expect("the service forwarder serves the ports")
        .expect("a forwarder for exposed ports")
    }

    async fn start_demo_app(&self) {
        let server = self.workspace.join("demo/server.mjs");
        podman_ok(&[
            "exec",
            "-d",
            &format!("axo-ses-{}", self.session_id),
            "node",
            server.to_str().unwrap(),
        ])
        .await;
        for _ in 0..50 {
            let probe = podman(&[
                "exec",
                &format!("axo-ses-{}", self.session_id),
                "node",
                "-e",
                "fetch('http://127.0.0.1:8765/api/orders').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))",
            ])
            .await;
            if probe.status.success() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("the demo app did not start");
    }

    async fn cleanup(&self) {
        let sandbox = self.sandbox.lock().unwrap().take();
        if let Some(sandbox) = sandbox {
            let _ = sandbox.stop_checked().await;
        }
        SessionSandbox::remove_named_with_dependencies(&self.session_id)
            .await
            .expect("the test Session and its browser are removed");
    }
}

/// Run `body`, then `cleanup` even when `body` panics, then re-raise.
async fn with_cleanup<F, C>(body: F, cleanup: C)
where
    F: std::future::Future<Output = ()> + Send + 'static,
    C: std::future::Future<Output = ()>,
{
    let outcome = tokio::spawn(body).await;
    cleanup.await;
    if let Err(error) = outcome {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("the test body did not finish: {error}");
    }
}

async fn drive(container: &BrowserContainer, input: Value, invocation: &str) -> Value {
    let output = container
        .run_script(
            invocation,
            DRIVER,
            serde_json::to_vec(&input).unwrap(),
            RUN_TIMEOUT,
        )
        .await
        .expect("the driver runs");
    assert_eq!(
        output.exit_code,
        Some(0),
        "driver stderr: {}",
        output.stderr
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "driver output is JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn driver_input(url: &str, steps: Value, proxy: Option<&str>) -> Value {
    json!({
        "schema": "axocoatl.browser-input/1",
        "url": url,
        "steps": steps,
        "snapshot": "aria",
        "proxy": proxy.map(|password| json!({"server": "http://127.0.0.1:3128", "username": "axo", "password": password})),
        "aut_origins": ["http://localhost:8765"],
        "screenshot": true,
        "limits": {"snapshot_max_bytes": 16384, "step_timeout_ms": 10000, "total_timeout_ms": 90000,
                   "console_max": 50, "network_max": 50, "output_max_bytes": 65536, "screenshot_max_bytes": 1048576},
    })
}

/// Spec C.6 Podman cases 1, 2, 3 (empty allow), 5 and 6, plus the check runner.
#[tokio::test]
#[ignore = "requires Podman and the browser image: CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-isolation --test browser_podman -- --ignored --test-threads=1"]
async fn browser_reaches_exposed_ports_and_nothing_else() {
    let fixture = Arc::new(Fixture::new());
    let body = fixture.clone();
    with_cleanup(async move {
        let fixture = body;
        let sandbox = fixture.start_session(SandboxNetwork::Bridge, &[8765]).await;
        fixture.start_demo_app().await;
        let forwarder = fixture.service_sockets(&[8765]).await;
        // A second call finds the forwarder alive and starts nothing.
        assert_eq!(fixture.service_sockets(&[8765]).await, forwarder);
        let forwarder_name = service_forwarder_name(&fixture.session_id);
        let session_container = format!("axo-ses-{}", fixture.session_id);
        let session_id = podman_ok(&["container", "inspect", "--format", "{{.Id}}", &session_container]).await;
        let inspect = podman_ok(&[
            "container", "inspect", "--format",
            "{{.HostConfig.NetworkMode}} {{.HostConfig.ReadonlyRootfs}} {{json .Mounts}}",
            &forwarder_name,
        ])
        .await;
        assert!(inspect.starts_with(&format!("container:{session_id} true")), "{inspect}");
        assert!(!inspect.contains(fixture.workspace.to_str().unwrap()), "{inspect}");
        let container = BrowserContainer::start(&fixture.launch(&[8765], false))
            .await
            .expect("the browser container starts");
        let name = browser_container_name(&fixture.session_id);

        // 1. The app under test, at the same URL the Agent uses.
        let out = drive(
            &container,
            driver_input(
                "http://localhost:8765/",
                json!([{"action": "wait_for", "text": "ORD-2051"}]),
                None,
            ),
            "inv-aut",
        )
        .await;
        assert_eq!(out["ok"], true, "{out:#}");
        assert_eq!(out["status"], 200);
        assert!(out["title"].as_str().is_some_and(|title| !title.is_empty()), "{out:#}");
        assert!(out["snapshot"]["text"].as_str().unwrap().contains("ORD-2051"), "{out:#}");
        assert_eq!(out["steps"][0]["code"], "await page.getByText('ORD-2051').first().waitFor();");
        assert_eq!(out["screenshot"]["type"], "jpeg");
        assert!(out["screenshot"]["base64"].as_str().unwrap().len() > 100);

        // 2. A port the Session does not expose refuses in the kernel.
        let out = drive(
            &container,
            driver_input("http://localhost:9999/", json!([]), None),
            "inv-closed",
        )
        .await;
        assert_eq!(out["ok"], false);
        let error = out["navigation"]["error"].as_str().unwrap();
        assert!(error.contains("ERR_CONNECTION_REFUSED"), "{error}");

        // 3. With no declared hosts there is no proxy and no route: nothing
        // else is reachable.
        let out = drive(
            &container,
            driver_input("http://example.com/", json!([]), None),
            "inv-outside",
        )
        .await;
        assert_eq!(out["ok"], false);
        let error = out["navigation"]["error"].as_str().unwrap();
        // Chromium sees no network at all; nothing reaches a proxy.
        assert!(
            error.contains("ERR_INTERNET_DISCONNECTED") || error.contains("ERR_NAME_NOT_RESOLVED"),
            "{error}"
        );
        assert!(out["network"]["blocked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["reason"] == "not_allowed"), "{out:#}");
        let out = drive(
            &container,
            driver_input("http://10.89.0.1:8000/", json!([]), None),
            "inv-outside-ip",
        )
        .await;
        assert_eq!(out["ok"], false, "{out:#}");
        assert!(!out["navigation"]["error"].as_str().unwrap().contains("ERR_PROXY"), "{out:#}");

        // A read-only helper's shell (writable /tmp, /var/tmp and /dev, no
        // TCP) reaches the app neither by TCP nor through any service
        // socket: the Session container never mounts them.
        let probe = "const net=require('net'),fs=require('fs');\
            const out={};let n=0;const done=()=>{if(++n===2)console.log(JSON.stringify(out))};\
            try{out.dir=fs.readdirSync('/run/axocoatl-svc')}catch(e){out.dir=e.code}\
            net.connect(8765,'127.0.0.1').on('connect',function(){out.tcp='connected';this.destroy();done()}).on('error',e=>{out.tcp=e.code;done()});\
            net.connect('/run/axocoatl-svc/8765.sock').on('connect',function(){out.sock='connected';this.destroy();done()}).on('error',e=>{out.sock=e.code;done()});";
        let root = sandbox.root().to_string_lossy().into_owned();
        let run_probe = |restricted: bool| {
            let sandbox = sandbox.clone();
            let root = root.clone();
            async move {
                let request = ExecRequest {
                    protocol: PROTOCOL_VERSION,
                    stdin: None,
                    invocation_id: format!("inv-helper-probe-{restricted}"),
                    argv: vec!["node".into(), "-e".into(), probe.into()],
                    timeout_ms: 30_000,
                    stdout_bytes: 4096,
                    stderr_bytes: 4096,
                    write_restriction: restricted.then(|| WriteRestriction {
                        writable: vec!["/tmp".into(), "/var/tmp".into(), "/dev".into()],
                        protected: vec![root],
                        deny_network: true,
                    }),
                };
                let prepared = sandbox.prepare_supervised_command(request).await.unwrap();
                let execution = prepared.dispatch().unwrap().finish().await.unwrap();
                let ServerMessage::Finished { outcome, stdout, stderr, .. } = execution.result() else {
                    panic!("the probe has no terminal result");
                };
                assert_eq!(*outcome, ProcessOutcome::Exited { code: 0 }, "{:?}", stderr.retained_bytes(4096));
                let text = String::from_utf8(stdout.retained_bytes(4096).unwrap()).unwrap();
                serde_json::from_str::<Value>(text.trim()).unwrap()
            }
        };
        let open = run_probe(false).await;
        assert_eq!(open["tcp"], "connected", "{open}");
        let helper = run_probe(true).await;
        assert_eq!(helper["tcp"], "EACCES", "{helper}");
        assert_eq!(helper["dir"], "ENOENT", "{helper}");
        assert_eq!(helper["sock"], "ENOENT", "{helper}");
        assert_eq!(open["dir"], "ENOENT", "{open}");

        // 5. No Workspace inside, the bridge is PID 1, isolation flags hold.
        let missing = podman(&["exec", &name, "ls", fixture.workspace.to_str().unwrap()]).await;
        assert!(!missing.status.success(), "the Workspace must not be visible");
        let cmdline = podman_ok(&["exec", &name, "cat", "/proc/1/cmdline"]).await;
        assert!(cmdline.starts_with("/axocoatl-exec-supervisor\0--bridge"), "{cmdline:?}");
        let inspect = podman_ok(&[
            "container",
            "inspect",
            "--format",
            "{{.HostConfig.NetworkMode}} {{.HostConfig.ReadonlyRootfs}} {{json .HostConfig.CapDrop}} {{json .Config.Env}}",
            &name,
        ])
        .await;
        assert!(inspect.starts_with("none true"), "{inspect}");
        assert!(!inspect.contains("axe_"), "{inspect}");
        let interfaces = podman_ok(&["exec", &name, "cat", "/proc/net/dev"]).await;
        let names: Vec<&str> = interfaces
            .lines()
            .skip(2)
            .filter_map(|line| line.split(':').next())
            .map(str::trim)
            .collect();
        assert_eq!(names, vec!["lo"], "{interfaces}");

        // The check runner: one passing and one failing reproduction.
        std::fs::write(
            fixture.workspace.join("qa/orders.spec.ts"),
            "import { test, expect } from '@playwright/test';\n\
             test('lists ORD-2051', async ({ page }) => {\n\
               await page.goto('/');\n\
               await expect(page.getByText('ORD-2051')).toBeVisible();\n\
             });\n\
             test('no order is payable below zero', async ({ page }) => {\n\
               await page.goto('/');\n\
               await expect(page.locator('body')).not.toContainText('-$', { timeout: 2000 });\n\
             });\n",
        )
        .unwrap();
        let spec = std::fs::read_to_string(fixture.workspace.join("qa/orders.spec.ts")).unwrap();
        let input = json!({
            "schema": "axocoatl.browser-check-input/1",
            "entry": "qa/orders.spec.ts",
            "files": [{"path": "qa/orders.spec.ts", "content": spec}],
            "base_url": "http://localhost:8765",
            "proxy": null,
            "limits": {"total_timeout_ms": 90000, "test_timeout_ms": 20000, "output_max_bytes": 65536,
                       "log_max_bytes": 4096, "screenshot_max_bytes": 1048576},
        });
        let output = container
            .run_script("inv-check", CHECK, serde_json::to_vec(&input).unwrap(), RUN_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0), "{}", output.stderr);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["counts"]["passed"], 1, "{report:#}");
        assert_eq!(report["counts"]["failed"], 1, "{report:#}");
        assert_eq!(report["status"], "failed");
        let failing = report["tests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|test| test["status"] == "failed")
            .unwrap();
        assert_eq!(failing["error"]["location"]["file"], "qa/orders.spec.ts");
        assert_eq!(report["screenshot"]["type"], "png");
        // Nothing of the run is left in the browser container.
        let leftovers = podman_ok(&["exec", &name, "sh", "-c", "ls -A /tmp | grep axo-check || true"]).await;
        assert!(leftovers.is_empty(), "{leftovers}");

        // A restarted Session container has a new network namespace: the
        // forwarder joined to the old one is replaced.
        podman_ok(&["restart", "--time", "0", &session_container]).await;
        fixture.start_demo_app().await;
        let replaced = fixture.service_sockets(&[8765]).await;
        assert_ne!(replaced, forwarder);
        let container = BrowserContainer::start(&fixture.launch(&[8765], false))
            .await
            .expect("the browser container starts again");
        let out = drive(
            &container,
            driver_input("http://localhost:8765/", json!([{"action": "wait_for", "text": "ORD-2051"}]), None),
            "inv-aut-again",
        )
        .await;
        assert_eq!(out["ok"], true, "{out:#}");

        // 6. The browser container and the forwarder go with the Session.
        sandbox.stop_checked().await.unwrap();
        let exists = podman(&["container", "exists", &name]).await;
        assert!(!exists.status.success(), "the browser container must be removed with the Session");
        let exists = podman(&["container", "exists", &forwarder_name]).await;
        assert!(!exists.status.success(), "the forwarder must be removed with the Session");
    }, async { fixture.cleanup().await })
    .await;
}

/// A fake decision point: allows one address and port, refuses the rest,
/// and keeps every request and close for assertions.
#[derive(Debug, Default)]
struct FakeAuthority {
    allowed: Mutex<Option<(IpAddr, u16)>>,
    token_hash: Mutex<Option<String>>,
    opens: Mutex<Vec<OpenRequest>>,
    closes: Mutex<Vec<CloseReport>>,
    events: Mutex<Vec<SidecarEvent>>,
}

#[async_trait::async_trait]
impl EgressAuthority for FakeAuthority {
    async fn grant(&self, _spec: GrantSpec) -> Result<EgressGrant, String> {
        unreachable!("the test sets the credential directly")
    }

    async fn decide(&self, open: OpenRequest) -> Decision {
        self.opens.lock().unwrap().push(open.clone());
        let expected = self.token_hash.lock().unwrap().clone();
        if open.auth.is_none() || open.auth != expected {
            return Decision::deny(407, "no_credential", "no credential");
        }
        let allowed = *self.allowed.lock().unwrap();
        match (allowed, open.host.parse::<IpAddr>()) {
            (Some((ip, port)), Ok(host)) if host == ip && open.port == port => {
                Decision::Allow { addrs: vec![ip] }
            }
            _ => Decision::deny(403, "not_allowed", "not in the browser allowlist"),
        }
    }

    async fn closed(&self, report: CloseReport) {
        self.closes.lock().unwrap().push(report);
    }

    async fn sidecar_event(&self, event: SidecarEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Spec C.6 Podman case 3 (declared hosts): an allowed private upstream is
/// reached only through the egress sidecar, with the call's credential.
#[tokio::test]
#[ignore = "requires Podman and the browser image: CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-isolation --test browser_podman -- --ignored --test-threads=1"]
async fn declared_hosts_go_through_the_egress_proxy() {
    let fixture = Arc::new(Fixture::new());
    let pid = std::process::id();
    let network = format!("axo-browser-test-{pid}");
    let octet = 100 + (pid % 100);
    let subnet = format!("10.89.{octet}.0/24");
    let upstream_ip = format!("10.89.{octet}.10");
    let upstream = format!("axo-browser-upstream-{pid}");
    let (label_key, label_value) = test_label();
    let label = format!("{label_key}={label_value}");
    let body = fixture.clone();
    let (body_network, body_upstream, body_upstream_ip) =
        (network.clone(), upstream.clone(), upstream_ip.clone());
    with_cleanup(async move {
        let (fixture, network, upstream, upstream_ip) =
            (body, body_network, body_upstream, body_upstream_ip);
        podman_ok(&["network", "create", "--subnet", &subnet, "--label", &label, &network]).await;
        podman_ok(&[
            "run", "-d", "--name", &upstream, "--label", &label, "--network", &network,
            "--ip", &upstream_ip, SESSION_IMAGE, "node", "-e",
            "require('http').createServer((q,s)=>{s.writeHead(200,{'content-type':'text/html'});s.end('<title>Upstream</title><h1>declared host</h1>')}).listen(8000,'0.0.0.0')",
        ])
        .await;
        // The Session's own network mode does not change the browser's: its
        // container has only loopback either way.
        fixture.start_session(SandboxNetwork::Bridge, &[8765]).await;
        fixture.service_sockets(&[8765]).await;

        let authority = Arc::new(FakeAuthority::default());
        *authority.allowed.lock().unwrap() = Some((upstream_ip.parse().unwrap(), 8000));
        let token = "axe_browser-live-test-credential";
        *authority.token_hash.lock().unwrap() =
            Some(format!("{:x}", Sha256::digest(token.as_bytes())));
        let image =
            axocoatl_isolation::egress_image::ensure_egress_image_for_podman(&fixture.installation)
                .await
                .expect("the egress image builds");
        let sidecar = EgressSidecar::start(
            SidecarSpec {
                session_id: fixture.session_id.clone(),
                runtime_authority: Some(fixture.authority.clone()),
                image,
                network: Some(network.clone()),
                max_connections: 32,
                require_resource_limits: false,
                labels: vec![label.clone()],
            },
            authority.clone(),
            ControlTiming::default(),
        )
        .await
        .expect("the sidecar says hello");
        let container = BrowserContainer::start(&fixture.launch(&[8765], true))
            .await
            .expect("the browser container starts with the proxy listener");

        let url = format!("http://{upstream_ip}:8000/");
        let out = drive(&container, driver_input(&url, json!([]), Some(token)), "inv-declared").await;
        assert_eq!(out["ok"], true, "{out:#}");
        assert_eq!(out["status"], 200);
        assert_eq!(out["title"], "Upstream");
        assert!(!out.to_string().contains(token), "the credential never comes back");
        let allowed: Vec<OpenRequest> = authority
            .opens
            .lock()
            .unwrap()
            .iter()
            .filter(|open| open.host == upstream_ip)
            .cloned()
            .collect();
        // Chromium sends its proxy credential only after a 407 challenge, so
        // a call's first request arrives without one and is refused.
        assert!(allowed.len() >= 2, "{allowed:?}");
        assert!(allowed.iter().all(|open| open.port == 8000));
        assert_eq!(allowed[0].auth, None);
        let expected = authority.token_hash.lock().unwrap().clone();
        assert!(allowed.iter().skip(1).all(|open| open.auth == expected), "{allowed:?}");

        // A host that is not declared is refused by the proxy.
        let out = drive(
            &container,
            driver_input("http://10.89.0.1:8000/", json!([]), Some(token)),
            "inv-undeclared",
        )
        .await;
        assert_eq!(out["ok"], false, "{out:#}");
        assert_eq!(out["status"], 403, "{out:#}");
        assert!(out["navigation"]["error"].as_str().unwrap().contains("not_allowed"), "{out:#}");
        let blocked = out["network"]["blocked"].as_array().unwrap();
        assert!(blocked.iter().any(|entry| entry["reason"] == "not_allowed"), "{out:#}");
        // Without the credential the proxy refuses with 407.
        let out = drive(&container, driver_input(&url, json!([]), None), "inv-tokenless").await;
        assert_eq!(out["ok"], false, "{out:#}");
        assert_ne!(out["status"], 200, "{out:#}");
        // Closes are reported with byte counts after the connection ends.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(authority
            .closes
            .lock()
            .unwrap()
            .iter()
            .any(|close| close.down > 0), "{:?}", authority.closes.lock().unwrap());
        sidecar.stop().await;
    }, async {
        let _ = podman(&["rm", "--force", "--time", "0", "--ignore", &upstream]).await;
        fixture.cleanup().await;
        let _ = podman(&["network", "rm", "--force", &network]).await;
    })
    .await;
}
