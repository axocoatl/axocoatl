//! The `browser` and `browser_check` tools for native Sessions.
//!
//! [`BrowserService`] is daemon-wide. For each Session it keeps the browser
//! container, a semaphore of `browser.max_parallel` permits and, when
//! `browser.allow` lists hosts, a browser-scope egress decision point and
//! its sidecar. Each call:
//!
//! 1. makes sure the Session container serves its exposed ports as sockets;
//! 2. starts the egress sidecar when declared hosts exist;
//! 3. starts or reuses the browser container;
//! 4. takes a browser egress credential bound to the call (declared hosts
//!    only), which reaches the driver only on its stdin;
//! 5. runs the driver or the check runner under the supervisor;
//! 6. drops the credential, which closes its connections, and keeps any
//!    screenshot and a `browser` event in the Session's network record.
//!
//! The browser never sees the repository. `browser_check` sends the one test
//! file it names and the source files that file imports, nothing else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axocoatl_config::EgressAllowYaml;
use axocoatl_core::SecureDir;
use axocoatl_isolation::browser_container::{
    ensure_service_sockets, BrowserContainer, BrowserLaunch,
};
use axocoatl_isolation::egress::{EgressAuthority, GrantKind, GrantSpec};
use axocoatl_isolation::egress_sidecar::{EgressSidecar, SidecarSpec};
use axocoatl_session::network_record::{BrowserTool as RecordedTool, NetworkEvent};
use axocoatl_session::SessionStore;
use axocoatl_tools::browser_tool::{
    check_payload, collect_check_files, drive_payload, stdin_with_proxy, CheckFile, CheckSource,
    CHECK_SCRIPT, DRIVER_SCRIPT, INLINE_CHECK_ENTRY, MAX_CHECK_FILE_BYTES,
};
use axocoatl_tools::{
    BrowserCheckTool, BrowserJob, BrowserReport, BrowserRunner, BrowserSettings, BrowserTool,
    BuiltinTool, RecordedScreenshot, RunnerOutput, BROWSER_CHECK_TOOL, BROWSER_TOOL,
};
use tokio::sync::Semaphore;

use crate::session_dispatch::{HostInvocationContext, HostInvocationTool};
use crate::session_egress::{EgressPolicyConfig, SessionEgress, SessionRecordSink, SystemResolver};
use crate::session_network::SessionNetworkRecords;
use axocoatl_session::control_authority::ExecutionProfile;

/// What the browser tools run with, resolved from the daemon's config.
#[derive(Debug, Clone)]
pub(crate) struct BrowserServiceConfig {
    pub image: String,
    pub allow: Vec<EgressAllowYaml>,
    pub private_destinations: Vec<String>,
    pub settings: BrowserSettings,
    pub max_parallel: u32,
    pub backend: String,
    /// Podman network for the egress sidecar (`sandbox.egress.sidecar_network`).
    pub sidecar_network: Option<String>,
    pub max_connections: u32,
    pub require_resource_limits: bool,
    pub runtime_authority: String,
    /// Extra labels on every container and volume, such as a test owner.
    pub labels: Vec<(String, String)>,
}

impl BrowserServiceConfig {
    pub(crate) fn from_config(
        config: &axocoatl_config::AxocoatlConfig,
        runtime_authority: String,
    ) -> Option<Self> {
        let browser = config.browser.as_ref()?;
        let egress = config.sandbox.egress.clone().unwrap_or_default();
        Some(Self {
            image: browser
                .image
                .clone()
                .unwrap_or_else(|| axocoatl_config::DEFAULT_BROWSER_IMAGE.to_string()),
            allow: browser.allow.clone(),
            private_destinations: browser.private_destinations.clone(),
            settings: BrowserSettings {
                snapshot_max_bytes: browser.snapshot_max_bytes,
                timeout_secs: browser.timeout_secs,
            },
            max_parallel: browser.max_parallel.clamp(1, 4),
            backend: match config.sandbox.backend.as_str() {
                "" => "podman".into(),
                other => other.into(),
            },
            sidecar_network: egress.sidecar_network,
            max_connections: egress.max_connections,
            require_resource_limits: config.sandbox.require_resource_limits,
            runtime_authority,
            labels: Vec::new(),
        })
    }

