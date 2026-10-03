//! The egress policy decision point for one Session.
//!
//! [`SessionEgress`] mints the credentials processes present to the egress
//! proxy, decides each proxied request, and writes every decision to the
//! Session's network record before answering. A host name is checked against
//! the allowlist before anything resolves it; allowed names are resolved here,
//! on the host, and every resulting address is classified. Only addresses
//! that pass are returned to the sidecar, which connects to nothing else.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axocoatl_config::AxocoatlConfig;
use axocoatl_config::EgressAllowYaml;
use axocoatl_core::netaddr::{self, AddrClass};
use axocoatl_core::SecureDir;
use axocoatl_exec::egress::protocol::{credential_hash, credential_tag, MAX_ALLOW_ADDRS};
use axocoatl_isolation::egress::{
    CloseReport, Decision, EgressAuthority, EgressGrant, GrantKind, GrantSpec, Liveness,
    OpenRequest, ProxySecret, RequestKind, SidecarEvent,
};
use axocoatl_isolation::egress_control::ControlHandle;
use axocoatl_session::network_record::{
    BindingKind, CloseOutcome, ConnKind, Decision as RecordDecision, EgressBinding, EgressScope,
    LimitKind, NetworkEvent, NetworkLine, PolicyChange, PolicyOp, PolicySource, SidecarState,
    UnbindReason, MAX_RECORDED_PATH_CHARS,
};

use crate::session_egress_policy::{validate_session_host, CompiledPolicy, SessionRule};
use crate::session_network::{PolicyRuleView, PolicyView, SessionNetworkRecords};

/// How long one name may take to resolve.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Where processes reach the proxy inside the container.
pub const PROXY_LISTEN: &str = "127.0.0.1:3128";
/// Proxy user name; the credential is the password.
pub const PROXY_USER: &str = "axo";
const NO_PROXY: &str = "localhost,127.0.0.1,::1";
/// Refusals of connections without a valid credential (`no_credential`,
/// `unknown_credential`) recorded one by one before the rate below applies.
/// Any process in the container can make them, so they must not fill the
/// record.
pub const UNATTRIBUTED_REFUSAL_BURST: u32 = 20;
/// After the burst, one such refusal is recorded per interval.
pub const UNATTRIBUTED_REFUSAL_INTERVAL: Duration = Duration::from_secs(5);
/// The ones not recorded are counted in one `limit` event per interval.
pub const UNRECORDED_REFUSALS_INTERVAL: Duration = Duration::from_secs(60);

/// Resolves allowed names on the host.
#[async_trait::async_trait]
pub trait EgressResolver: Send + Sync + fmt::Debug {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String>;
}

/// The host's resolver, with a 5 s limit.
#[derive(Debug, Default)]
pub struct SystemResolver;

#[async_trait::async_trait]
impl EgressResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        let found = tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| "resolution timed out".to_string())?
            .map_err(|error| error.to_string())?;
        Ok(found.map(|address| address.ip()).collect())
    }
}

/// Why a record write failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordFailure {
    Full,
    Unavailable(String),
}

/// Where a Session's egress events go.
#[async_trait::async_trait]
pub trait EgressRecordSink: Send + Sync + fmt::Debug {
    async fn append(&self, event: NetworkEvent) -> Result<u64, RecordFailure>;
    async fn append_control(&self, event: NetworkEvent) -> Result<u64, RecordFailure>;
    /// Every recorded line, for replaying per-Session policy changes.
    async fn history(&self) -> Result<Vec<NetworkLine>, RecordFailure>;
}

/// The daemon's sink: one Session's network record.
#[derive(Debug, Clone)]
pub struct SessionRecordSink {
    records: Arc<SessionNetworkRecords>,
    session: String,
}

impl SessionRecordSink {
    pub fn new(records: Arc<SessionNetworkRecords>, session: impl Into<String>) -> Self {
        Self {
            records,
            session: session.into(),
        }
    }
}

fn record_failure(error: crate::session_network::RecordServiceError) -> RecordFailure {
    if error.is_full() {
        RecordFailure::Full
    } else {
        RecordFailure::Unavailable(error.to_string())
    }
}

#[async_trait::async_trait]
impl EgressRecordSink for SessionRecordSink {
    async fn append(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        self.records
            .append(&self.session, event)
            .await
            .map_err(record_failure)
    }

    async fn append_control(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        self.records
            .append_control(&self.session, event)
            .await
            .map_err(record_failure)
    }

    async fn history(&self) -> Result<Vec<NetworkLine>, RecordFailure> {
        let mut lines = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .records
                .read_after(
                    &self.session,
                    after,
                    axocoatl_session::network_record::MAX_READ_LIMIT,
                )
                .await
                .map_err(record_failure)?;
            if page.events.is_empty() {
                return Ok(lines);
            }
            after = page.events.last().map(|line| line.seq);
            lines.extend(page.events);
        }
    }
}

/// The allowlists a Session's policies compile from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicyConfig {
    pub session_allow: Vec<EgressAllowYaml>,
    pub session_private: Vec<String>,
    /// `None` when the browser tool is not configured.
    pub browser: Option<(Vec<EgressAllowYaml>, Vec<String>)>,
}

impl EgressPolicyConfig {
    pub fn from_config(config: &AxocoatlConfig) -> Self {
        let egress = config.sandbox.egress.clone().unwrap_or_default();
        Self {
            session_allow: egress.allow,
            session_private: egress.private_destinations,
            browser: config
                .browser
                .as_ref()
                .map(|browser| (browser.allow.clone(), browser.private_destinations.clone())),
        }
    }
}

