//! Gap 2 against real containers: under `network: egress` the browser
//! reaches its declared hosts through the Session's own egress proxy and
//! decision point, under a `browser` policy kept apart from the Session's
//! list. Ignored by default; it needs Podman, the browser image, the
//! prepared tools image and the egress-capable embedded supervisor:
//!
//! ```text
//! CONTAINER_CONNECTION=<connection> \
//! AXO_SUPERVISOR_TEST_IMAGE=localhost/axocoatl-supervisor-test-root:20260914 \
//!     cargo test -p axocoatl-daemon --lib actual_browser_declared_hosts -- --ignored
//! ```
//!
//! Every Podman object it creates carries `io.axocoatl.test=gaps-policy-<pid>`
//! and is removed at the end.
use super::*;
use crate::session_dispatch_browser::{
    BrowserService, BrowserServiceConfig, SessionEgressSource, SessionPorts,
};
use crate::session_egress::{EgressPolicyConfig, SessionEgress, SessionRecordSink};
use crate::session_network::SessionNetworkRecords;
use axocoatl_session::network_record::{
    BindingKind, Decision as Recorded, EgressScope, NetworkEvent,
};

/// One upstream on its own Podman network, serving the same page on 8000
/// (declared for the browser) and 8001 (allowed for the Session).
struct Upstream {
    label: String,
    network: String,
    container: String,
    ip: String,
    subnet: String,
}

impl Upstream {
    fn podman(args: &[&str]) -> std::process::Output {
        std::process::Command::new("podman")
            .args(args)
            .output()
            .unwrap()
    }

    fn start() -> Self {
        let pid = std::process::id();
        let octet = pid % 250;
        let upstream = Self {
            label: format!("io.axocoatl.test=gaps-policy-{pid}"),
            network: format!("axo-gaps-policy-{pid}"),
            container: format!("axo-gaps-policy-up-{pid}"),
            ip: format!("10.93.{octet}.10"),
            subnet: format!("10.93.{octet}.0/24"),
        };
        let created = Self::podman(&[
            "network",
            "create",
            "--label",
            &upstream.label,
            "--subnet",
            &upstream.subnet,
            &upstream.network,
        ]);
        assert!(created.status.success(), "{created:?}");
        let started = Self::podman(&[
            "run", "-d", "--name", &upstream.container, "--label", &upstream.label,
            "--network", &upstream.network, "--ip", &upstream.ip,
            "docker.io/library/node:22-alpine", "node", "-e",
            "const h=require('http');for(const p of [8000,8001])h.createServer((q,r)=>{console.log('ACCESS '+p+' '+q.url);r.writeHead(200,{'content-type':'text/html'});r.end('<title>Declared '+p+'</title><h1>hello '+q.url+'</h1>\\n')}).listen(p)",
        ]);
        assert!(started.status.success(), "{started:?}");
        upstream
    }

    fn access_log(&self) -> String {
        String::from_utf8_lossy(&Self::podman(&["logs", &self.container]).stdout).into_owned()
    }

    fn cidr(&self, port: u16) -> axocoatl_config::EgressAllowYaml {
        axocoatl_config::EgressAllowYaml::Cidr(axocoatl_config::EgressCidrYaml {
            cidr: format!("{}/32", self.ip),
            ports: Some(vec![port]),
        })
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        let _ = Self::podman(&["rm", "--force", "--time", "0", "--ignore", &self.container]);
        let label = format!("label={}", self.label);
        let containers = Self::podman(&["ps", "-aq", "--filter", &label]);
        for container in String::from_utf8_lossy(&containers.stdout).split_whitespace() {
            let _ = Self::podman(&["rm", "--force", "--time", "0", "--ignore", container]);
        }
        let _ = Self::podman(&["network", "rm", "--force", &self.network]);
        let volumes = Self::podman(&["volume", "ls", "-q", "--filter", &label]);
        for volume in String::from_utf8_lossy(&volumes.stdout).split_whitespace() {
            let _ = Self::podman(&["volume", "rm", "--force", volume]);
        }
    }
}

struct NoPorts;

#[async_trait::async_trait]
impl SessionPorts for NoPorts {
    async fn exposed_ports(&self, _: &str) -> std::result::Result<Vec<u16>, String> {
        Ok(Vec::new())
    }
}

/// The Session's own decision point, and its sidecar's state from the real
/// sandbox, as the daemon gives them to the browser tools.
struct RunningSession {
    egress: Arc<SessionEgress>,
    sandbox: Arc<axocoatl_isolation::SessionSandbox>,
}

#[async_trait::async_trait]
impl SessionEgressSource for RunningSession {
    async fn session_egress(&self, _: &str) -> std::result::Result<Arc<SessionEgress>, String> {
        Ok(self.egress.clone())
    }
    async fn session_sidecar_ready(&self, _: &str) -> bool {
        self.sandbox.egress_status().is_some_and(|status| {
            status.phase == axocoatl_isolation::egress_sidecar::SidecarPhase::Ready
        })
    }
}