    fn declared_hosts(&self) -> bool {
        !self.allow.is_empty()
    }
}

/// The exposed ports of a Session, read from the daemon's Session records.
#[async_trait::async_trait]
pub(crate) trait SessionPorts: Send + Sync {
    async fn exposed_ports(&self, session_id: &str) -> Result<Vec<u16>, String>;
}

#[async_trait::async_trait]
impl SessionPorts for tokio::sync::Mutex<SessionStore> {
    async fn exposed_ports(&self, session_id: &str) -> Result<Vec<u16>, String> {
        self.lock()
            .await
            .get(session_id)
            .map(|session| session.exposed_ports)
            .ok_or_else(|| format!("session '{session_id}' not found"))
    }
}

struct BrowserEgress {
    authority: Arc<SessionEgress>,
    sidecar: Arc<EgressSidecar>,
}

/// One Session's browser state.
struct BrowserSession {
    permits: Arc<Semaphore>,
    /// Serializes checking and restarting the Session's port forwarder.
    sockets: tokio::sync::Mutex<()>,
    container: tokio::sync::Mutex<Option<Arc<BrowserContainer>>>,
    egress: tokio::sync::Mutex<Option<BrowserEgress>>,
}

/// The daemon's browser runtime.
pub(crate) struct BrowserService {
    config: BrowserServiceConfig,
    supervisor_installation: SecureDir,
    egress_context: SecureDir,
    /// Host directories the browser must never read from a Workspace.
    control_plane_dirs: Vec<PathBuf>,
    records: Arc<SessionNetworkRecords>,
    ports: Arc<dyn SessionPorts>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<BrowserSession>>>,
}

impl std::fmt::Debug for BrowserService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserService")
            .field("image", &self.config.image)
            .finish_non_exhaustive()
    }
}

impl BrowserService {
    pub(crate) fn new(
        config: BrowserServiceConfig,
        supervisor_installation: SecureDir,
        egress_context: SecureDir,
        control_plane_dirs: Vec<PathBuf>,
        records: Arc<SessionNetworkRecords>,
        ports: Arc<dyn SessionPorts>,
    ) -> Self {
        Self {
            config,
            supervisor_installation,
            egress_context,
            control_plane_dirs,
            records,
            ports,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn config(&self) -> &BrowserServiceConfig {
        &self.config
    }

    async fn session(&self, session_id: &str) -> Arc<BrowserSession> {
        self.sessions
            .lock()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(BrowserSession {
                    permits: Arc::new(Semaphore::new(self.config.max_parallel as usize)),
                    sockets: tokio::sync::Mutex::new(()),
                    container: tokio::sync::Mutex::new(None),
                    egress: tokio::sync::Mutex::new(None),
                })
            })
            .clone()
    }

    /// Drop a Session's browser state and stop its egress sidecar. The
    /// Session's runtime cleanup removes the containers and volumes.
    pub(crate) async fn forget(&self, session_id: &str) {
        let session = self.sessions.lock().await.remove(session_id);
        if let Some(session) = session {
            if let Some(egress) = session.egress.lock().await.take() {
                egress.sidecar.stop().await;
            }
            if let Some(container) = session.container.lock().await.take() {
                let _ = container.remove().await;
            }
        }
    }