/// A refused per-Session allow or revoke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressPolicyError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
}

struct ScopeState {
    policy: Arc<CompiledPolicy>,
    session_rules: Vec<SessionRule>,
    revision: u64,
}

struct Binding {
    tag: String,
    binding: EgressBinding,
    scope: EgressScope,
    kind: GrantKind,
    liveness: Option<Liveness>,
    env_file: Option<String>,
}

struct OpenConnection {
    token_hash: Option<String>,
    rule_id: String,
    scope: EgressScope,
}

/// Rate limit for recording refusals of connections without a valid
/// credential: a burst, then one per interval; the rest are counted.
struct UnattributedRefusals {
    tokens: u32,
    refilled: tokio::time::Instant,
    unrecorded: u64,
    summary_scheduled: bool,
}

impl Default for UnattributedRefusals {
    fn default() -> Self {
        Self {
            tokens: UNATTRIBUTED_REFUSAL_BURST,
            refilled: tokio::time::Instant::now(),
            unrecorded: 0,
            summary_scheduled: false,
        }
    }
}

impl UnattributedRefusals {
    /// Whether to record this refusal one by one.
    fn admit(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        let interval = UNATTRIBUTED_REFUSAL_INTERVAL.as_millis().max(1);
        let earned = now.duration_since(self.refilled).as_millis() / interval;
        if earned > 0 {
            let earned = u32::try_from(earned).unwrap_or(u32::MAX);
            self.tokens = self
                .tokens
                .saturating_add(earned)
                .min(UNATTRIBUTED_REFUSAL_BURST);
            self.refilled = if self.tokens == UNATTRIBUTED_REFUSAL_BURST {
                now
            } else {
                self.refilled + UNATTRIBUTED_REFUSAL_INTERVAL * earned
            };
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            self.unrecorded += 1;
            false
        }
    }
}

#[derive(Default)]
struct State {
    scopes: HashMap<EgressScope, ScopeState>,
    bindings: HashMap<String, Binding>,
    open: HashMap<(u32, u64), OpenConnection>,
    control: Option<ControlHandle>,
    commands: HashSet<String>,
    record_full_reported: bool,
    unattributed: UnattributedRefusals,
    /// Addresses refused whatever the policy lists (host gateways).
    forbidden: HashSet<IpAddr>,
    /// The highest sidecar generation in this Session's record.
    last_generation: u32,
}

/// The policy decision point for one Session.
pub struct SessionEgress {
    session_id: String,
    config: EgressPolicyConfig,
    records: Arc<dyn EgressRecordSink>,
    resolver: Arc<dyn EgressResolver>,
    classify: fn(IpAddr) -> AddrClass,
    env_dir: Option<SecureDir>,
    state: Mutex<State>,
    /// Serializes per-Session allows and revokes.
    policy_changes: tokio::sync::Mutex<()>,
    this: Weak<SessionEgress>,
}

impl fmt::Debug for SessionEgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionEgress")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

fn binding_kind(kind: GrantKind) -> BindingKind {
    match kind {
        GrantKind::Agent => BindingKind::Agent,
        GrantKind::Setup => BindingKind::Setup,
        GrantKind::Provisioning => BindingKind::Provisioning,
        GrantKind::Terminal => BindingKind::Terminal,
        GrantKind::Browser => BindingKind::Browser,
    }
}

fn grant_scope(kind: GrantKind) -> EgressScope {
    match kind {
        GrantKind::Agent | GrantKind::Setup | GrantKind::Terminal => EgressScope::Session,
        GrantKind::Provisioning => EgressScope::Provisioning,
        GrantKind::Browser => EgressScope::Browser,
    }
}

fn unbind_reason(kind: GrantKind) -> UnbindReason {
    match kind {
        GrantKind::Agent => UnbindReason::Settled,
        GrantKind::Setup => UnbindReason::SetupDone,
        GrantKind::Provisioning => UnbindReason::ProvisioningDone,
        GrantKind::Terminal => UnbindReason::TerminalClosed,
        GrantKind::Browser => UnbindReason::BrowserDone,
    }
}