async fn egress_sandbox(
    f: &mut Fixture,
    upstream: &Upstream,
    authority: Arc<SessionEgress>,
) -> Arc<axocoatl_isolation::SessionSandbox> {
    use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
    use sha2::{Digest, Sha256};
    let image =
        std::env::var("AXO_SUPERVISOR_TEST_IMAGE").expect("set the explicit prepared tools image");
    let policy = SandboxPolicy {
        allow_untrusted_image: true,
        network: SandboxNetwork::Egress,
        runtime_authority: Some(format!(
            "{:x}",
            Sha256::digest(f.owner.metadata().session_id.as_bytes())
        )),
        supervisor_installation: Some(
            f.owner
                .inner
                .data_root
                .child("execution-supervisors")
                .unwrap(),
        ),
        egress: Some(axocoatl_isolation::egress::EgressAttachment {
            authority,
            sidecar_network: Some(upstream.network.clone()),
            max_connections: 32,
            labels: vec![upstream.label.clone()],
        }),
        ..SandboxPolicy::default()
    };
    let sandbox = Arc::new(
        SessionSandbox::start(
            &f.owner.metadata().session_id,
            f.owner.root(),
            Some(&image),
            &[],
            &[],
            &policy,
        )
        .await
        .expect("the egress Session starts"),
    );
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    {
        let inner = Arc::get_mut(&mut f.owner.inner).unwrap();
        inner.metadata.execution_identity = sandbox.execution_identity().unwrap().to_owned();
        inner.sandbox = registered.clone();
        inner
            .sandboxes
            .lock()
            .await
            .insert(inner.metadata.session_id.clone(), registered);
    }
    sandbox
}