    /// The browser-scope decision point, with its sidecar running.
    async fn ensure_egress(
        &self,
        session_id: &str,
        session: &BrowserSession,
    ) -> Result<Arc<SessionEgress>, String> {
        let mut egress = session.egress.lock().await;
        if egress.is_none() {
            let authority = SessionEgress::open_browser_only(
                session_id,
                EgressPolicyConfig {
                    session_allow: Vec::new(),
                    session_private: Vec::new(),
                    browser: Some((
                        self.config.allow.clone(),
                        self.config.private_destinations.clone(),
                    )),
                },
                Arc::new(SessionRecordSink::new(self.records.clone(), session_id)),
                Arc::new(SystemResolver),
            )
            .await?;
            let architecture = axocoatl_isolation::egress_image::podman_architecture()
                .await
                .map_err(|error| error.to_string())?;
            let image = axocoatl_isolation::egress_image::ensure_egress_image(
                &architecture,
                &self.egress_context,
            )
            .await
            .map_err(|error| error.to_string())?;
            let sidecar = Arc::new(EgressSidecar::new(
                SidecarSpec {
                    session_id: session_id.to_string(),
                    runtime_authority: self.config.runtime_authority.clone(),
                    image,
                    network: self.config.sidecar_network.clone(),
                    max_connections: self.config.max_connections,
                    with_limits: true,
                    require_limits: self.config.require_resource_limits,
                    labels: self.config.labels.clone(),
                },
                authority.clone() as Arc<dyn EgressAuthority>,
            ));
            *egress = Some(BrowserEgress { authority, sidecar });
        }
        let current = egress.as_ref().expect("set above");
        let control = current
            .sidecar
            .ensure_running()
            .await
            .map_err(|error| error.to_string())?;
        current.authority.attach_control(control);
        Ok(current.authority.clone())
    }

    /// The running browser container for these ports, started if needed.
    async fn ensure_container(
        &self,
        session_id: &str,
        session: &BrowserSession,
        ports: &[u16],
    ) -> Result<Arc<BrowserContainer>, String> {
        let egress = self.config.declared_hosts();
        let mut slot = session.container.lock().await;
        if let Some(container) = slot.as_ref() {
            if container.serves(ports, egress) && container.is_running().await {
                return Ok(container.clone());
            }
            let _ = container.remove().await;
            *slot = None;
        }
        let container = BrowserContainer::start(&BrowserLaunch {
            session_id: session_id.to_string(),
            runtime_authority: self.config.runtime_authority.clone(),
            image: self.config.image.clone(),
            exposed_ports: ports.to_vec(),
            egress,
            supervisor_installation: self.supervisor_installation.clone(),
            require_resource_limits: self.config.require_resource_limits,
            labels: self.config.labels.clone(),
        })
        .await
        .map_err(|error| error.to_string())?;
        let container = Arc::new(container);
        *slot = Some(container.clone());
        Ok(container)
    }

    fn inside_control_plane(&self, checkout: &Path, relative: &str) -> bool {
        let path = checkout.join(relative);
        self.control_plane_dirs
            .iter()
            .any(|directory| path.starts_with(directory))
    }

