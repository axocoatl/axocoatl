//! The `browser` and `browser_check` tools for native Sessions.
//!
//! [`BrowserService`] is daemon-wide. For each Session it keeps the browser
//! container, a semaphore of `browser.max_parallel` permits, a lock that
//! lets `browser_check` run alone and, under `bridge` and `none` when
//! `browser.allow` lists hosts, a browser-scope egress decision point and its
//! sidecar. Each call:
//!
//! 1. makes sure the Session's exposed ports are served as sockets: under
//!    `bridge` and `none` by the service forwarder (a container of its own in
//!    the Session's network namespace; the Session container never sees the
//!    sockets), under `egress` by the Session container's own bridge, which
//!    serves them for Preview;
//! 2. when declared hosts exist, finds the decision point that answers the
//!    browser's proxy: under `egress` the Session's own, whose sidecar
//!    (`axo-egr-{session}`) already runs and whose `browser` scope compiles
//!    `browser.allow` apart from the Session's list; under `bridge` and
//!    `none` a browser-only one, with a sidecar of its own;
//! 3. starts or reuses the browser container;
//! 4. takes a browser egress credential bound to the call (declared hosts
//!    only), which reaches the driver only on its stdin;
//! 5. runs the driver or the check runner under the supervisor;
//! 6. drops the credential, which closes its connections, and keeps any
//!    screenshot and a `browser` event in the Session's network record. A
//!    call that fails is recorded too, with its reason.
//!
//! `browser_check` runs code the model wrote. It runs alone in the browser
//! container, and the container is replaced after it, so nothing it leaves
//! (files, processes) reaches another call.
//!
//! The browser never sees the repository. `browser_check` sends the one test
//! file it names and the source files that file imports, nothing else. The
//! tools are refused in Ways attempt lanes: the browser reaches the primary
//! Session container's ports, not an attempt's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axocoatl_config::EgressAllowYaml;
use axocoatl_core::SecureDir;
use axocoatl_isolation::browser_container::{
    browser_proxy_port, ensure_service_sockets, BrowserContainer, BrowserLaunch,
    ServiceSocketsLaunch,
};
use axocoatl_isolation::egress::{EgressAuthority, GrantKind, GrantSpec};
use axocoatl_isolation::egress_control::ControlTiming;
use axocoatl_isolation::egress_sidecar::{EgressSidecar, SidecarPhase, SidecarSpec};
use axocoatl_session::network_record::{BrowserTool as RecordedTool, NetworkEvent};
use axocoatl_session::SessionStore;
use axocoatl_tools::browser_tool::{
    check_payload, collect_check_files, drive_payload, stdin_with_proxy, CheckFile, CheckSource,
    ProxyCredential, CHECK_SCRIPT, DRIVER_SCRIPT, INLINE_CHECK_ENTRY, MAX_CHECK_FILE_BYTES,
    MAX_RECORDED_ERROR_CHARS,
};
use axocoatl_tools::{
    BrowserCheckTool, BrowserJob, BrowserReport, BrowserRunner, BrowserSettings, BrowserTool,
    BuiltinTool, RecordedScreenshot, RunnerOutput, BROWSER_CHECK_TOOL, BROWSER_TOOL,
};
use tokio::sync::Semaphore;

use crate::session_dispatch::{HostInvocationContext, HostInvocationTool};
use crate::session_egress::{EgressPolicyConfig, SessionEgress, SessionRecordSink, SystemResolver};
use crate::session_network::{PolicyView, SessionNetworkRecords, SidecarView};
use crate::session_network_reload::{reload_points, PointsReload};
use axocoatl_session::control_authority::ExecutionProfile;
use axocoatl_session::network_record::EgressScope;

/// Why the browser tools are refused in a Ways attempt lane.
pub(crate) const ATTEMPT_REFUSAL: &str = "the browser tools are not available in a Ways attempt: \
     the browser reaches the primary Session container's exposed ports, not this attempt's app, \
     so its results would not describe this attempt's code";
/// Why `browser_check` is refused to a read-only Agent.
pub(crate) const READ_ONLY_CHECK_REFUSAL: &str = "browser_check runs test code the model writes, \
     which can do whatever the apps on the exposed ports allow, so it is not offered to a \
     read-only Agent (writes: []); give it browser instead";

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
    /// `sandbox.network`. Under `egress` declared browser hosts belong to
    /// the Session's own decision point and sidecar.
    pub session_network: String,
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
            session_network: config.sandbox.network.clone(),
            labels: Vec::new(),
        })
    }
}

