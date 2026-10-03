//! A read-only helper drives the browser through the real native admission
//! path, the real browser container and the Session's real network record.
//! Ignored by default: it needs Podman, the browser image and the curated
//! `docker.io/library/node:20-slim`:
//!
//! ```text
//! CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-daemon --lib \
//!     actual_read_only_helper_drives_the_browser -- --ignored
//! ```
use super::*;
use crate::session_dispatch_browser::{
    BrowserHostTool, BrowserService, BrowserServiceConfig, SessionPorts,
};
use crate::session_network::SessionNetworkRecords;
use axocoatl_session::network_record::{BrowserTool as RecordedTool, NetworkEvent};

struct FixedPorts(Vec<u16>);

#[async_trait::async_trait]
impl SessionPorts for FixedPorts {
    async fn exposed_ports(&self, _: &str) -> std::result::Result<Vec<u16>, String> {
        Ok(self.0.clone())
    }
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
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

async fn browser_sandbox(f: &mut Fixture) -> Arc<axocoatl_isolation::SessionSandbox> {
    use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
    use sha2::{Digest, Sha256};
    let policy = SandboxPolicy {
        network: SandboxNetwork::Bridge,
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
        ..SandboxPolicy::default()
    };
    let sandbox = Arc::new(
        SessionSandbox::start(
            &f.owner.metadata().session_id,
            f.owner.root(),
            Some("docker.io/library/node:20-slim"),
            &[8765],
            &[],
            &policy,
        )
        .await
        .unwrap(),
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

#[tokio::test]
#[ignore = "requires Podman, the browser image and docker.io/library/node:20-slim"]
async fn actual_read_only_helper_drives_the_browser_and_the_call_is_recorded() {
    let mut f = fixture().await;
    copy_tree(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../demo/one-app/workspace-template"),
        f._workspace.path(),
    );
    let sandbox = browser_sandbox(&mut f).await;
    let session_id = f.owner.metadata().session_id.clone();
    let server = f.owner.root().join("demo/server.mjs");
    let started = tokio::process::Command::new("podman")
        .args([
            "exec",
            "-d",
            &format!("axo-ses-{session_id}"),
            "node",
            server.to_str().unwrap(),
        ])
        .status()
        .await
        .unwrap();
    assert!(started.success());
    let data_root = f.owner.inner.data_root.clone();

    // A read-only helper that lists `browser`.
    let r = run_scoped(&mut f, &["read_file", "browser"], &[]);
    let Run {
        registry,
        controller,
        activation,
        resource,
        config,
        profile,
    } = r;
    let registry = Arc::new(registry);
    let records = Arc::new(SessionNetworkRecords::new(
        Arc::new(crate::bootstrap::RegistryNetworkRecords(registry.clone())),
        50_000,
    ));
    let browser = axocoatl_config::AxocoatlConfig {
        browser: Some(axocoatl_config::BrowserConfigYaml::default()),
        ..Default::default()
    };
    let service = Arc::new(BrowserService::new(
        BrowserServiceConfig::from_config(&browser, format!("browser-test-{session_id}")).unwrap(),
        data_root.child("execution-supervisors").unwrap(),
        vec![data_root.path().to_path_buf()],
        records.clone(),
        Arc::new(FixedPorts(vec![8765])),
    ));
    controller
        .register_host_invocation_tool(Arc::new(BrowserHostTool::browser(service.clone())))
        .unwrap();
    let provider = Provider::new(vec![(
        "browser",
        serde_json::json!({
            "url": "http://localhost:8765/",
            "steps": [{"action": "wait_for", "text": "ORD-2051"}],
        }),
    )]);
    let resources = AutonomousActivationResources {
        config,
        profile,
        provider: provider.clone(),
        counter: Arc::new(Counter),
        tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
    };
    let result = tokio::time::timeout(Duration::from_secs(240), async {
        controller
            .prepare_repository_activation(activation.clone(), resources, resource.clone())
            .unwrap()
            .run()
            .await
    })
    .await;
    let page = records.read_after(&session_id, None, 100).await;
    let screenshot = match page.as_ref().ok().and_then(|page| page.events.last()) {
        Some(line) => match &line.event {
            NetworkEvent::Browser {
                screenshot: Some(shot),
                ..
            } => records
                .read_screenshot(&session_id, &shot.sha256)
                .await
                .ok()
                .flatten(),
            _ => None,
        },
        None => None,
    };
    service.forget(&session_id).await;
    records.close(&session_id).await;
    let stop = sandbox.stop_checked().await;
    let browser_left = tokio::process::Command::new("podman")
        .args(["container", "exists", &format!("axo-brw-{session_id}")])
        .status()
        .await
        .unwrap();
    let _ = axocoatl_isolation::SessionSandbox::remove_named_with_dependencies(&session_id).await;

    stop.unwrap();
    assert!(
        !browser_left.success(),
        "the browser container goes with the Session"
    );
    let result = result.unwrap().unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert!(provider.offered.lock().unwrap()[0].contains(&"browser".to_string()));
    assert!(
        provider.saw(1, "ORD-2051"),
        "the snapshot reached the helper"
    );
    let page = page.unwrap();
    let event = page
        .events
        .last()
        .expect("the call is in the network record");
    let NetworkEvent::Browser {
        tool,
        activation_id,
        agent,
        ok,
        url,
        status,
        screenshot: shot,
        invocation_id,
        ..
    } = &event.event
    else {
        panic!("{:?}", event.event);
    };
    assert_eq!(*tool, RecordedTool::Browser);
    assert_eq!(activation_id, activation.activation_id.as_str());
    assert_eq!(agent, "repository-definition");
    assert!(*ok);
    assert_eq!(url.as_deref(), Some("http://localhost:8765/"));
    assert_eq!(*status, Some(200));
    let (media, bytes) = screenshot.expect("the screenshot is kept in the record");
    assert_eq!(media, "image/jpeg");
    assert_eq!(bytes.len() as u64, shot.as_ref().unwrap().bytes);
    // The helper's result is settled like any other tool call.
    let snapshot = controller.snapshot().unwrap();
    assert!(snapshot
        .contract()
        .invocations()
        .iter()
        .any(|invocation| invocation.invocation_id.as_str() == invocation_id));
    assert!(
        !provider.saw(1, "base64"),
        "no screenshot reaches the model"
    );
}