    /// Read a test file and its relative imports from the checkout, without
    /// following links and never from Axocoatl's own directories.
    async fn check_files(
        self: &Arc<Self>,
        checkout: SecureDir,
        entry: String,
    ) -> Result<Vec<CheckFile>, String> {
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut read = |path: &str| -> Result<Option<String>, String> {
                if service.inside_control_plane(checkout.path(), path) {
                    return Ok(None);
                }
                match checkout.read_limited(path, MAX_CHECK_FILE_BYTES) {
                    Ok(bytes) => String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|_| format!("{path} is not UTF-8 text")),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(format!("{path} cannot be read: {error}")),
                }
            };
            collect_check_files(&entry, &mut read)
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

fn aut_origins(ports: &[u16]) -> Vec<String> {
    ports
        .iter()
        .map(|port| format!("http://localhost:{port}"))
        .collect()
}

/// The password in a proxy URL `http://axo:<token>@127.0.0.1:3128`.
fn proxy_password(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    parsed.password().map(str::to_string)
}

/// Runs one call for one exact invocation.
struct BrowserCallRunner {
    service: Arc<BrowserService>,
    context: HostInvocationContext,
    /// The egress credential tag the call used, for its record event.
    token: Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl BrowserRunner for BrowserCallRunner {
    async fn run(&self, job: &BrowserJob) -> Result<RunnerOutput, String> {
        let service = &self.service;
        let session_id = self.context.session_id.as_str();
        let session = service.session(session_id).await;
        let _permit = session
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| error.to_string())?;
        let ports = service.ports.exposed_ports(session_id).await?;
        let settings = service.config.settings;
        let (script, payload) = match job {
            BrowserJob::Drive(job) => (
                DRIVER_SCRIPT,
                drive_payload(job, &aut_origins(&ports), &settings),
            ),
            BrowserJob::Check(job) => {
                let (entry, files) = match &job.source {
                    CheckSource::Path(path) => {
                        let checkout = self.context.checkout.clone().ok_or(
                            "browser_check with a path needs this Session's repository; pass the test as script instead",
                        )?;
                        (
                            path.clone(),
                            service.check_files(checkout, path.clone()).await?,
                        )
                    }
                    CheckSource::Script(script) => (
                        INLINE_CHECK_ENTRY.to_string(),
                        vec![CheckFile {
                            path: INLINE_CHECK_ENTRY.to_string(),
                            content: script.clone(),
                        }],
                    ),
                };
                let base_url = match (&job.base_url, ports.first()) {
                    (Some(url), _) => url.clone(),
                    (None, Some(port)) => format!("http://localhost:{port}"),
                    (None, None) => {
                        return Err("this Session exposes no ports; pass base_url".to_string())
                    }
                };
                (
                    CHECK_SCRIPT,
                    check_payload(&entry, &files, job, &base_url, &settings),
                )
            }
        };
        {
            let _sockets = session.sockets.lock().await;
            ensure_service_sockets(session_id, &ports)
                .await
                .map_err(|error| error.to_string())?;
        }
        let egress = if service.config.declared_hosts() {
            Some(service.ensure_egress(session_id, &session).await?)
        } else {
            None
        };
        let container = service
            .ensure_container(session_id, &session, &ports)
            .await?;
        let grant = match &egress {
            Some(authority) => {
                let grant = authority
                    .grant(GrantSpec {
                        invocation_id: Some(self.context.invocation_id.as_str().to_string()),
                        activation_id: Some(
                            self.context.activation.activation_id.as_str().to_string(),
                        ),
                        node_id: Some(self.context.activation.node_id.as_str().to_string()),
                        agent: Some(self.context.agent.clone()),
                        ..GrantSpec::new(GrantKind::Browser)
                    })
                    .await?;
                *self
                    .token
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(grant.token_tag.clone());
                Some(grant)
            }
            None => None,
        };
        let password = grant
            .as_ref()
            .and_then(|grant| grant.proxy_url_for_stdin.as_ref())
            .and_then(|url| proxy_password(url.expose()));
        let stdin = stdin_with_proxy(&payload, password.as_deref())?;
        let output = container
            .run_script(
                self.context.invocation_id.as_str(),
                script,
                stdin,
                settings.run_timeout(),
            )
            .await;
        // Unbind the credential and close what it opened before answering.
        drop(grant);
        let output = output.map_err(|error| error.to_string())?;
        Ok(RunnerOutput {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn record(
        &self,
        job: &BrowserJob,
        report: &BrowserReport,
    ) -> Result<Option<RecordedScreenshot>, String> {
        let session_id = self.context.session_id.as_str();
        let mut dropped = report.screenshot_dropped.clone();
        let stored = match &report.screenshot {
            Some(shot) => match self
                .service
                .records
                .store_screenshot(session_id, shot.media_type, shot.bytes.clone())
                .await
            {
                Ok(stored) => Some(stored),
                Err(error) if error.is_full() => {
                    dropped = Some("record_full".into());
                    None
                }
                Err(error) => return Err(error.to_string()),
            },
            None => None,
        };
        let clip = |value: Option<&str>| {
            value.map(|value| {
                value
                    .chars()
                    .take(axocoatl_session::network_record::MAX_RECORDED_URL_CHARS)
                    .collect::<String>()
            })
        };
        let blocked = report.result["network"]["blocked"]
            .as_array()
            .map_or(0, |blocked| blocked.len() as u32);
        let (tool, test_path) = match job {
            BrowserJob::Drive(_) => (RecordedTool::Browser, None),
            BrowserJob::Check(check) => (
                RecordedTool::BrowserCheck,
                Some(match &check.source {
                    CheckSource::Path(path) => path.clone(),
                    CheckSource::Script(_) => INLINE_CHECK_ENTRY.to_string(),
                }),
            ),
        };
        let event = NetworkEvent::Browser {
            tool,
            invocation_id: self.context.invocation_id.as_str().to_string(),
            activation_id: self.context.activation.activation_id.as_str().to_string(),
            agent: self.context.agent.clone(),
            ok: report.ok,
            url: clip(job.url()),
            final_url: clip(report.final_url.as_deref()),
            status: report.status,
            test_path,
            check_status: report.check_status.clone(),
            blocked,
            token: self
                .token
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone(),
            screenshot: stored.clone(),
            screenshot_dropped: dropped.map(|reason| reason.chars().take(200).collect()),
            ms: report.ms,
        };
        self.service
            .records
            .append(session_id, event)
            .await
            .map_err(|error| error.to_string())?;
        Ok(stored.map(|stored| RecordedScreenshot {
            sha256: stored.sha256,
            bytes: stored.bytes as usize,
        }))
    }
}

/// `browser` or `browser_check` as a host-invocation tool.
pub(crate) struct BrowserHostTool {
    service: Arc<BrowserService>,
    check: bool,
}

impl BrowserHostTool {
    pub(crate) fn browser(service: Arc<BrowserService>) -> Self {
        Self {
            service,
            check: false,
        }
    }

    pub(crate) fn browser_check(service: Arc<BrowserService>) -> Self {
        Self {
            service,
            check: true,
        }
    }
}

/// Why this daemon cannot run the browser tools at all, if it cannot.
pub(crate) fn browser_refusal(backend: &str) -> Option<String> {
    (backend != "podman").then(|| {
        format!("the browser tools run in a local Podman container; this daemon uses backend: {backend}")
    })
}

impl HostInvocationTool for BrowserHostTool {
    fn name(&self) -> &'static str {
        if self.check {
            BROWSER_CHECK_TOOL
        } else {
            BROWSER_TOOL
        }
    }

    fn definition(&self) -> Arc<dyn BuiltinTool> {
        if self.check {
            Arc::new(BrowserCheckTool::definition())
        } else {
            Arc::new(BrowserTool::definition())
        }
    }

    fn refusal(&self, _profile: &ExecutionProfile) -> Option<String> {
        browser_refusal(&self.service.config.backend)
    }

    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool> {
        let runner = Arc::new(BrowserCallRunner {
            service: self.service.clone(),
            context,
            token: Mutex::new(None),
        });
        if self.check {
            Arc::new(BrowserCheckTool::with_runner(runner))
        } else {
            Arc::new(BrowserTool::with_runner(runner))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_session::turn_contract::{
        ActivationId, ActivationRef, ExecutionEpochId, InvocationId, LogicalTurnId, SessionId,
        TurnNodeId,
    };
    use axocoatl_tools::browser_tool::{parse_check_call, parse_drive_call};
    use axocoatl_tools::Screenshot;

    struct FixedPorts(Vec<u16>);

    #[async_trait::async_trait]
    impl SessionPorts for FixedPorts {
        async fn exposed_ports(&self, _: &str) -> Result<Vec<u16>, String> {
            Ok(self.0.clone())
        }
    }

    fn service(
        root: &Path,
        records: Arc<SessionNetworkRecords>,
        control: Vec<PathBuf>,
    ) -> Arc<BrowserService> {
        let config = axocoatl_config::AxocoatlConfig {
            browser: Some(axocoatl_config::BrowserConfigYaml::default()),
            ..Default::default()
        };
        let dir = SecureDir::open_or_create_all(root.join("service")).unwrap();
        Arc::new(BrowserService::new(
            BrowserServiceConfig::from_config(&config, "authority".into()).unwrap(),
            dir.child("supervisors").unwrap(),
            dir.child("egress").unwrap(),
            control,
            records,
            Arc::new(FixedPorts(vec![8765])),
        ))
    }

    fn context(checkout: Option<SecureDir>) -> HostInvocationContext {
        HostInvocationContext {
            session_id: "ses-1".into(),
            invocation_id: InvocationId::new("inv-browser-1").unwrap(),
            activation: ActivationRef {
                session_id: SessionId::new("ses-1").unwrap(),
                turn_id: LogicalTurnId::new("turn-1").unwrap(),
                execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
                node_id: TurnNodeId::new("scout").unwrap(),
                generation: 1,
                activation_id: ActivationId::new("act-1").unwrap(),
            },
            agent: "qa-scout".into(),
            read_only: true,
            checkout,
        }
    }

    #[tokio::test]
    async fn a_finished_call_is_recorded_with_its_screenshot() {
        let root = tempfile::tempdir().unwrap();
        let stores = crate::session_network::tests::Stores::new(&["ses-1"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let runner = BrowserCallRunner {
            service: service(root.path(), records.clone(), Vec::new()),
            context: context(None),
            token: Mutex::new(Some("0123456789abcdef".into())),
        };
        let job = BrowserJob::Drive(
            parse_drive_call(&serde_json::json!({"url": "http://localhost:8765/"})).unwrap(),
        );
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg.extend([3u8; 32]);
        let report = BrowserReport {
            tool: BROWSER_TOOL,
            ok: true,
            final_url: Some("http://localhost:8765/cart".into()),
            status: Some(200),
            check_status: None,
            ms: 900,
            result: serde_json::json!({"network": {"blocked": [{"url": "http://cdn.test/", "reason": "not_allowed"}]}}),
            screenshot: Some(Screenshot {
                media_type: "image/jpeg",
                bytes: jpeg.clone(),
            }),
            screenshot_dropped: None,
        };
        let recorded = runner.record(&job, &report).await.unwrap().unwrap();
        assert_eq!(recorded.bytes, 36);
        let page = records.read_after("ses-1", None, 100).await.unwrap();
        let event = &page.events.last().unwrap().event;
        match event {
            NetworkEvent::Browser {
                tool,
                invocation_id,
                activation_id,
                agent,
                ok,
                url,
                final_url,
                status,
                blocked,
                token,
                screenshot,
                ..
            } => {
                assert_eq!(*tool, RecordedTool::Browser);
                assert_eq!(invocation_id, "inv-browser-1");
                assert_eq!(activation_id, "act-1");
                assert_eq!(agent, "qa-scout");
                assert!(*ok);
                assert_eq!(url.as_deref(), Some("http://localhost:8765/"));
                assert_eq!(final_url.as_deref(), Some("http://localhost:8765/cart"));
                assert_eq!(*status, Some(200));
                assert_eq!(*blocked, 1);
                assert_eq!(token.as_deref(), Some("0123456789abcdef"));
                assert_eq!(screenshot.as_ref().unwrap().sha256, recorded.sha256);
            }
            other => panic!("{other:?}"),
        }
        let (media, bytes) = records
            .read_screenshot("ses-1", &recorded.sha256)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((media.as_str(), bytes), ("image/jpeg", jpeg));

        // A check is recorded with its test file and status.
        let check = BrowserJob::Check(
            parse_check_call(&serde_json::json!({"path": "qa/b07.spec.ts"})).unwrap(),
        );
        let report = BrowserReport {
            tool: BROWSER_CHECK_TOOL,
            ok: false,
            final_url: None,
            status: None,
            check_status: Some("failed".into()),
            ms: 4000,
            result: serde_json::json!({"status": "failed"}),
            screenshot: None,
            screenshot_dropped: Some("too_large".into()),
        };
        assert!(runner.record(&check, &report).await.unwrap().is_none());
        let page = records.read_after("ses-1", None, 100).await.unwrap();
        assert!(matches!(
            &page.events.last().unwrap().event,
            NetworkEvent::Browser { tool: RecordedTool::BrowserCheck, test_path: Some(path), check_status: Some(status), screenshot_dropped: Some(dropped), .. }
                if path == "qa/b07.spec.ts" && status == "failed" && dropped == "too_large"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_files_never_follow_links_or_read_axocoatl_state() {
        let root = tempfile::tempdir().unwrap();
        let checkout_path = root.path().join("checkout");
        std::fs::create_dir_all(checkout_path.join("qa")).unwrap();
        std::fs::create_dir_all(checkout_path.join("data")).unwrap();
        std::fs::write(
            checkout_path.join("qa/a.spec.ts"),
            "import { x } from './fixtures';\nimport t from '../data/token.json';\n",
        )
        .unwrap();
        std::fs::write(checkout_path.join("qa/fixtures.ts"), "export const x = 1;").unwrap();
        std::fs::write(checkout_path.join("data/token.json"), "{\"secret\": true}").unwrap();
        std::fs::write(root.path().join("outside.ts"), "export const y = 1;").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("outside.ts"),
            checkout_path.join("qa/link.spec.ts"),
        )
        .unwrap();
        let checkout = SecureDir::open(&checkout_path).unwrap();
        let stores = crate::session_network::tests::Stores::new(&["ses-1"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let service = service(root.path(), records, vec![checkout_path.join("data")]);
        let files = service
            .check_files(checkout.clone(), "qa/a.spec.ts".into())
            .await
            .unwrap();
        let mut paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["qa/a.spec.ts", "qa/fixtures.ts"]);
        let error = service
            .check_files(checkout, "qa/link.spec.ts".into())
            .await
            .unwrap_err();
        assert!(error.contains("cannot be read"), "{error}");
    }

    #[test]
    fn origins_passwords_and_refusals() {
        assert_eq!(
            aut_origins(&[5173, 8765]),
            vec!["http://localhost:5173", "http://localhost:8765"]
        );
        assert_eq!(
            proxy_password("http://axo:axe_abc-DEF_123@127.0.0.1:3128").as_deref(),
            Some("axe_abc-DEF_123")
        );
        assert_eq!(proxy_password("http://127.0.0.1:3128"), None);
        assert!(browser_refusal("podman").is_none());
        assert!(browser_refusal("e2b").unwrap().contains("Podman"));
    }

    #[test]
    fn config_resolves_the_default_image_and_bounds() {
        let mut config = axocoatl_config::AxocoatlConfig::default();
        assert!(BrowserServiceConfig::from_config(&config, "a".into()).is_none());
        config.browser = Some(axocoatl_config::BrowserConfigYaml::default());
        let resolved = BrowserServiceConfig::from_config(&config, "a".into()).unwrap();
        assert_eq!(resolved.image, axocoatl_config::DEFAULT_BROWSER_IMAGE);
        assert_eq!(resolved.backend, "podman");
        assert_eq!(resolved.max_parallel, 2);
        assert!(!resolved.declared_hosts());
        assert_eq!(resolved.settings.timeout_secs, 120);
    }
}