/// The Session's own egress decision point and sidecar, under `network:
/// egress`. The daemon implements it over its decision points and the
/// Session's runtime.
#[async_trait::async_trait]
pub(crate) trait SessionEgressSource: Send + Sync {
    /// The Session's decision point, opened on first use.
    async fn session_egress(&self, session_id: &str) -> Result<Arc<SessionEgress>, String>;
    /// Whether the Session's runtime runs and its egress sidecar is ready.
    async fn session_sidecar_ready(&self, session_id: &str) -> bool;
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
    /// `browser` calls share it; `browser_check`, which runs code the model
    /// wrote with the same user and `/tmp` as every call in the container,
    /// holds it alone.
    calls: Arc<tokio::sync::RwLock<()>>,
    /// Serializes checking and restarting the Session's port forwarder.
    sockets: tokio::sync::Mutex<()>,
    container: tokio::sync::Mutex<Option<Arc<BrowserContainer>>>,
    egress: tokio::sync::Mutex<Option<BrowserEgress>>,
}

/// The daemon's browser runtime.
pub(crate) struct BrowserService {
    config: BrowserServiceConfig,
    /// `browser.allow` and `browser.private_destinations` in force; a
    /// configuration reload replaces them.
    declared: Mutex<(Vec<EgressAllowYaml>, Vec<String>)>,
    /// The Session's own decision point under `network: egress`.
    egress_source: Arc<dyn SessionEgressSource>,
    supervisor_installation: SecureDir,
    /// Host directories the browser must never read from a Workspace.
    control_plane_dirs: Vec<PathBuf>,
    records: Arc<SessionNetworkRecords>,
    ports: Arc<dyn SessionPorts>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<BrowserSession>>>,
    /// The scratch egress image the service forwarder runs, once built.
    forwarder_image: tokio::sync::Mutex<Option<String>>,
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
        control_plane_dirs: Vec<PathBuf>,
        records: Arc<SessionNetworkRecords>,
        ports: Arc<dyn SessionPorts>,
        egress_source: Arc<dyn SessionEgressSource>,
    ) -> Self {
        Self {
            declared: Mutex::new((config.allow.clone(), config.private_destinations.clone())),
            egress_source,
            config,
            supervisor_installation,
            control_plane_dirs,
            records,
            ports,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            forwarder_image: tokio::sync::Mutex::new(None),
        }
    }

    fn declared(&self) -> (Vec<EgressAllowYaml>, Vec<String>) {
        self.declared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Whether `browser.allow` lists hosts now.
    fn declared_hosts(&self) -> bool {
        !self
            .declared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .0
            .is_empty()
    }

    /// Apply reloaded `browser.allow` and `browser.private_destinations`:
    /// later calls use them, and each browser-only decision point (under
    /// `bridge` and `none`) that has not applied them records and applies
    /// them at once. Under `egress` the Session's own decision point holds
    /// the browser's policy and is reloaded with the Session's.
    pub(crate) async fn reload_declared(
        &self,
        allow: Vec<EgressAllowYaml>,
        private_destinations: Vec<String>,
        actor: &str,
    ) -> PointsReload {
        *self
            .declared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) =
            (allow.clone(), private_destinations.clone());
        let mut sessions: Vec<(String, Arc<BrowserSession>)> = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        sessions.sort_by(|left, right| left.0.cmp(&right.0));
        let config = EgressPolicyConfig {
            session_allow: Vec::new(),
            session_private: Vec::new(),
            browser: Some((allow, private_destinations)),
        };
        let mut points = Vec::new();
        for (session_id, session) in sessions {
            if let Some(egress) = session.egress.lock().await.as_ref() {
                points.push((session_id, egress.authority.clone(), config.clone()));
            }
        }
        reload_points(points, actor).await
    }

    async fn session(&self, session_id: &str) -> Arc<BrowserSession> {
        self.sessions
            .lock()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(BrowserSession {
                    permits: Arc::new(Semaphore::new(self.config.max_parallel as usize)),
                    calls: Arc::new(tokio::sync::RwLock::new(())),
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

    /// The scratch egress image tag, built from the bundled supervisor once.
    async fn forwarder_image(&self) -> Result<String, String> {
        let mut cached = self.forwarder_image.lock().await;
        if let Some(image) = cached.as_ref() {
            return Ok(image.clone());
        }
        let image = axocoatl_isolation::egress_image::ensure_egress_image_for_podman(
            &self.supervisor_installation,
        )
        .await
        .map_err(|error| error.to_string())?;
        *cached = Some(image.clone());
        Ok(image)
    }

    /// Serve the Session's exposed ports as sockets for the browser through
    /// the service forwarder. Under `network: egress` the Session's start
    /// already ran it for Preview, and this keeps it.
    async fn ensure_service_sockets(
        &self,
        session_id: &str,
        session: &BrowserSession,
        ports: &[u16],
    ) -> Result<(), String> {
        let _sockets = session.sockets.lock().await;
        // Under egress this finds the forwarder serving the same ports for
        // the same Session container and keeps it.
        let image = self.forwarder_image().await?;
        let result = ensure_service_sockets(&ServiceSocketsLaunch {
            session_id: session_id.to_string(),
            runtime_authority: self.config.runtime_authority.clone(),
            image,
            ports: ports.to_vec(),
            require_resource_limits: self.config.require_resource_limits,
            labels: self.config.labels.clone(),
        })
        .await;
        if result.is_err() {
            // The image may have been removed; look again on the next call.
            *self.forwarder_image.lock().await = None;
        }
        result.map(|_| ()).map_err(|error| error.to_string())
    }

    /// The browser's sidecar and policy for the Session's network view, when
    /// its declared hosts have been used. A sidecar being started reads as
    /// `starting`.
    pub(crate) async fn network_view(
        &self,
        session_id: &str,
    ) -> (Option<SidecarView>, Vec<PolicyView>) {
        let session = self.sessions.lock().await.get(session_id).cloned();
        let Some(session) = session else {
            return (None, Vec::new());
        };
        let Ok(egress) = session.egress.try_lock() else {
            return (
                Some(SidecarView {
                    state: "starting".into(),
                    generation: 0,
                    restarts: 0,
                }),
                Vec::new(),
            );
        };
        match egress.as_ref() {
            Some(current) => {
                let status = current.sidecar.status();
                (
                    Some(SidecarView {
                        state: status.phase.as_str().to_string(),
                        generation: status.generation,
                        restarts: status.restarts,
                    }),
                    current.authority.policy_views(),
                )
            }
            None => (None, Vec::new()),
        }
    }

    /// The decision point that answers the browser's proxy, with its
    /// sidecar running. Under `network: egress` it is the Session's own: its
    /// sidecar (`axo-egr-{session}`) already serves the socket the browser
    /// container mounts, and its `browser` scope holds `browser.allow`. A
    /// second sidecar here would take that name and split the record's
    /// policy. Under `bridge` and `none` the browser has its own.
    async fn ensure_egress(
        &self,
        session_id: &str,
        session: &BrowserSession,
    ) -> Result<Arc<SessionEgress>, String> {
        if self.config.session_network == "egress" {
            let authority = self.egress_source.session_egress(session_id).await?;
            if authority.policy(EgressScope::Browser).is_none() {
                return Err(
                    "this Session's egress decision point has no browser policy; restart the daemon after adding the browser block"
                        .to_string(),
                );
            }
            if !self.egress_source.session_sidecar_ready(session_id).await {
                return Err("the Session's egress proxy is not running, so the browser cannot reach its declared hosts; it starts with the Session's runtime".to_string());
            }
            return Ok(authority);
        }
        let mut egress = session.egress.lock().await;
        // The sidecar restarts itself when its channel is lost. One stopped
        // with the Session's runtime is started again here; one that used up
        // its restarts stays failed until the Session's runtime restarts.
        if let Some(current) = egress.as_ref() {
            match current.sidecar.status().phase {
                SidecarPhase::Stopped => {
                    if let Some(stale) = egress.take() {
                        stale.sidecar.stop().await;
                    }
                }
                SidecarPhase::Failed => {
                    return Err("the browser's egress proxy stopped too often and is not \
                         started again until the Session restarts"
                        .to_string());
                }
                _ => return Ok(current.authority.clone()),
            }
        }
        let authority = SessionEgress::open_browser_only(
            session_id,
            EgressPolicyConfig {
                session_allow: Vec::new(),
                session_private: Vec::new(),
                browser: Some(self.declared()),
            },
            Arc::new(SessionRecordSink::new(self.records.clone(), session_id)),
            Arc::new(SystemResolver),
        )
        .await?;
        let image = self.forwarder_image().await?;
        let sidecar = EgressSidecar::start(
            SidecarSpec {
                session_id: session_id.to_string(),
                runtime_authority: Some(self.config.runtime_authority.clone()),
                image,
                network: self.config.sidecar_network.clone(),
                max_connections: self.config.max_connections,
                require_resource_limits: self.config.require_resource_limits,
                labels: self
                    .config
                    .labels
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
            },
            authority.clone() as Arc<dyn EgressAuthority>,
            ControlTiming::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
        *egress = Some(BrowserEgress {
            authority: authority.clone(),
            sidecar: Arc::new(sidecar),
        });
        Ok(authority)
    }

    /// The running browser container for these ports, started if needed.
    async fn ensure_container(
        &self,
        session_id: &str,
        session: &BrowserSession,
        ports: &[u16],
    ) -> Result<Arc<BrowserContainer>, String> {
        let egress = self.declared_hosts();
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

/// Holds a call's place: shared for `browser`, alone for `browser_check`.
enum CallTurn {
    Shared(#[allow(dead_code)] tokio::sync::OwnedRwLockReadGuard<()>),
    Alone(#[allow(dead_code)] tokio::sync::OwnedRwLockWriteGuard<()>),
}

#[async_trait::async_trait]
impl BrowserRunner for BrowserCallRunner {
    async fn run(&self, job: &BrowserJob) -> Result<RunnerOutput, String> {
        if self.context.attempt {
            return Err(ATTEMPT_REFUSAL.to_string());
        }
        let service = &self.service;
        let session_id = self.context.session_id.as_str();
        let session = service.session(session_id).await;
        // browser_check runs code the model wrote with the same user and
        // /tmp as every other call here, so it runs alone.
        let _turn = match job {
            BrowserJob::Check(_) => CallTurn::Alone(session.calls.clone().write_owned().await),
            BrowserJob::Drive(_) => CallTurn::Shared(session.calls.clone().read_owned().await),
        };
        let _permit = session
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| error.to_string())?;
        let ports = service.ports.exposed_ports(session_id).await?;
        let proxy_server = format!("http://127.0.0.1:{}", browser_proxy_port(&ports));
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
        service
            .ensure_service_sockets(session_id, &session, &ports)
            .await?;
        let egress = if service.declared_hosts() {
            Some(service.ensure_egress(session_id, &session).await?)
        } else {
            None
        };
        let container = service
            .ensure_container(session_id, &session, &ports)
            .await?;
        if matches!(job, BrowserJob::Check(_)) {
            // The test runs code the model wrote: no later call reuses this
            // container, even if this one is cancelled before it is removed
            // (the next start replaces it by name).
            let mut slot = session.container.lock().await;
            if slot
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &container))
            {
                *slot = None;
            }
        }
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
        let stdin = stdin_with_proxy(
            &payload,
            password.as_deref().map(|password| ProxyCredential {
                server: &proxy_server,
                password,
            }),
        )?;
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
        if matches!(job, BrowserJob::Check(_)) {
            // Still alone: remove the container, so no file or process the
            // test left behind reaches a later call.
            if let Err(error) = container.remove().await {
                tracing::warn!(session = session_id, %error, "removing the browser container after browser_check failed");
            }
        }
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
            error: report
                .error
                .as_ref()
                .map(|reason| reason.chars().take(MAX_RECORDED_ERROR_CHARS).collect()),
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
pub(crate) fn browser_refusal(config: &BrowserServiceConfig) -> Option<String> {
    (config.backend != "podman").then(|| {
        format!(
            "the browser tools run in a local Podman container; this daemon uses backend: {}",
            config.backend
        )
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

    fn refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        if self.check && profile.write_scope.as_ref().is_some_and(Vec::is_empty) {
            return Some(READ_ONLY_CHECK_REFUSAL.to_string());
        }
        browser_refusal(&self.service.config)
    }

    fn attempt_refusal(&self) -> Option<String> {
        Some(ATTEMPT_REFUSAL.to_string())
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
pub(crate) mod tests {
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

    /// The Session's own decision point, when there is one, and whether its
    /// sidecar is ready.
    pub(crate) struct FixedEgress {
        pub(crate) egress: Option<Arc<SessionEgress>>,
        pub(crate) ready: bool,
    }

    #[async_trait::async_trait]
    impl SessionEgressSource for FixedEgress {
        async fn session_egress(&self, _: &str) -> Result<Arc<SessionEgress>, String> {
            self.egress
                .clone()
                .ok_or_else(|| "this daemon has no Session decision points".to_string())
        }

        async fn session_sidecar_ready(&self, _: &str) -> bool {
            self.ready
        }
    }

    pub(crate) fn no_session_egress() -> Arc<dyn SessionEgressSource> {
        Arc::new(FixedEgress {
            egress: None,
            ready: false,
        })
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
            control,
            records,
            Arc::new(FixedPorts(vec![8765])),
            no_session_egress(),
        ))
    }

    fn egress_service(root: &Path, source: FixedEgress) -> Arc<BrowserService> {
        let mut config = axocoatl_config::AxocoatlConfig {
            browser: Some(axocoatl_config::BrowserConfigYaml::default()),
            ..Default::default()
        };
        config.sandbox.network = "egress".into();
        let mut resolved = BrowserServiceConfig::from_config(&config, "authority".into()).unwrap();
        resolved.allow = vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: "docs.test".into(),
            ports: None,
        })];
        let stores = crate::session_network::tests::Stores::new(&["ses-1"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let dir = SecureDir::open_or_create_all(root.join("service")).unwrap();
        Arc::new(BrowserService::new(
            resolved,
            dir.child("supervisors").unwrap(),
            Vec::new(),
            records,
            Arc::new(FixedPorts(vec![8765])),
            Arc::new(source),
        ))
    }

    async fn session_decision_point(
        browser: Option<(Vec<EgressAllowYaml>, Vec<String>)>,
    ) -> Arc<SessionEgress> {
        use crate::session_egress::tests::{FakeRecord, FakeResolver};
        SessionEgress::open(
            "ses-1",
            EgressPolicyConfig {
                session_allow: vec![EgressAllowYaml::Preset("npm".into())],
                session_private: Vec::new(),
                browser,
            },
            Arc::new(FakeRecord::default()),
            FakeResolver::with(&[]),
            None,
        )
        .await
        .unwrap()
    }

    /// Gap 2: under `network: egress` the browser's declared hosts are
    /// answered by the Session's own decision point and sidecar, never by a
    /// second sidecar of the browser's.
    #[tokio::test]
    async fn under_egress_declared_hosts_use_the_sessions_own_decision_point() {
        let root = tempfile::tempdir().unwrap();
        let docs = vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: "docs.test".into(),
            ports: None,
        })];
        let egress = session_decision_point(Some((docs.clone(), Vec::new()))).await;
        let service = egress_service(
            root.path(),
            FixedEgress {
                egress: Some(egress.clone()),
                ready: true,
            },
        );
        assert!(browser_refusal(&service.config).is_none());
        assert!(service.declared_hosts());
        let session = service.session("ses-1").await;
        let found = service.ensure_egress("ses-1", &session).await.unwrap();
        assert!(Arc::ptr_eq(&found, &egress));
        // No browser-only decision point or sidecar was made for it.
        assert!(session.egress.lock().await.is_none());
        let (sidecar, policies) = service.network_view("ses-1").await;
        assert!(sidecar.is_none() && policies.is_empty());

        // With the Session's sidecar down, the call is refused, and nothing
        // starts a second one.
        let stopped = egress_service(
            root.path(),
            FixedEgress {
                egress: Some(egress.clone()),
                ready: false,
            },
        );
        let session = stopped.session("ses-1").await;
        let error = stopped.ensure_egress("ses-1", &session).await.unwrap_err();
        assert!(error.contains("egress proxy is not running"), "{error}");
        assert!(session.egress.lock().await.is_none());

        // A decision point opened without the browser's scope refuses too.
        let without = egress_service(
            root.path(),
            FixedEgress {
                egress: Some(session_decision_point(None).await),
                ready: true,
            },
        );
        let session = without.session("ses-1").await;
        let error = without.ensure_egress("ses-1", &session).await.unwrap_err();
        assert!(error.contains("no browser policy"), "{error}");
    }

    /// A reload changes which hosts later calls declare; under `bridge`
    /// and `none` it also reloads each browser-only decision point.
    #[tokio::test]
    async fn a_reload_replaces_the_declared_hosts_and_reloads_browser_only_decision_points() {
        use crate::session_egress::tests::{FakeRecord, FakeResolver};
        let root = tempfile::tempdir().unwrap();
        let stores = crate::session_network::tests::Stores::new(&["ses-1"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let service = service(root.path(), records, Vec::new());
        assert!(!service.declared_hosts());
        let docs = vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: "docs.test".into(),
            ports: None,
        })];
        let reloaded = service
            .reload_declared(docs.clone(), Vec::new(), "human")
            .await;
        assert_eq!(reloaded, PointsReload::default());
        assert!(service.declared_hosts());
        assert_eq!(service.declared(), (docs.clone(), Vec::new()));

        // A browser-only decision point that is running records the change.
        let record = Arc::new(FakeRecord::default());
        let authority = SessionEgress::open_browser_only(
            "ses-1",
            EgressPolicyConfig {
                session_allow: Vec::new(),
                session_private: Vec::new(),
                browser: Some((docs.clone(), Vec::new())),
            },
            record.clone(),
            FakeResolver::with(&[]),
        )
        .await
        .unwrap();
        let fonts = vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: "fonts.test".into(),
            ports: None,
        })];
        let browser_only = |allow: &[EgressAllowYaml]| EgressPolicyConfig {
            session_allow: Vec::new(),
            session_private: Vec::new(),
            browser: Some((allow.to_vec(), Vec::new())),
        };
        let reloaded = reload_points(
            vec![("ses-1".to_string(), authority.clone(), browser_only(&fonts))],
            "human",
        )
        .await;
        assert!(reloaded.failed.is_empty(), "{:?}", reloaded.failed);
        assert_eq!(reloaded.lagging, ["browser.allow"]);
        let revisions = reloaded.revisions;
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            (
                revisions[0].session_id.as_str(),
                revisions[0].scope.as_str(),
                revisions[0].revision
            ),
            ("ses-1", "browser", 2)
        );
        let policy = authority.policy(EgressScope::Browser).unwrap();
        assert!(policy.match_name("fonts.test", 443).is_some());
        assert!(policy.match_name("docs.test", 443).is_none());
        // A browser-only decision point has no Session scope to reload.
        assert!(authority.policy(EgressScope::Session).is_none());
        assert!(record.events().iter().any(|event| matches!(
            event,
            NetworkEvent::Policy {
                scope: EgressScope::Browser,
                source: axocoatl_session::network_record::PolicySource::ConfigReload,
                revision: 2,
                ..
            }
        )));
        // Lists it has applied already leave it alone and record nothing.
        let lines = record.events().len();
        let again = reload_points(
            vec![("ses-1".to_string(), authority.clone(), browser_only(&fonts))],
            "human",
        )
        .await;
        assert_eq!(again, PointsReload::default());
        assert_eq!(record.events().len(), lines);
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
            attempt: false,
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
            error: None,
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
            error: None,
        };
        assert!(runner.record(&check, &report).await.unwrap().is_none());
        let page = records.read_after("ses-1", None, 100).await.unwrap();
        assert!(matches!(
            &page.events.last().unwrap().event,
            NetworkEvent::Browser { tool: RecordedTool::BrowserCheck, test_path: Some(path), check_status: Some(status), screenshot_dropped: Some(dropped), .. }
                if path == "qa/b07.spec.ts" && status == "failed" && dropped == "too_large"
        ));
    }

    #[tokio::test]
    async fn an_attempt_lane_is_refused_before_anything_starts_and_still_recorded() {
        let root = tempfile::tempdir().unwrap();
        let stores = crate::session_network::tests::Stores::new(&["ses-1"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let service = service(root.path(), records.clone(), Vec::new());
        let host = BrowserHostTool::browser(service.clone());
        assert_eq!(host.attempt_refusal().as_deref(), Some(ATTEMPT_REFUSAL));
        assert_eq!(
            BrowserHostTool::browser_check(service.clone())
                .attempt_refusal()
                .as_deref(),
            Some(ATTEMPT_REFUSAL)
        );
        let mut bound = context(None);
        bound.attempt = true;
        let error = host
            .bind(bound)
            .execute(serde_json::json!({"url": "http://localhost:8765/"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Ways attempt"), "{error}");
        // No browser state was created for the Session, and the refused call
        // is in the record with its reason.
        assert!(service.sessions.lock().await.is_empty());
        let page = records.read_after("ses-1", None, 100).await.unwrap();
        assert!(matches!(
            &page.events.last().unwrap().event,
            NetworkEvent::Browser { ok: false, error: Some(reason), screenshot: None, .. }
                if reason.contains("Ways attempt")
        ));
    }

    #[test]
    fn browser_check_is_not_for_read_only_agents() {
        let root = tempfile::tempdir().unwrap();
        let stores = crate::session_network::tests::Stores::new(&["ses-1"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let service = service(root.path(), records, Vec::new());
        let profile = |writes: Option<Vec<String>>| ExecutionProfile {
            definition: "qa-scout".into(),
            provider: "ollama".into(),
            model: "m".into(),
            isolation: "in-process".into(),
            tools: vec!["browser".into(), "browser_check".into()],
            write_scope: writes,
        };
        let check = BrowserHostTool::browser_check(service.clone());
        let browse = BrowserHostTool::browser(service);
        assert_eq!(
            check.refusal(&profile(Some(Vec::new()))).as_deref(),
            Some(READ_ONLY_CHECK_REFUSAL)
        );
        assert!(check.refusal(&profile(None)).is_none());
        assert!(check
            .refusal(&profile(Some(vec!["qa/**".into()])))
            .is_none());
        assert!(browse.refusal(&profile(Some(Vec::new()))).is_none());
        // A helper or reviewer that lists it cannot take read-only work.
        let tools = vec![
            "read_file".to_string(),
            "browser".into(),
            "browser_check".into(),
        ];
        assert_eq!(
            crate::session_dispatch::changing_tools(&tools, Some(&[])),
            vec!["browser_check"]
        );
        assert!(crate::session_dispatch::changing_tools(&tools[..2], Some(&[])).is_empty());
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

    /// Spec C.6 case 3 against the real decision point, its sidecar and the
    /// browser container: a declared host is reached with the call's own
    /// credential, and the record has its bind, open, close, unbind and
    /// browser events under the call's browser binding. Ignored by default:
    ///
    /// ```text
    /// CONTAINER_CONNECTION=<connection> cargo test -p axocoatl-daemon --lib \
    ///     actual_declared_host_call -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires Podman, the browser image and docker.io/library/node:20-slim"]
    async fn actual_declared_host_call_is_bound_opened_closed_and_recorded() {
        use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
        use axocoatl_session::network_record::{BindingKind, Decision, UnbindReason};
        use sha2::Digest;

        async fn podman(args: &[&str]) -> std::process::Output {
            tokio::process::Command::new("podman")
                .args(args)
                .output()
                .await
                .unwrap()
        }
        let pid = std::process::id();
        let label = ("io.axocoatl.test".to_string(), format!("browser-{pid}"));
        let label_arg = format!("{}={}", label.0, label.1);
        let octet = pid % 200;
        let subnet = format!("10.90.{octet}.0/24");
        let upstream_ip = format!("10.90.{octet}.10");
        let network = format!("axo-browser-daemon-test-{pid}");
        let upstream = format!("axo-browser-daemon-upstream-{pid}");
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let session_id = format!("brw-daemon-{}", &unique[..12]);
        let authority = format!("{:x}", sha2::Sha256::digest(unique.as_bytes()));
        let root = tempfile::Builder::new()
            .prefix("axo-browser-daemon-")
            .tempdir()
            .unwrap();
        let dir = SecureDir::open(root.path().canonicalize().unwrap()).unwrap();
        let workspace = dir.child("workspace").unwrap();
        let supervisors = dir.child("supervisors").unwrap();

        for args in [
            vec!["network", "create", "--subnet", &subnet, "--label", &label_arg, &network],
            vec![
                "run", "-d", "--name", &upstream, "--label", &label_arg, "--network", &network,
                "--ip", &upstream_ip, "docker.io/library/node:20-slim", "node", "-e",
                "require('http').createServer((q,s)=>{s.writeHead(200,{'content-type':'text/html'});s.end('<title>Upstream</title><h1>declared host</h1>')}).listen(8000,'0.0.0.0')",
            ],
        ] {
            let output = podman(&args).await;
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        let sandbox = SessionSandbox::start(
            &session_id,
            workspace.path(),
            Some("docker.io/library/node:20-slim"),
            &[8765],
            &[],
            &SandboxPolicy {
                network: SandboxNetwork::Bridge,
                runtime_authority: Some(authority.clone()),
                supervisor_installation: Some(supervisors.clone()),
                ..SandboxPolicy::default()
            },
        )
        .await
        .expect("the Session container starts");

        let stores = crate::session_network::tests::Stores::new(&[session_id.as_str()]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let config = axocoatl_config::AxocoatlConfig {
            browser: Some(axocoatl_config::BrowserConfigYaml::default()),
            ..Default::default()
        };
        let mut resolved = BrowserServiceConfig::from_config(&config, authority.clone()).unwrap();
        resolved.allow = vec![EgressAllowYaml::Cidr(axocoatl_config::EgressCidrYaml {
            cidr: subnet.clone(),
            ports: Some(vec![8000]),
        })];
        resolved.private_destinations = vec![subnet.clone()];
        resolved.sidecar_network = Some(network.clone());
        resolved.labels = vec![label.clone()];
        let service = Arc::new(BrowserService::new(
            resolved,
            supervisors,
            Vec::new(),
            records.clone(),
            Arc::new(FixedPorts(vec![8765])),
            no_session_egress(),
        ));
        let mut call = context(None);
        call.session_id = session_id.clone();
        call.activation.session_id = SessionId::new(&session_id).unwrap();
        let runner = Arc::new(BrowserCallRunner {
            service: service.clone(),
            context: call,
            token: Mutex::new(None),
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(240),
            BrowserTool::with_runner(runner.clone())
                .execute(serde_json::json!({"url": format!("http://{upstream_ip}:8000/")})),
        )
        .await;
        let (sidecar, policies) = service.network_view(&session_id).await;
        let page = records.read_after(&session_id, None, 1000).await;
        service.forget(&session_id).await;
        records.close(&session_id).await;
        let stopped = sandbox.stop_checked().await;
        let removed = SessionSandbox::remove_named_with_dependencies(&session_id).await;
        let _ = podman(&["rm", "--force", "--time", "0", "--ignore", &upstream]).await;
        let _ = podman(&["network", "rm", "--force", &network]).await;
        stopped.unwrap();
        removed.unwrap();

        let result = result
            .expect("the call finishes")
            .expect("the call succeeds");
        assert_eq!(result["ok"], true, "{result:#}");
        assert_eq!(result["title"], "Upstream", "{result:#}");
        let sidecar = sidecar.expect("the browser's sidecar is in the network view");
        assert_eq!(sidecar.state, "ready");
        assert_eq!(sidecar.generation, 1);
        assert_eq!(
            policies
                .iter()
                .map(|policy| policy.scope.as_str())
                .collect::<Vec<_>>(),
            vec!["browser"]
        );
        let events: Vec<NetworkEvent> = page
            .unwrap()
            .events
            .into_iter()
            .map(|line| line.event)
            .collect();
        let tag = runner
            .token
            .lock()
            .unwrap()
            .clone()
            .expect("the call took a credential");
        let bound = |binding: &Option<axocoatl_session::network_record::EgressBinding>| {
            binding.as_ref().is_some_and(|binding| {
                binding.kind == BindingKind::Browser
                    && binding.invocation_id.as_deref() == Some("inv-browser-1")
                    && binding.activation_id.as_deref() == Some("act-1")
                    && binding.agent.as_deref() == Some("qa-scout")
            })
        };
        assert!(events.iter().any(|event| matches!(event,
            NetworkEvent::Bind { token, binding, .. } if *token == tag && bound(&Some(binding.clone())))), "{events:#?}");
        let allowed: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                NetworkEvent::Open {
                    conn,
                    decision: Decision::Allow,
                    host,
                    port: 8000,
                    token,
                    binding,
                    ..
                } if *host == upstream_ip
                    && token.as_deref() == Some(tag.as_str())
                    && bound(binding) =>
                {
                    Some(conn)
                }
                _ => None,
            })
            .collect();
        assert!(!allowed.is_empty(), "{events:#?}");
        assert!(
            events.iter().any(|event| matches!(event,
            NetworkEvent::Close { conn, down, .. } if allowed.contains(&conn) && *down > 0)),
            "{events:#?}"
        );
        assert!(
            events.iter().any(|event| matches!(event,
            NetworkEvent::Unbind { token, reason: UnbindReason::BrowserDone } if *token == tag)),
            "{events:#?}"
        );
        assert!(events.iter().any(|event| matches!(event,
            NetworkEvent::Browser { ok: true, token: Some(token), error: None, .. } if *token == tag)), "{events:#?}");
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
        let mut config = axocoatl_config::AxocoatlConfig {
            browser: Some(axocoatl_config::BrowserConfigYaml::default()),
            ..Default::default()
        };
        let resolved = BrowserServiceConfig::from_config(&config, "a".into()).unwrap();
        assert!(browser_refusal(&resolved).is_none());
        let mut e2b = resolved.clone();
        e2b.backend = "e2b".into();
        assert!(browser_refusal(&e2b).unwrap().contains("Podman"));
        // Under network: egress, declared hosts go through the Session's own
        // decision point, so they are no reason to refuse the tools.
        config.sandbox.network = "egress".into();
        let mut egress = BrowserServiceConfig::from_config(&config, "a".into()).unwrap();
        assert!(browser_refusal(&egress).is_none());
        egress.allow = vec![axocoatl_config::EgressAllowYaml::Preset("npm".into())];
        assert!(browser_refusal(&egress).is_none());
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
        assert!(resolved.allow.is_empty());
        assert_eq!(resolved.settings.timeout_secs, 120);
    }
}