/// `axe_` and 32 random bytes, base64url.
fn mint_token() -> Result<String, String> {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("no randomness for an egress credential: {error}"))?;
    Ok(format!(
        "axe_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

/// The env file a supervised process gets under `network: egress`.
pub fn env_file_contents(token: &str) -> String {
    let url = format!("http://{PROXY_USER}:{token}@{PROXY_LISTEN}");
    let mut contents = String::new();
    for name in [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "https_proxy",
        "http_proxy",
        "npm_config_proxy",
        "npm_config_https_proxy",
    ] {
        contents.push_str(&format!("{name}={url}\n"));
    }
    contents.push_str(&format!("NO_PROXY={NO_PROXY}\nno_proxy={NO_PROXY}\n"));
    contents.push_str("NODE_USE_ENV_PROXY=1\n");
    contents
}

fn hint(reason: &str, host: &str, port: u16) -> String {
    match reason {
        "not_allowed" => format!(
            "{host}:{port} is not in this Session's egress allowlist. Ask the user to allow it for this Session in Session network or to add it under sandbox.egress.allow."
        ),
        "private_destination" => format!(
            "{host} resolves to a private address. Axocoatl refuses private addresses unless the user lists the range under sandbox.egress.private_destinations."
        ),
        "forbidden_destination" => format!(
            "{host} resolves to a loopback, link-local, host-gateway or other special address, which Axocoatl never allows."
        ),
        "no_credential" => "This process has no egress credential. Under network: egress, read-only helpers, required checks and processes started outside a tool call have no network.".into(),
        "unknown_credential" => "This credential is not valid here; the tool call, setup step or terminal it belonged to has ended.".into(),
        "binding_ended" => "The terminal or tool call this credential belonged to has ended.".into(),
        "record_unavailable" => "The Session's network record is full or unavailable, so new connections are refused.".into(),
        "invalid_host" => format!("{host} is not a valid host name or IP address."),
        "resolve_failed" => format!("{host} could not be resolved."),
        _ => "Axocoatl refused this connection.".into(),
    }
}

/// The sidecar generation an event belongs to, if it names one.
fn recorded_generation(event: &NetworkEvent) -> Option<u32> {
    match event {
        NetworkEvent::Sidecar { generation, .. } => Some(*generation),
        NetworkEvent::Open { conn, .. } | NetworkEvent::Close { conn, .. } => conn
            .strip_prefix('g')
            .and_then(|rest| rest.split_once(':'))
            .and_then(|(generation, _)| generation.parse().ok()),
        _ => None,
    }
}

fn bounded_host(host: &str) -> String {
    let host: String = host.chars().filter(|c| !c.is_control()).take(253).collect();
    if host.is_empty() {
        "-".into()
    } else {
        host
    }
}

struct Verdict {
    decision: Decision,
    reason: Option<&'static str>,
    status: Option<u16>,
    rule: Option<String>,
    addrs: Vec<IpAddr>,
    revision: Option<u64>,
}

impl Verdict {
    fn deny(status: u16, reason: &'static str, host: &str, port: u16) -> Self {
        Self {
            decision: Decision::deny(status, reason, hint(reason, host, port)),
            reason: Some(reason),
            status: Some(status),
            rule: None,
            addrs: Vec::new(),
            revision: None,
        }
    }
}

struct Caller {
    hash: Option<String>,
    tag: Option<String>,
    binding: Option<EgressBinding>,
    scope: Option<EgressScope>,
}

impl SessionEgress {
    /// Create the decision point, replaying per-Session allows and revokes
    /// from the record and recording each scope's policy when it changed.
    pub async fn open(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
        env_dir: Option<SecureDir>,
    ) -> Result<Arc<Self>, String> {
        Self::open_with_classifier(
            session_id,
            config,
            records,
            resolver,
            env_dir,
            netaddr::classify,
        )
        .await
    }

    pub(crate) async fn open_with_classifier(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
        env_dir: Option<SecureDir>,
        classify: fn(IpAddr) -> AddrClass,
    ) -> Result<Arc<Self>, String> {
        let history = records
            .history()
            .await
            .map_err(|error| format!("reading the network record: {error:?}"))?;
        // Command ids already applied, so a resend after a restart is refused.
        let commands: HashSet<String> = history
            .iter()
            .filter_map(|line| match &line.event {
                NetworkEvent::Policy {
                    change: Some(change),
                    ..
                } => change.command_id.clone(),
                _ => None,
            })
            .collect();
        let mut scopes = vec![EgressScope::Session, EgressScope::Provisioning];
        if config.browser.is_some() {
            scopes.push(EgressScope::Browser);
        }
        let mut states = HashMap::new();
        for scope in scopes {
            let mut session_rules: Vec<SessionRule> = Vec::new();
            let mut revision = 0;
            let mut digest = None;
            for line in &history {
                let NetworkEvent::Policy {
                    scope: event_scope,
                    revision: event_revision,
                    digest: event_digest,
                    source,
                    change,
                    ..
                } = &line.event
                else {
                    continue;
                };
                if *event_scope != scope {
                    continue;
                }
                revision = *event_revision;
                digest = Some(event_digest.clone());
                match (source, change) {
                    (PolicySource::SessionAllow, Some(change)) if change.op == PolicyOp::Allow => {
                        session_rules.push(SessionRule {
                            revision: *event_revision,
                            host: change.host.clone(),
                            ports: change.ports.clone(),
                        })
                    }
                    (PolicySource::SessionRevoke, Some(change))
                        if change.op == PolicyOp::Revoke =>
                    {
                        session_rules.retain(|rule| rule.host != change.host)
                    }
                    _ => {}
                }
            }
            let policy = Self::compile_scope(&config, scope, &session_rules)?;
            if digest.as_deref() != Some(policy.digest()) {
                revision += 1;
                records
                    .append_control(NetworkEvent::Policy {
                        scope,
                        revision,
                        digest: policy.digest().to_string(),
                        source: PolicySource::Config,
                        rules: policy.rendered(),
                        change: None,
                        actor: None,
                    })
                    .await
                    .map_err(|error| format!("recording the egress policy: {error:?}"))?;
            }
            states.insert(
                scope,
                ScopeState {
                    policy: Arc::new(policy),
                    session_rules,
                    revision,
                },
            );
        }
        let session_id = session_id.into();
        let last_generation = history
            .iter()
            .filter_map(|line| recorded_generation(&line.event))
            .max()
            .unwrap_or(0);
        Ok(Arc::new_cyclic(|this| Self {
            session_id,
            config,
            records,
            resolver,
            classify,
            env_dir,
            state: Mutex::new(State {
                scopes: states,
                commands,
                last_generation,
                forbidden: axocoatl_config::egress::HOST_GATEWAYS
                    .iter()
                    .map(|gateway| IpAddr::V4(*gateway))
                    .collect(),
                ..State::default()
            }),
            policy_changes: tokio::sync::Mutex::new(()),
            this: this.clone(),
        }))
    }

    fn compile_scope(
        config: &EgressPolicyConfig,
        scope: EgressScope,
        session_rules: &[SessionRule],
    ) -> Result<CompiledPolicy, String> {
        match scope {
            EgressScope::Session => CompiledPolicy::compile(
                scope,
                &config.session_allow,
                &config.session_private,
                session_rules,
            ),
            EgressScope::Provisioning => Ok(CompiledPolicy::provisioning()),
            EgressScope::Browser => {
                let (allow, private) = config.browser.clone().unwrap_or_default();
                CompiledPolicy::compile(scope, &allow, &private, session_rules)
            }
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Use this control channel for revokes. Each sidecar generation attaches
    /// its own.
    pub fn attach_control(&self, handle: ControlHandle) {
        self.state().control = Some(handle);
    }

    /// The compiled policy of `scope`, if that scope exists.
    pub fn policy(&self, scope: EgressScope) -> Option<Arc<CompiledPolicy>> {
        self.state()
            .scopes
            .get(&scope)
            .map(|state| state.policy.clone())
    }

    /// Policies for the network view.
    pub fn policy_views(&self) -> Vec<PolicyView> {
        let state = self.state();
        let mut views: Vec<PolicyView> = state
            .scopes
            .iter()
            .map(|(scope, scope_state)| PolicyView {
                scope: scope.as_str().to_string(),
                revision: scope_state.revision,
                digest: scope_state.policy.digest().to_string(),
                rules: scope_state
                    .policy
                    .rules()
                    .iter()
                    .map(|rule| PolicyRuleView {
                        id: rule.id.clone(),
                        text: rule.text.clone(),
                        source: rule.source.as_str().to_string(),
                    })
                    .collect(),
            })
            .collect();
        views.sort_by(|left, right| left.scope.cmp(&right.scope));
        views
    }

    /// Number of live credentials, for tests and status.
    pub fn live_bindings(&self) -> usize {
        self.state().bindings.len()
    }

    async fn record_open(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        let result = self.records.append(event).await;
        if result == Err(RecordFailure::Full) {
            self.report_record_full().await;
        }
        result
    }

    /// Say once, in the control headroom, that the record reached its cap.
    async fn report_record_full(&self) {
        let report = {
            let mut state = self.state();
            !std::mem::replace(&mut state.record_full_reported, true)
        };
        if report {
            let _ = self
                .records
                .append_control(NetworkEvent::Limit {
                    what: LimitKind::RecordFull,
                    detail: "the network record reached its cap; new connections are refused"
                        .into(),
                })
                .await;
        }
    }

    /// Whether to record a refusal of a connection without a valid
    /// credential one by one. The ones that are not are summed up in a
    /// `limit` event at most once per [`UNRECORDED_REFUSALS_INTERVAL`].
    fn admit_unattributed_refusal(&self) -> bool {
        let schedule = {
            let mut state = self.state();
            if state.unattributed.admit() {
                return true;
            }
            !std::mem::replace(&mut state.unattributed.summary_scheduled, true)
        };
        if schedule {
            let this = self.this.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        tokio::time::sleep(UNRECORDED_REFUSALS_INTERVAL).await;
                        if let Some(egress) = this.upgrade() {
                            egress.record_unrecorded_refusals().await;
                        }
                    });
                }
                Err(_) => self.state().unattributed.summary_scheduled = false,
            }
        }
        false
    }

    /// Record how many refusals were counted but not recorded one by one.
    async fn record_unrecorded_refusals(&self) {
        let count = {
            let mut state = self.state();
            state.unattributed.summary_scheduled = false;
            std::mem::take(&mut state.unattributed.unrecorded)
        };
        if count == 0 {
            return;
        }
        let event = NetworkEvent::Limit {
            what: LimitKind::UnrecordedRefusals,
            detail: format!(
                "{count} connection(s) without a valid credential were refused and not recorded one by one; at most {UNATTRIBUTED_REFUSAL_BURST} at once, then one every {} s, are",
                UNATTRIBUTED_REFUSAL_INTERVAL.as_secs()
            ),
        };
        if let Err(error) = self.records.append(event).await {
            tracing::warn!(session = %self.session_id, ?error, "recording the unrecorded egress refusals failed");
        }
    }

    /// Note a sidecar generation seen in this Session.
    fn note_generation(&self, generation: u32) {
        let mut state = self.state();
        state.last_generation = state.last_generation.max(generation);
    }

    /// Remove one binding now: revoke its open connections and delete its env
    /// file. Returns the unbind event to record, if the binding existed.
    fn release(&self, hash: &str, reason: UnbindReason) -> Option<NetworkEvent> {
        let (binding, ids, control) = {
            let mut state = self.state();
            let binding = state.bindings.remove(hash)?;
            let generation = state.control.as_ref().map(ControlHandle::generation);
            let mut ids: Vec<u64> = state
                .open
                .iter()
                .filter(|((connection_generation, _), connection)| {
                    Some(*connection_generation) == generation
                        && connection.token_hash.as_deref() == Some(hash)
                })
                .map(|((_, id), _)| *id)
                .collect();
            ids.sort_unstable();
            (binding, ids, state.control.clone())
        };
        if let (Some(control), false) = (control, ids.is_empty()) {
            control.revoke(ids);
        }
        if let (Some(dir), Some(file)) = (&self.env_dir, &binding.env_file) {
            if let Err(error) = dir.remove_file(file) {
                tracing::warn!(session = %self.session_id, %error, "removing an egress env file failed");
            }
        }
        Some(NetworkEvent::Unbind {
            token: binding.tag,
            reason,
        })
    }

    /// Remove one binding now and record the unbind in the background.
    fn unbind(&self, hash: &str, reason: UnbindReason) {
        let Some(event) = self.release(hash, reason) else {
            return;
        };
        let records = self.records.clone();
        let session = self.session_id.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Err(error) = records.append_control(event).await {
                        tracing::warn!(session = %session, ?error, "recording an egress unbind failed");
                    }
                });
            }
            Err(_) => {
                tracing::warn!(session = %session, "egress unbind outside a runtime was not recorded")
            }
        }
    }

    /// The Session's runtime stopped: every credential still bound belonged
    /// to processes that no longer exist. Unbind them all, recorded in order.
    async fn unbind_all(&self, reason: UnbindReason) {
        let mut hashes: Vec<String> = self.state().bindings.keys().cloned().collect();
        hashes.sort();
        for hash in hashes {
            if let Some(event) = self.release(&hash, reason) {
                if let Err(error) = self.records.append_control(event).await {
                    tracing::warn!(session = %self.session_id, ?error, "recording an egress unbind failed");
                }
            }
        }
    }

    /// Allow one exact host for this Session (`session` or `browser` scope).
    pub async fn allow(
        &self,
        scope: EgressScope,
        host: &str,
        ports: Option<Vec<u16>>,
        actor: &str,
        command_id: &str,
    ) -> Result<(u64, String), EgressPolicyError> {
        self.change_policy(scope, PolicyOp::Allow, host, ports, actor, command_id)
            .await
    }

    /// Remove this Session's allows for one host and close the connections
    /// they admitted. Config rules are not revocable here.
    pub async fn revoke(
        &self,
        scope: EgressScope,
        host: &str,
        actor: &str,
        command_id: &str,
    ) -> Result<(u64, String), EgressPolicyError> {
        self.change_policy(scope, PolicyOp::Revoke, host, None, actor, command_id)
            .await
    }

    async fn change_policy(
        &self,
        scope: EgressScope,
        op: PolicyOp,
        host: &str,
        ports: Option<Vec<u16>>,
        actor: &str,
        command_id: &str,
    ) -> Result<(u64, String), EgressPolicyError> {
        if scope == EgressScope::Provisioning {
            return Err(EgressPolicyError::Invalid(
                "the provisioning policy cannot be changed for one Session".into(),
            ));
        }
        if command_id.is_empty() || command_id.len() > 128 {
            return Err(EgressPolicyError::Invalid(
                "command_id must be 1-128 characters".into(),
            ));
        }
        let host = validate_session_host(host).map_err(EgressPolicyError::Invalid)?;
        let ports = match op {
            PolicyOp::Allow => axocoatl_config::egress::validate_ports(ports.as_deref())
                .map_err(EgressPolicyError::Invalid)?,
            PolicyOp::Revoke => Vec::new(),
        };
        // One change at a time: compute it, record it, then publish it, so the
        // record never holds a change that did not take effect.
        let _serial = self.policy_changes.lock().await;
        let (revision, policy, session_rules, removed_ids) = {
            let state = self.state();
            if state.commands.contains(command_id) {
                return Err(EgressPolicyError::Conflict(format!(
                    "command {command_id} was already applied"
                )));
            }
            let scope_state = state.scopes.get(&scope).ok_or_else(|| {
                EgressPolicyError::Invalid(format!("this Session has no {} policy", scope.as_str()))
            })?;
            let revision = scope_state.revision + 1;
            let mut session_rules = scope_state.session_rules.clone();
            let mut removed_ids = Vec::new();
            match op {
                PolicyOp::Allow => session_rules.push(SessionRule {
                    revision,
                    host: host.clone(),
                    ports: ports.clone(),
                }),
                PolicyOp::Revoke => {
                    let before = session_rules.len();
                    removed_ids = session_rules
                        .iter()
                        .filter(|rule| rule.host == host)
                        .map(|rule| format!("session#rev{}", rule.revision))
                        .collect();
                    session_rules.retain(|rule| rule.host != host);
                    if session_rules.len() == before {
                        return Err(EgressPolicyError::Invalid(format!(
                            "{host} has no allow for this Session to revoke"
                        )));
                    }
                }
            }
            let policy = Self::compile_scope(&self.config, scope, &session_rules)
                .map_err(EgressPolicyError::Invalid)?;
            (revision, policy, session_rules, removed_ids)
        };
        self.records
            .append_control(NetworkEvent::Policy {
                scope,
                revision,
                digest: policy.digest().to_string(),
                source: match op {
                    PolicyOp::Allow => PolicySource::SessionAllow,
                    PolicyOp::Revoke => PolicySource::SessionRevoke,
                },
                rules: policy.rendered(),
                change: Some(PolicyChange {
                    op,
                    host: host.clone(),
                    ports,
                    command_id: Some(command_id.to_string()),
                }),
                actor: Some(actor.chars().take(128).collect()),
            })
            .await
            .map_err(|error| {
                EgressPolicyError::Unavailable(format!(
                    "recording the policy change failed: {error:?}"
                ))
            })?;
        let digest = policy.digest().to_string();
        let (control, revoke_ids) = {
            let mut state = self.state();
            state.commands.insert(command_id.to_string());
            let scope_state = state.scopes.get_mut(&scope).expect("scope checked above");
            scope_state.revision = revision;
            scope_state.policy = Arc::new(policy);
            scope_state.session_rules = session_rules;
            let generation = state.control.as_ref().map(ControlHandle::generation);
            let mut revoke_ids: Vec<u64> = state
                .open
                .iter()
                .filter(|((connection_generation, _), connection)| {
                    Some(*connection_generation) == generation
                        && connection.scope == scope
                        && removed_ids.contains(&connection.rule_id)
                })
                .map(|((_, id), _)| *id)
                .collect();
            revoke_ids.sort_unstable();
            (state.control.clone(), revoke_ids)
        };
        if let (Some(control), false) = (control, revoke_ids.is_empty()) {
            control.revoke(revoke_ids);
        }
        Ok((revision, digest))
    }

    /// Who presented `auth`, and the refusal when the credential cannot be
    /// used. A credential whose liveness check fails is unbound here.
    fn caller(&self, auth: Option<&str>) -> (Caller, Option<(u16, &'static str)>) {
        let Some(hash) = auth else {
            let nobody = Caller {
                hash: None,
                tag: None,
                binding: None,
                scope: None,
            };
            return (nobody, Some((407, "no_credential")));
        };
        let (found, ended) = {
            let state = self.state();
            match state.bindings.get(hash) {
                None => (None, None),
                Some(binding) => {
                    let alive = binding.liveness.as_ref().is_none_or(|alive| alive());
                    (
                        Some((binding.binding.clone(), binding.scope)),
                        (!alive).then_some(binding.kind),
                    )
                }
            }
        };
        let mut caller = Caller {
            hash: Some(hash.to_string()),
            tag: Some(credential_tag(hash)),
            binding: None,
            scope: None,
        };
        let Some((binding, scope)) = found else {
            return (caller, Some((407, "unknown_credential")));
        };
        caller.binding = Some(binding);
        caller.scope = Some(scope);
        if let Some(kind) = ended {
            self.unbind(hash, unbind_reason(kind));
            return (caller, Some((407, "binding_ended")));
        }
        (caller, None)
    }

    /// Whether `ip` (or the IPv4 address it embeds) is a host gateway.
    fn is_host_gateway(&self, ip: IpAddr) -> bool {
        let embedded = match ip {
            IpAddr::V6(v6) => netaddr::embedded_ipv4(v6).map(IpAddr::V4),
            IpAddr::V4(_) => None,
        };
        let state = self.state();
        state.forbidden.contains(&ip) || embedded.is_some_and(|v4| state.forbidden.contains(&v4))
    }

    async fn verdict(&self, open: &OpenRequest, scope: EgressScope) -> Verdict {
        let Some((policy, revision)) = self
            .state()
            .scopes
            .get(&scope)
            .map(|state| (state.policy.clone(), state.revision))
        else {
            return Verdict::deny(403, "not_allowed", &open.host, open.port);
        };
        let deny = |status, reason| Verdict {
            revision: Some(revision),
            ..Verdict::deny(status, reason, &open.host, open.port)
        };
        let (rule, addrs) = if let Some(ip) = netaddr::parse_ip_literal(&open.host) {
            // A literal needs no resolution, so a never-allowed address is
            // named as such even when no range lists it.
            if (self.classify)(ip).is_forbidden() || self.is_host_gateway(ip) {
                return Verdict {
                    addrs: vec![ip],
                    ..deny(403, "forbidden_destination")
                };
            }
            match policy.match_ip(ip, open.port) {
                Some(rule) => (rule.id.clone(), vec![ip]),
                None => return deny(403, "not_allowed"),
            }
        } else {
            let Ok(name) = netaddr::normalize_host_name(&open.host) else {
                return deny(400, "invalid_host");
            };
            let Some(rule) = policy.match_name(&name, open.port) else {
                return deny(403, "not_allowed");
            };
            let rule = rule.id.clone();
            // Only now, after the allowlist matched, does the name resolve.
            let mut addrs = match self.resolver.resolve(&name, open.port).await {
                Ok(addrs) if !addrs.is_empty() => addrs,
                _ => return deny(502, "resolve_failed"),
            };
            let mut seen = HashSet::new();
            addrs.retain(|addr| seen.insert(*addr));
            addrs.truncate(MAX_ALLOW_ADDRS);
            (rule, addrs)
        };
        let classes: Vec<AddrClass> = addrs.iter().map(|addr| (self.classify)(*addr)).collect();
        if classes.iter().any(|class| class.is_forbidden())
            || addrs.iter().any(|addr| self.is_host_gateway(*addr))
        {
            return Verdict {
                rule: Some(rule),
                addrs,
                ..deny(403, "forbidden_destination")
            };
        }
        if addrs.iter().zip(&classes).any(|(addr, class)| {
            matches!(class, AddrClass::Private(_)) && !policy.allows_private(*addr)
        }) {
            return Verdict {
                rule: Some(rule),
                addrs,
                ..deny(403, "private_destination")
            };
        }
        Verdict {
            decision: Decision::Allow {
                addrs: addrs.clone(),
            },
            reason: None,
            status: None,
            rule: Some(rule),
            addrs,
            revision: Some(revision),
        }
    }
}