fn browser_context(session_id: &str) -> crate::session_dispatch::HostInvocationContext {
    use axocoatl_session::turn_contract::{
        ActivationId, ActivationRef, ExecutionEpochId, InvocationId, LogicalTurnId, SessionId,
        TurnNodeId,
    };
    crate::session_dispatch::HostInvocationContext {
        session_id: session_id.to_string(),
        invocation_id: InvocationId::new("inv-browser-egress").unwrap(),
        activation: ActivationRef {
            session_id: SessionId::new(session_id).unwrap(),
            turn_id: LogicalTurnId::new("turn-1").unwrap(),
            execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
            node_id: TurnNodeId::new("scout").unwrap(),
            generation: 1,
            activation_id: ActivationId::new("act-browser-egress").unwrap(),
        },
        agent: "qa-scout".into(),
        read_only: true,
        checkout: None,
        attempt: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE, the browser image and the egress-capable embedded helper"]
async fn actual_browser_declared_hosts_use_the_egress_sessions_own_proxy() {
    let upstream = Upstream::start();
    let mut f = fixture().await;
    let session_id = f.owner.metadata().session_id.clone();
    let stores = crate::session_network::tests::Stores::new(&[session_id.as_str()]);
    let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
    // Port 8000 is declared for the browser only, 8001 allowed for the
    // Session only, exactly as `EgressPolicyConfig::from_config` builds it.
    let mut config = axocoatl_config::AxocoatlConfig {
        browser: Some(axocoatl_config::BrowserConfigYaml {
            allow: vec![upstream.cidr(8000)],
            private_destinations: vec![upstream.subnet.clone()],
            ..Default::default()
        }),
        ..Default::default()
    };
    config.sandbox.network = "egress".into();
    config.sandbox.egress = Some(axocoatl_config::EgressConfigYaml {
        allow: vec![upstream.cidr(8001)],
        private_destinations: vec![upstream.subnet.clone()],
        ..Default::default()
    });
    let egress = SessionEgress::open(
        session_id.clone(),
        EgressPolicyConfig::from_config(&config),
        Arc::new(SessionRecordSink::new(records.clone(), session_id.clone())),
        Arc::new(crate::session_egress::SystemResolver),
        Some(f.owner.inner.data_root.child("egress-env").unwrap()),
    )
    .await
    .unwrap();
    let sandbox = egress_sandbox(&mut f, &upstream, egress.clone()).await;
    git_init(f._workspace.path());

    // A writer's shell: the Session's host is reachable, the browser's is not.
    let r = run(&mut f, &["bash"], true);
    let command = format!(
        "wget -q -T 5 -O - http://{ip}:8001/from-agent 2>&1; echo \"agent-session=$?\"; \
         wget -q -T 5 -O - http://{ip}:8000/agent-to-browser-host 2>&1; echo \"agent-browser=$?\"",
        ip = upstream.ip
    );
    let provider = Provider::new(vec![("bash", serde_json::json!({ "command": command }))]);
    let writer = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;

    // The browser, through the Session's own proxy and decision point.
    let mut browser =
        BrowserServiceConfig::from_config(&config, format!("gaps-policy-{session_id}")).unwrap();
    browser.labels = vec![(
        "io.axocoatl.test".to_string(),
        upstream
            .label
            .trim_start_matches("io.axocoatl.test=")
            .to_string(),
    )];
    let data_root = f.owner.inner.data_root.clone();
    let service = Arc::new(BrowserService::new(
        browser,
        data_root.child("execution-supervisors").unwrap(),
        vec![data_root.path().to_path_buf()],
        records.clone(),
        Arc::new(NoPorts),
        Arc::new(RunningSession {
            egress: egress.clone(),
            sandbox: sandbox.clone(),
        }),
    ));
    let tool = crate::session_dispatch_browser::BrowserHostTool::browser(service.clone());
    let declared = tokio::time::timeout(
        Duration::from_secs(240),
        crate::session_dispatch::HostInvocationTool::bind(&tool, browser_context(&session_id))
            .execute(serde_json::json!({"url": format!("http://{}:8000/", upstream.ip)})),
    )
    .await;
    let session_only = tokio::time::timeout(
        Duration::from_secs(240),
        crate::session_dispatch::HostInvocationTool::bind(&tool, browser_context(&session_id))
            .execute(serde_json::json!({"url": format!("http://{}:8001/", upstream.ip)})),
    )
    .await;
    // Close frames follow the responses by a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let page = records.read_after(&session_id, None, 1000).await;
    let access = upstream.access_log();
    let ip = upstream.ip.clone();
    let policies = egress.policy_views();
    service.forget(&session_id).await;
    let stopped = sandbox.stop_checked().await;
    let removed =
        axocoatl_isolation::SessionSandbox::remove_named_with_dependencies(&session_id).await;
    records.close(&session_id).await;
    drop(upstream);
    stopped.unwrap();
    removed.unwrap();

    let writer = writer.expect("the writer finishes").unwrap();
    assert!(writer.accepted, "{:?}", writer.failure);
    assert!(provider.saw(1, "hello /from-agent"));
    assert!(provider.saw(1, "agent-session=0"));
    assert!(
        provider.saw(1, "403 Forbidden"),
        "the browser's host is refused to the writer"
    );
    assert!(!provider.saw(1, "agent-browser=0"));
    let declared = declared
        .expect("the browser call finishes")
        .expect("it succeeds");
    assert_eq!(declared["ok"], true, "{declared:#}");
    assert_eq!(declared["title"], "Declared 8000", "{declared:#}");
    if let Ok(result) = session_only.expect("the second browser call finishes") {
        assert_ne!(result["title"], "Declared 8001", "{result:#}");
    }
    assert!(access.contains("ACCESS 8001 /from-agent"), "{access}");
    assert!(access.contains("ACCESS 8000 /"), "{access}");
    assert!(!access.contains("agent-to-browser-host"), "{access}");
    assert!(
        !access.lines().any(|line| line.trim() == "ACCESS 8001 /"),
        "{access}"
    );
    let browser_revision = policies
        .iter()
        .find(|policy| policy.scope == "browser")
        .expect("the Session's decision point has the browser's policy")
        .revision;

    let events: Vec<NetworkEvent> = page
        .unwrap()
        .events
        .into_iter()
        .map(|line| line.event)
        .collect();
    let opened = |port: u16, kind: BindingKind, decision: Recorded| {
        events.iter().find_map(|event| match event {
            NetworkEvent::Open {
                port: p,
                decision: d,
                binding: Some(binding),
                scope,
                policy_revision,
                reason,
                host,
                ..
            } if *p == port && binding.kind == kind && *d == decision && *host == ip => {
                Some((*scope, *policy_revision, reason.clone(), binding.clone()))
            }
            _ => None,
        })
    };
    // The browser's own declared host: allowed, in the browser scope, under
    // the call's browser binding and the browser policy's revision.
    let (scope, revision, _, binding) = opened(8000, BindingKind::Browser, Recorded::Allow)
        .unwrap_or_else(|| panic!("{events:#?}"));
    assert_eq!(scope, Some(EgressScope::Browser));
    assert_eq!(revision, Some(browser_revision));
    assert_eq!(binding.invocation_id.as_deref(), Some("inv-browser-egress"));
    assert_eq!(binding.agent.as_deref(), Some("qa-scout"));
    // The Session's host is refused to the browser.
    let (scope, _, reason, _) =
        opened(8001, BindingKind::Browser, Recorded::Deny).unwrap_or_else(|| panic!("{events:#?}"));
    assert_eq!(scope, Some(EgressScope::Browser));
    assert_eq!(reason.as_deref(), Some("not_allowed"));
    // And the browser's host is refused to the writer.
    let (scope, _, reason, _) =
        opened(8000, BindingKind::Agent, Recorded::Deny).unwrap_or_else(|| panic!("{events:#?}"));
    assert_eq!(scope, Some(EgressScope::Session));
    assert_eq!(reason.as_deref(), Some("not_allowed"));
    assert!(opened(8001, BindingKind::Agent, Recorded::Allow).is_some());
    // The calls themselves are recorded with their credential's tag.
    assert!(events.iter().any(|event| matches!(
        event,
        NetworkEvent::Browser { ok: true, token: Some(_), invocation_id, .. } if invocation_id == "inv-browser-egress"
    )));
    // No second sidecar: only the Session's proxy recorded sidecar events.
    let sidecars: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Sidecar {
                generation, state, ..
            } if *state == axocoatl_session::network_record::SidecarState::Ready => {
                Some(*generation)
            }
            _ => None,
        })
        .collect();
    assert_eq!(sidecars, [1], "{events:#?}");
}