struct GrantGuard {
    egress: Weak<SessionEgress>,
    hash: String,
    reason: UnbindReason,
}

impl Drop for GrantGuard {
    fn drop(&mut self) {
        if let Some(egress) = self.egress.upgrade() {
            egress.unbind(&self.hash, self.reason);
        }
    }
}

#[async_trait::async_trait]
impl EgressAuthority for SessionEgress {
    async fn grant(&self, spec: GrantSpec) -> Result<EgressGrant, String> {
        let token = mint_token()?;
        let hash = credential_hash(&token);
        let tag = credential_tag(&hash);
        let scope = grant_scope(spec.kind);
        if !self.state().scopes.contains_key(&scope) {
            return Err(format!(
                "this Session has no {} egress policy",
                scope.as_str()
            ));
        }
        let binding = EgressBinding {
            kind: binding_kind(spec.kind),
            invocation_id: spec.invocation_id.clone(),
            activation_id: spec.activation_id.clone(),
            node_id: spec.node_id.clone(),
            agent: spec.agent.clone(),
            process: spec.process.clone(),
            terminal_id: spec.terminal_id.clone(),
            setup_index: spec.setup_index,
        };
        let env_file = if spec.kind == GrantKind::Browser {
            None
        } else {
            let dir = self
                .env_dir
                .as_ref()
                .ok_or("no directory for egress env files")?;
            let name = format!("egress-{tag}.env");
            dir.atomic_write_with_mode(&name, env_file_contents(&token).as_bytes(), 0o600)
                .map_err(|error| format!("writing the egress env file: {error}"))?;
            Some(name)
        };
        // Record the binding before the credential exists anywhere but its
        // env file, so every connection made with it is attributable.
        if let Err(error) = self
            .records
            .append(NetworkEvent::Bind {
                token: tag.clone(),
                binding: binding.clone(),
                scope,
            })
            .await
        {
            if let (Some(dir), Some(name)) = (&self.env_dir, &env_file) {
                let _ = dir.remove_file(name);
            }
            if error == RecordFailure::Full {
                self.report_record_full().await;
            }
            return Err(format!("recording the egress binding failed: {error:?}"));
        }
        self.state().bindings.insert(
            hash.clone(),
            Binding {
                tag: tag.clone(),
                binding,
                scope,
                kind: spec.kind,
                liveness: spec.liveness.clone(),
                env_file: env_file.clone(),
            },
        );
        let guard = GrantGuard {
            egress: self.this.clone(),
            hash,
            reason: unbind_reason(spec.kind),
        };
        let env_path = env_file
            .as_ref()
            .and_then(|name| self.env_dir.as_ref().map(|dir| dir.path().join(name)));
        let proxy = (spec.kind == GrantKind::Browser)
            .then(|| ProxySecret::new(format!("http://{PROXY_USER}:{token}@{PROXY_LISTEN}")));
        Ok(EgressGrant::new(env_path, tag, proxy, Box::new(guard)))
    }

    async fn decide(&self, open: OpenRequest) -> Decision {
        self.note_generation(open.generation);
        let (caller, refusal) = self.caller(open.auth.as_deref());
        let scope = caller.scope.unwrap_or(EgressScope::Session);
        let mut verdict = match refusal {
            None => self.verdict(&open, scope).await,
            // Without a credential nothing can be recorded once the record
            // is full; say so rather than blame the missing credential (a
            // setup step or terminal goes ahead without one then).
            Some((_, "no_credential")) if self.state().record_full_reported => {
                Verdict::deny(503, "record_unavailable", &open.host, open.port)
            }
            Some((status, reason)) => Verdict::deny(status, reason, &open.host, open.port),
        };
        let key = (open.generation, open.id);
        if matches!(verdict.decision, Decision::Allow { .. }) {
            // The credential may have been released, or the rule revoked,
            // while the name resolved. Check again and register the
            // connection in one step under the lock that release and revoke
            // take, so from here on either of them revokes it. The proxy
            // holds a revoke that overtakes this answer and refuses the
            // connection when the answer arrives.
            let mut state = self.state();
            let bound = caller
                .hash
                .as_ref()
                .is_some_and(|hash| state.bindings.contains_key(hash));
            let current = state.scopes.get(&scope).map(|scope_state| {
                (
                    scope_state.revision,
                    verdict.rule.as_ref().is_some_and(|rule| {
                        scope_state
                            .policy
                            .rules()
                            .iter()
                            .any(|live| &live.id == rule)
                    }),
                )
            });
            if !bound {
                verdict = Verdict::deny(407, "binding_ended", &open.host, open.port);
            } else if let Some((revision, false)) = current {
                verdict = Verdict {
                    revision: Some(revision),
                    ..Verdict::deny(403, "not_allowed", &open.host, open.port)
                };
            } else {
                state.open.insert(
                    key,
                    OpenConnection {
                        token_hash: caller.hash.clone(),
                        rule_id: verdict.rule.clone().unwrap_or_default(),
                        scope,
                    },
                );
            }
        }
        let allowed = matches!(verdict.decision, Decision::Allow { .. });
        let unattributed = matches!(verdict.reason, Some("no_credential" | "unknown_credential"));
        if unattributed && !self.admit_unattributed_refusal() {
            // Counted, not recorded; a refusal stands either way.
            return verdict.decision;
        }
        let path = open
            .path
            .as_ref()
            .map(|path| path.chars().take(MAX_RECORDED_PATH_CHARS).collect());
        let event = NetworkEvent::Open {
            conn: format!("g{}:{}", open.generation, open.id),
            decision: if allowed {
                RecordDecision::Allow
            } else {
                RecordDecision::Deny
            },
            reason: verdict.reason.map(str::to_string),
            status: verdict.status,
            rule: verdict.rule.clone(),
            host: bounded_host(&open.host),
            port: open.port,
            conn_kind: match open.kind {
                RequestKind::Connect => ConnKind::Connect,
                RequestKind::Http => ConnKind::Http,
            },
            method: open.method.clone(),
            path,
            addrs: verdict.addrs.iter().map(ToString::to_string).collect(),
            token: caller.tag.clone(),
            binding: caller.binding.clone(),
            scope: caller.scope,
            policy_revision: verdict.revision,
        };
        let recorded = self.record_open(event).await;
        if !allowed {
            // A refusal stands whether or not it could be recorded.
            return verdict.decision;
        }
        if recorded.is_err() {
            self.state().open.remove(&key);
            return Decision::deny(
                503,
                "record_unavailable",
                hint("record_unavailable", &open.host, open.port),
            );
        }
        verdict.decision
    }

    async fn closed(&self, report: CloseReport) {
        self.state().open.remove(&(report.generation, report.id));
        let event = NetworkEvent::Close {
            conn: format!("g{}:{}", report.generation, report.id),
            ip: report.ip.map(|ip| ip.to_string()),
            up: report.up,
            down: report.down,
            ms: report.ms,
            outcome: match report.outcome {
                axocoatl_isolation::egress::CloseOutcome::Closed => CloseOutcome::Closed,
                axocoatl_isolation::egress::CloseOutcome::Reset => CloseOutcome::Reset,
                axocoatl_isolation::egress::CloseOutcome::UpstreamFailed => {
                    CloseOutcome::UpstreamFailed
                }
                axocoatl_isolation::egress::CloseOutcome::Interrupted => CloseOutcome::Interrupted,
                axocoatl_isolation::egress::CloseOutcome::Revoked => CloseOutcome::Revoked,
                axocoatl_isolation::egress::CloseOutcome::IdleTimeout => CloseOutcome::IdleTimeout,
            },
            error: report.error.map(|error| error.chars().take(512).collect()),
        };
        if let Err(error) = self.records.append(event).await {
            tracing::warn!(session = %self.session_id, ?error, "recording an egress close failed");
        }
    }

    fn attach_control(&self, handle: ControlHandle) {
        SessionEgress::attach_control(self, handle);
    }

    fn first_generation(&self) -> u32 {
        self.state().last_generation.saturating_add(1)
    }

    fn forbid_destinations(&self, addrs: &[IpAddr]) {
        self.state().forbidden.extend(addrs.iter().copied());
    }

    async fn sidecar_event(&self, event: SidecarEvent) {
        let (state, generation, container, detail) = match event {
            SidecarEvent::Starting {
                generation,
                container,
            } => (SidecarState::Starting, generation, container, None),
            SidecarEvent::Ready {
                generation,
                container,
            } => (SidecarState::Ready, generation, container, None),
            SidecarEvent::ChannelLost { generation, detail } => {
                (SidecarState::ChannelLost, generation, None, Some(detail))
            }
            SidecarEvent::Restarting { generation } => {
                (SidecarState::Restarting, generation, None, None)
            }
            SidecarEvent::Failed { generation, detail } => {
                (SidecarState::Failed, generation, None, Some(detail))
            }
            SidecarEvent::BudgetSpent { generation, detail } => {
                // The record says why egress stopped, in its control headroom.
                if let Err(error) = self
                    .records
                    .append_control(NetworkEvent::Limit {
                        what: LimitKind::RestartBudget,
                        detail: detail.chars().take(512).collect(),
                    })
                    .await
                {
                    tracing::warn!(session = %self.session_id, ?error, "recording the egress restart budget failed");
                }
                (SidecarState::Failed, generation, None, Some(detail))
            }
            SidecarEvent::Stopped { generation } => (SidecarState::Stopped, generation, None, None),
        };
        self.note_generation(generation);
        if state == SidecarState::Stopped {
            self.record_unrecorded_refusals().await;
            self.unbind_all(UnbindReason::SessionStopped).await;
        }
        if matches!(
            state,
            SidecarState::ChannelLost | SidecarState::Failed | SidecarState::Stopped
        ) {
            let mut current = self.state();
            if current
                .control
                .as_ref()
                .is_some_and(|control| control.generation() == generation)
            {
                current.control = None;
            }
            current
                .open
                .retain(|(connection_generation, _), _| *connection_generation != generation);
        }
        if let Err(error) = self
            .records
            .append_control(NetworkEvent::Sidecar {
                state,
                generation,
                container,
                detail: detail.map(|detail| detail.chars().take(512).collect()),
            })
            .await
        {
            tracing::warn!(session = %self.session_id, ?error, "recording an egress sidecar event failed");
        }
    }
}

#[cfg(test)]
#[path = "session_egress_tests.rs"]
pub(crate) mod tests;
