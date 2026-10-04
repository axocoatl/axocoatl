//! The egress policy decision point for one Session.
//!
//! [`SessionEgress`] mints the credentials processes present to the egress
//! proxy, decides each proxied request, and writes every decision to the
//! Session's network record before answering. A host name is checked against
//! the allowlist before anything resolves it; allowed names are resolved here,
//! on the host, and every resulting address is classified. Only addresses
//! that pass are returned to the sidecar, which connects to nothing else.
//!
//! A `CONNECT` to a host and port under `sandbox.egress.routes`, from a
//! process kind the route serves, is resolved and classified the same way and
//! then answered with a relay: the sidecar carries the client's TLS bytes
//! here, and the route broker ([`crate::egress_broker`]) ends TLS with a
//! certificate from this Session's own authority, checks each request against
//! the route's rules, adds the route's credential and sends the request to
//! the addresses resolved for the connection. On a route's ports the route
//! decides, whatever `allow` lists.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axocoatl_config::AxocoatlConfig;
use axocoatl_config::{CredentialSourceYaml, EgressAllowYaml, EgressRouteYaml};
use axocoatl_core::netaddr::{self, AddrClass};
use axocoatl_core::SecureDir;
use axocoatl_exec::egress::protocol::{credential_hash, credential_tag, MAX_ALLOW_ADDRS};
use axocoatl_isolation::egress::{
    CloseReport, Decision, EgressAuthority, EgressGrant, GrantKind, GrantSpec, Liveness,
    OpenRequest, ProxySecret, RelayOpen, RelayStream, RequestKind, SidecarEvent,
};
use axocoatl_isolation::egress_control::ControlHandle;
use axocoatl_isolation::session_trust::TrustFile;
use axocoatl_session::network_record::{
    BindingKind, CloseOutcome, ConnKind, Decision as RecordDecision, EgressBinding, EgressScope,
    LimitKind, NetworkEvent, NetworkLine, PolicyChange, PolicyOp, PolicySource, ProposalState,
    SidecarState, UnbindReason, MAX_RECORDED_PATH_CHARS,
};

use crate::egress_broker::terminate::BrokerTimeouts;
use crate::egress_broker::{
    BrokerRecordSink, RelayContext, Route, RouteTable, SessionBroker, SessionCa, TrustMaterial,
    UpstreamConnector, WorkspaceRoots,
};
use crate::session_egress_policy::{
    validate_session_host, CompiledPolicy, RoutePolicyEntry, SessionRule,
};
use crate::session_network::{PolicyRuleView, PolicyView, SessionNetworkRecords};
use crate::session_network_proposals::{
    new_proposal_id, Deciding, ProposalBook, ProposalRequest, ProposalView, Proposed,
    MAX_PENDING_PROPOSALS,
};
use crate::session_network_reload::{ConfigReload, ScopeReload, ScopeReloadFailure};

/// How long one name may take to resolve.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// A container start renews a Session's certificate authority with less
/// than this left (it is valid for 30 days), so a running container trusts
/// one that lasts at least this long.
pub const CA_RENEW_BEFORE: Duration = Duration::from_secs(2 * 24 * 3600);
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

/// Why a record write failed. The record has no cap, so it fails only when
/// it cannot be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordFailure {
    Unavailable(String),
}

/// The kinds of recorded lines a reopened decision point replays.
pub const REPLAYED_KINDS: &[&str] = &["policy", "proposal"];

/// Where a Session's egress events go.
#[async_trait::async_trait]
pub trait EgressRecordSink: Send + Sync + fmt::Debug {
    async fn append(&self, event: NetworkEvent) -> Result<u64, RecordFailure>;
    /// Append an event the decision point writes about itself: a `policy`,
    /// `sidecar`, `unbind` or `limit` event, or a person's decision on a
    /// proposal. The Session's record takes it like any other.
    async fn append_control(&self, event: NetworkEvent) -> Result<u64, RecordFailure>;
    /// Hand every recorded `policy` and `proposal` line ([`REPLAYED_KINDS`])
    /// to `visit`, oldest first, for replaying per-Session policy changes and
    /// proposals, and return the highest sidecar generation the record names
    /// ([`NetworkEvent::generation`]). Lines of other kinds may be handed
    /// over too. The Session's record is read one segment at a time, so
    /// nothing is kept but what `visit` keeps.
    async fn replay(
        &self,
        visit: &mut (dyn for<'line> FnMut(&'line NetworkLine) + Send),
    ) -> Result<u32, RecordFailure>;
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
    RecordFailure::Unavailable(error.to_string())
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
        self.append(event).await
    }

    async fn replay(
        &self,
        visit: &mut (dyn for<'line> FnMut(&'line NetworkLine) + Send),
    ) -> Result<u32, RecordFailure> {
        let mut after = None;
        loop {
            let page = self
                .records
                .read_kinds_after(
                    &self.session,
                    after,
                    REPLAYED_KINDS,
                    axocoatl_session::network_record::MAX_READ_LIMIT,
                )
                .await
                .map_err(record_failure)?;
            for line in &page.lines {
                visit(line);
            }
            if page.done {
                break;
            }
            after = page.next_after;
        }
        Ok(self
            .records
            .stats(&self.session)
            .await
            .map_err(record_failure)?
            .max_generation)
    }
}

/// The allowlists and routes a Session's policies compile from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressPolicyConfig {
    pub session_allow: Vec<EgressAllowYaml>,
    pub session_private: Vec<String>,
    /// `browser.allow` and `browser.private_destinations`; `None` when the
    /// browser tools are not configured.
    pub browser: Option<(Vec<EgressAllowYaml>, Vec<String>)>,
    /// `sandbox.egress.routes`, part of the Session scope.
    pub routes: Vec<EgressRouteYaml>,
    /// `credentials`, where the routes' credentials are read. Never values.
    pub credentials: BTreeMap<String, CredentialSourceYaml>,
}

impl EgressPolicyConfig {
    /// The policies of a Session's own decision point under `network:
    /// egress`: the Session's list and routes and, whenever `browser:` is
    /// configured, the browser's declared hosts as a scope of their own.
    /// Under `egress` the browser goes through the Session's own sidecar
    /// with a `browser` credential, which is checked only against
    /// `browser.allow`, while the Session's credentials are checked only
    /// against `sandbox.egress`. Under `bridge` and `none` the browser opens
    /// a decision point of its own instead
    /// ([`SessionEgress::open_browser_only`]).
    pub fn from_config(config: &AxocoatlConfig) -> Self {
        let egress = config.sandbox.egress.clone().unwrap_or_default();
        Self {
            session_allow: egress.allow,
            session_private: egress.private_destinations,
            browser: config
                .browser
                .as_ref()
                .map(|browser| (browser.allow.clone(), browser.private_destinations.clone())),
            routes: egress.routes,
            credentials: config.credentials.clone(),
        }
    }
}

/// What a decision point's route broker uses: the connector to route
/// upstreams, the Workspaces a credential file or `upstream_ca` must stay
/// out of, and its time limits.
#[derive(Clone)]
pub struct RouteSettings {
    pub upstream: Arc<UpstreamConnector>,
    pub workspaces: WorkspaceRoots,
    pub timeouts: BrokerTimeouts,
}

impl Default for RouteSettings {
    /// This computer's trust settings and own-address check, and no
    /// Workspaces.
    fn default() -> Self {
        Self {
            upstream: Arc::new(UpstreamConnector::new()),
            workspaces: Arc::new(Vec::new),
            timeouts: BrokerTimeouts::default(),
        }
    }
}

impl fmt::Debug for RouteSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteSettings")
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

/// The route broker of one decision point, made when it first has routes:
/// the Session's certificate authority lives in it, and only in memory.
struct RouteBroker {
    broker: Arc<SessionBroker>,
    /// The trust files for the authority, made on first use.
    trust: Option<Arc<[TrustFile]>>,
}

impl RouteBroker {
    fn new(session_id: &str, settings: &RouteSettings) -> Result<Self, String> {
        let ca = SessionCa::new(session_id)
            .map_err(|error| format!("creating the Session's certificate authority: {error}"))?;
        Ok(Self {
            broker: Arc::new(
                SessionBroker::new(
                    Arc::new(ca),
                    settings.upstream.clone(),
                    settings.workspaces.clone(),
                )
                .with_timeouts(settings.timeouts),
            ),
            trust: None,
        })
    }
}

#[cfg(test)]
#[path = "session_egress_reload_tests.rs"]
mod reload_tests;

#[cfg(test)]
#[path = "session_egress_proposal_tests.rs"]
mod proposal_tests;

/// A refused per-Session allow or revoke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressPolicyError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    NotFound(String),
}

struct ScopeState {
    policy: Arc<CompiledPolicy>,
    /// The routes `policy` names; only the Session scope has any.
    routes: Arc<RouteTable>,
    session_rules: Vec<SessionRule>,
    revision: u64,
}

/// One scope's policy as the record left it.
#[derive(Default)]
struct RecordedScope {
    rules: Vec<SessionRule>,
    revision: u64,
    digest: Option<String>,
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
    /// The destination as asked for, lowercase.
    host: String,
    port: u16,
    /// Set for a connection answered with a relay.
    relay: Option<RelayTarget>,
}

/// What the route broker needs for one relayed connection.
#[derive(Clone)]
struct RelayTarget {
    route: Arc<Route>,
    /// The addresses resolved and allowed for the connection.
    addrs: Vec<IpAddr>,
    binding: Option<EgressBinding>,
    token_tag: Option<String>,
}

/// A relayed connection's close waits for the broker's outcome, so the
/// `close` event says why the broker ended it (`sni_mismatch` and the like).
enum RelaySlot {
    /// Answered with a relay that the broker has not taken yet; the close,
    /// if it came already.
    Pending(Option<CloseReport>),
    /// The broker is serving it; the close, if it came already.
    Serving(Option<CloseReport>),
    /// The broker ended it, for this reason; the close has not come yet.
    Done(Option<String>),
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
    unattributed: UnattributedRefusals,
    /// Addresses refused whatever the policy lists (host gateways).
    forbidden: HashSet<IpAddr>,
    /// The highest sidecar generation in this Session's record.
    last_generation: u32,
    /// Relayed connections whose `close` is not recorded yet.
    relays: HashMap<(u32, u64), RelaySlot>,
}

/// The policy decision point for one Session.
pub struct SessionEgress {
    session_id: String,
    /// The lists each scope compiles from. [`SessionEgress::reload_config`]
    /// replaces a scope's once its new policy is recorded, under
    /// `policy_changes`.
    config: Mutex<EgressPolicyConfig>,
    records: Arc<dyn EgressRecordSink>,
    resolver: Arc<dyn EgressResolver>,
    classify: fn(IpAddr) -> AddrClass,
    env_dir: Option<SecureDir>,
    state: Mutex<State>,
    /// Serializes per-Session allows and revokes.
    policy_changes: tokio::sync::Mutex<()>,
    this: Weak<SessionEgress>,
    /// Agents' requests for hosts, waiting for a person.
    proposals: Mutex<ProposalBook>,
    /// What the route broker uses.
    route_settings: RouteSettings,
    /// The route broker, made when the Session scope first has routes.
    broker: Mutex<Option<RouteBroker>>,
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
        "record_unavailable" => "The Session's network record is unavailable, so new connections are refused.".into(),
        "invalid_host" => format!("{host} is not a valid host name or IP address."),
        "resolve_failed" => format!("{host} could not be resolved."),
        "tls_required" => format!(
            "{host}:{port} is an egress route: Axocoatl reads its requests only over HTTPS, so send them with https:// through the proxy."
        ),
        "route_not_for_binding" => format!(
            "{host}:{port} is an egress route that does not serve this kind of process. Add the kind to the route's for: list in sandbox.egress.routes."
        ),
        _ => "Axocoatl refused this connection.".into(),
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
    /// The route of a relay.
    route: Option<Arc<Route>>,
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
            route: None,
        }
    }

    /// Whether the connection goes ahead, as a tunnel or a relay.
    fn admitted(&self) -> bool {
        matches!(self.decision, Decision::Allow { .. } | Decision::Relay)
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

    /// [`SessionEgress::open`] with the route broker's settings.
    pub async fn open_with_routes(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
        env_dir: Option<SecureDir>,
        route_settings: RouteSettings,
    ) -> Result<Arc<Self>, String> {
        Self::open_session(
            session_id,
            config,
            records,
            resolver,
            env_dir,
            netaddr::classify,
            route_settings,
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
        Self::open_session(
            session_id,
            config,
            records,
            resolver,
            env_dir,
            classify,
            RouteSettings::default(),
        )
        .await
    }

    /// A Session's decision point with every scope it has and the route
    /// broker's settings.
    pub(crate) async fn open_session(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
        env_dir: Option<SecureDir>,
        classify: fn(IpAddr) -> AddrClass,
        route_settings: RouteSettings,
    ) -> Result<Arc<Self>, String> {
        let mut scopes = vec![EgressScope::Session, EgressScope::Provisioning];
        if config.browser.is_some() {
            scopes.push(EgressScope::Browser);
        }
        Self::open_scopes_with_classifier(
            session_id,
            config,
            records,
            resolver,
            env_dir,
            classify,
            &scopes,
            route_settings,
        )
        .await
    }

    /// A decision point for the browser's declared hosts alone, for a
    /// Session that does not run under `network: egress`. Only the browser
    /// scope is compiled and recorded; every other credential kind is refused.
    pub async fn open_browser_only(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
    ) -> Result<Arc<Self>, String> {
        if config.browser.is_none() {
            return Err("the browser is not configured".into());
        }
        Self::open_scopes_with_classifier(
            session_id,
            config,
            records,
            resolver,
            None,
            netaddr::classify,
            &[EgressScope::Browser],
            RouteSettings::default(),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn open_browser_only_with_classifier(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
        classify: fn(IpAddr) -> AddrClass,
    ) -> Result<Arc<Self>, String> {
        Self::open_scopes_with_classifier(
            session_id,
            config,
            records,
            resolver,
            None,
            classify,
            &[EgressScope::Browser],
            RouteSettings::default(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_scopes_with_classifier(
        session_id: impl Into<String>,
        config: EgressPolicyConfig,
        records: Arc<dyn EgressRecordSink>,
        resolver: Arc<dyn EgressResolver>,
        env_dir: Option<SecureDir>,
        classify: fn(IpAddr) -> AddrClass,
        scopes: &[EgressScope],
        route_settings: RouteSettings,
    ) -> Result<Arc<Self>, String> {
        let session_id = session_id.into();
        let workspaces = (route_settings.workspaces)();
        // Replay the record's policy changes and proposals, one segment at a
        // time: command ids already applied, so a resend after a restart is
        // refused, and each scope's revision, digest and per-Session rules.
        let mut commands: HashSet<String> = HashSet::new();
        let mut recorded: HashMap<EgressScope, RecordedScope> = HashMap::new();
        let mut proposals = ProposalBook::default();
        let last_generation = records
            .replay(&mut |line: &NetworkLine| {
                proposals.apply(line);
                let NetworkEvent::Policy {
                    scope,
                    revision,
                    digest,
                    source,
                    change,
                    ..
                } = &line.event
                else {
                    return;
                };
                if let Some(command_id) = change.as_ref().and_then(|c| c.command_id.clone()) {
                    commands.insert(command_id);
                }
                let state = recorded.entry(*scope).or_default();
                state.revision = *revision;
                state.digest = Some(digest.clone());
                match (source, change) {
                    (PolicySource::SessionAllow, Some(change)) if change.op == PolicyOp::Allow => {
                        state.rules.push(SessionRule {
                            revision: *revision,
                            host: change.host.clone(),
                            ports: change.ports.clone(),
                        })
                    }
                    (PolicySource::SessionRevoke, Some(change))
                        if change.op == PolicyOp::Revoke =>
                    {
                        state.rules.retain(|rule| rule.host != change.host)
                    }
                    _ => {}
                }
            })
            .await
            .map_err(|error| format!("reading the network record: {error:?}"))?;
        let scopes = scopes.to_vec();
        let mut states = HashMap::new();
        for scope in scopes {
            let RecordedScope {
                rules: session_rules,
                mut revision,
                digest,
            } = recorded.remove(&scope).unwrap_or_default();
            let (policy, routes) =
                Self::compile_scope(&config, scope, &session_rules, &workspaces)?;
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
                    routes,
                    session_rules,
                    revision,
                },
            );
        }
        let broker = match states.get(&EgressScope::Session) {
            Some(state) if !state.routes.is_empty() => {
                Some(RouteBroker::new(&session_id, &route_settings)?)
            }
            _ => None,
        };
        Ok(Arc::new_cyclic(|this| Self {
            session_id,
            config: Mutex::new(config),
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
            proposals: Mutex::new(proposals),
            route_settings,
            broker: Mutex::new(broker),
        }))
    }

    /// Compile one scope's policy, and for the Session scope its routes,
    /// which the policy's digest and rendering then cover. `workspaces` are
    /// refused as places for a route's `upstream_ca`.
    fn compile_scope(
        config: &EgressPolicyConfig,
        scope: EgressScope,
        session_rules: &[SessionRule],
        workspaces: &[PathBuf],
    ) -> Result<(CompiledPolicy, Arc<RouteTable>), String> {
        match scope {
            EgressScope::Session => {
                let policy = CompiledPolicy::compile(
                    scope,
                    &config.session_allow,
                    &config.session_private,
                    session_rules,
                )?;
                let routes = RouteTable::compile(&config.routes, &config.credentials, workspaces)?;
                let entries = routes
                    .routes()
                    .iter()
                    .map(|route| RoutePolicyEntry {
                        id: route.label(),
                        text: route.policy_text(),
                        canonical: route.canonical(),
                    })
                    .collect();
                Ok((policy.with_routes(entries)?, Arc::new(routes)))
            }
            EgressScope::Provisioning => Ok((
                CompiledPolicy::provisioning(),
                Arc::new(RouteTable::default()),
            )),
            EgressScope::Browser => {
                let (allow, private) = config.browser.clone().unwrap_or_default();
                Ok((
                    CompiledPolicy::compile(scope, &allow, &private, session_rules)?,
                    Arc::new(RouteTable::default()),
                ))
            }
        }
    }

    /// The route broker, made now if there is none yet. Its authority then
    /// stays for this decision point's lifetime, through reloads.
    fn ensure_route_broker(&self) -> Result<Arc<SessionBroker>, String> {
        let mut broker = self
            .broker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if broker.is_none() {
            *broker = Some(RouteBroker::new(&self.session_id, &self.route_settings)?);
        }
        Ok(broker
            .as_ref()
            .map(|broker| broker.broker.clone())
            .expect("made above"))
    }

    /// The route broker, if this decision point has had routes.
    fn route_broker(&self) -> Option<Arc<SessionBroker>> {
        self.broker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|broker| broker.broker.clone())
    }

    /// The files that make a container trust this Session's certificate
    /// authority, while the Session has routes: the bundle of this
    /// computer's roots plus the authority, and the authority alone. A
    /// container started with them mounts them at `/etc/axocoatl/ca`; one
    /// started without them never trusts a route's certificates. They hold
    /// certificates only; the authority's key stays in the daemon.
    pub fn trust_files(&self) -> Result<Option<Arc<[TrustFile]>>, String> {
        let has_routes = self
            .state()
            .scopes
            .get(&EgressScope::Session)
            .is_some_and(|scope| !scope.routes.is_empty());
        if !has_routes {
            return Ok(None);
        }
        let mut broker = self
            .broker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Each start renews an authority close to its end, so a container
        // started now trusts one that lasts.
        let renew = broker.as_ref().is_none_or(|current| {
            current.broker.ca().not_after() <= std::time::SystemTime::now() + CA_RENEW_BEFORE
        });
        if renew {
            *broker = Some(RouteBroker::new(&self.session_id, &self.route_settings)?);
        }
        let Some(broker) = broker.as_mut() else {
            return Ok(None);
        };
        if broker.trust.is_none() {
            let material = TrustMaterial::new(broker.broker.ca());
            if !material.host_root_errors.is_empty() || material.host_roots == 0 {
                tracing::warn!(
                    session = %self.session_id,
                    roots = material.host_roots,
                    errors = ?material.host_root_errors,
                    "some of this computer's trusted roots could not be read for the Session's trust bundle"
                );
            }
            broker.trust = Some(material.files().into());
        }
        Ok(broker.trust.clone())
    }

    /// The Session's certificate authority in PEM, while it has one.
    pub fn authority_pem(&self) -> Option<String> {
        self.broker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|broker| broker.broker.ca().pem())
    }

    #[cfg(test)]
    pub(crate) fn authority_der(&self) -> Option<rustls::pki_types::CertificateDer<'static>> {
        self.broker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|broker| broker.broker.ca().der().clone())
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
                    .chain(
                        scope_state
                            .policy
                            .routes()
                            .iter()
                            .map(|route| PolicyRuleView {
                                id: route.id.clone(),
                                text: route.text.clone(),
                                source: "route".to_string(),
                            }),
                    )
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
        self.records.append(event).await
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
        self.change_policy(scope, PolicyOp::Allow, host, ports, actor, command_id, None)
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
        self.change_policy(scope, PolicyOp::Revoke, host, None, actor, command_id, None)
            .await
    }

    /// Apply reloaded allowlists (`axocoatl network reload`). Every scope is
    /// compiled first, so an invalid list changes nothing. Each scope whose
    /// policy changed records it (`source: config_reload`) and then uses it
    /// for new connections; open connections admitted by a rule the new
    /// policy no longer has, unchanged, are closed. Removing a private range
    /// closes the scope's open connections. This Session's own allows stay.
    ///
    /// A scope whose policy could not be recorded keeps its policy and its
    /// lists ([`SessionEgress::policy_config`]), so a later per-Session allow
    /// still compiles from them and a later reload tries that scope again.
    /// The reload runs to its end even when the caller stops waiting.
    pub async fn reload_config(
        &self,
        config: EgressPolicyConfig,
        actor: &str,
    ) -> Result<ConfigReload, EgressPolicyError> {
        let this = self.strong()?;
        let actor = actor.to_string();
        tokio::spawn(async move { this.reload_config_now(config, &actor).await })
            .await
            .map_err(|error| {
                EgressPolicyError::Unavailable(format!("reloading the policy failed: {error}"))
            })?
    }

    /// This decision point as an `Arc`, for work that must finish even when
    /// its caller is dropped.
    fn strong(&self) -> Result<Arc<SessionEgress>, EgressPolicyError> {
        self.this.upgrade().ok_or_else(|| {
            EgressPolicyError::Unavailable("this Session's decision point is closing".into())
        })
    }

    async fn reload_config_now(
        &self,
        config: EgressPolicyConfig,
        actor: &str,
    ) -> Result<ConfigReload, EgressPolicyError> {
        let _serial = self.policy_changes.lock().await;
        let workspaces = (self.route_settings.workspaces)();
        let planned = {
            let state = self.state();
            let mut scopes: Vec<(&EgressScope, &ScopeState)> = state.scopes.iter().collect();
            scopes.sort_by_key(|(scope, _)| scope.as_str());
            let mut planned = Vec::new();
            for (scope, scope_state) in scopes {
                let (policy, routes) =
                    Self::compile_scope(&config, *scope, &scope_state.session_rules, &workspaces)
                        .map_err(EgressPolicyError::Invalid)?;
                if policy.digest() != scope_state.policy.digest() {
                    planned.push((*scope, scope_state.revision + 1, policy, routes));
                }
            }
            planned
        };
        // A Session that gains its first routes gets its authority before
        // the new policy is recorded.
        if planned
            .iter()
            .any(|(scope, _, _, routes)| *scope == EgressScope::Session && !routes.is_empty())
        {
            self.ensure_route_broker()
                .map_err(EgressPolicyError::Unavailable)?;
        }
        // A scope whose policy does not change compiles the same from either
        // list, and a scope this decision point does not have uses neither:
        // both take the new lists now. A changed scope takes them only once
        // its policy is recorded and in use.
        let changing: Vec<EgressScope> = planned.iter().map(|(scope, _, _, _)| *scope).collect();
        self.adopt_lists(&config, |scope| !changing.contains(&scope));
        let mut reload = ConfigReload::default();
        for (scope, revision, policy, routes) in planned {
            let digest = policy.digest().to_string();
            let recorded = self
                .records
                .append_control(NetworkEvent::Policy {
                    scope,
                    revision,
                    digest: digest.clone(),
                    source: PolicySource::ConfigReload,
                    rules: policy.rendered(),
                    change: None,
                    actor: Some(actor.chars().take(128).collect()),
                })
                .await;
            if let Err(error) = recorded {
                tracing::warn!(session = %self.session_id, scope = scope.as_str(), ?error, "recording a reloaded policy failed; the scope keeps its policy");
                reload.failed.push(ScopeReloadFailure {
                    scope,
                    error: format!(
                        "recording the reloaded {} policy failed: {error:?}",
                        scope.as_str()
                    ),
                });
                continue;
            }
            let (control, revoke_ids) = {
                let mut state = self.state();
                let generation = state.control.as_ref().map(ControlHandle::generation);
                let scope_state = state.scopes.get_mut(&scope).expect("planned from a scope");
                let old = std::mem::replace(&mut scope_state.policy, Arc::new(policy));
                scope_state.routes = routes;
                scope_state.revision = revision;
                let new = scope_state.policy.clone();
                let narrowed = old
                    .private_destinations()
                    .iter()
                    .any(|range| !new.private_destinations().contains(range));
                // A route that changed in any way closes its connections:
                // they were served by its old rules or credential. A tunnel
                // to a host and port a route now covers closes too: the
                // route decides there.
                let gone = old.removed_ids(&new);
                let routes = scope_state.routes.clone();
                let mut revoke_ids: Vec<u64> = state
                    .open
                    .iter()
                    .filter(|((connection_generation, _), connection)| {
                        Some(*connection_generation) == generation
                            && connection.scope == scope
                            && (narrowed
                                || gone.contains(&connection.rule_id)
                                || (connection.relay.is_none()
                                    && routes.find(&connection.host, connection.port).is_some()))
                    })
                    .map(|((_, id), _)| *id)
                    .collect();
                revoke_ids.sort_unstable();
                (state.control.clone(), revoke_ids)
            };
            self.adopt_lists(&config, |adopted| adopted == scope);
            let closed = revoke_ids.len();
            if let (Some(control), false) = (control, revoke_ids.is_empty()) {
                control.revoke(revoke_ids);
            }
            reload.changed.push(ScopeReload {
                scope,
                revision,
                digest,
                closed,
            });
        }
        Ok(reload)
    }

    /// Take the lists of `config` for the scopes `adopt` names (the
    /// `session` scope's two lists, the `browser` scope's pair).
    fn adopt_lists(&self, config: &EgressPolicyConfig, adopt: impl Fn(EgressScope) -> bool) {
        let mut applied = self
            .config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if adopt(EgressScope::Session) {
            applied.session_allow = config.session_allow.clone();
            applied.session_private = config.session_private.clone();
            applied.routes = config.routes.clone();
            applied.credentials = config.credentials.clone();
        }
        if adopt(EgressScope::Browser) {
            applied.browser = config.browser.clone();
        }
    }

    /// The allowlists this decision point compiles from: per scope, the
    /// ones it last recorded a policy for.
    pub fn policy_config(&self) -> EgressPolicyConfig {
        self.config
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn proposal_book(&self) -> std::sync::MutexGuard<'_, ProposalBook> {
        self.proposals
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// An Agent asks for one exact host. The request is recorded as a
    /// pending proposal before it is kept, and joins an identical pending
    /// one instead of adding another. Only a person decides it
    /// ([`SessionEgress::approve_proposal`], [`SessionEgress::reject_proposal`]).
    pub async fn propose(&self, request: ProposalRequest) -> Result<Proposed, EgressPolicyError> {
        let host = validate_session_host(&request.host).map_err(EgressPolicyError::Invalid)?;
        let policy = self.policy(EgressScope::Session).ok_or_else(|| {
            EgressPolicyError::Invalid("this Session has no session egress policy".into())
        })?;
        if request
            .ports
            .iter()
            .all(|port| policy.match_name(&host, *port).is_some())
        {
            return Err(EgressPolicyError::Invalid(format!(
                "{host} is already allowed for this Session on ports {:?}; a refusal of it had another reason, which the proxy's answer names",
                request.ports
            )));
        }
        {
            let book = self.proposal_book();
            if let Some((view, outcome)) = book.joinable(&host, &request.ports) {
                return Ok(Proposed {
                    view,
                    created: false,
                    outcome,
                });
            }
            if book.pending() >= MAX_PENDING_PROPOSALS {
                return Err(EgressPolicyError::Conflict(format!(
                    "this Session already has {MAX_PENDING_PROPOSALS} proposals waiting for a person"
                )));
            }
        }
        let id = new_proposal_id().map_err(EgressPolicyError::Unavailable)?;
        let view = ProposalView {
            id: id.clone(),
            state: ProposalState::Pending,
            host,
            ports: request.ports,
            reason: Some(request.reason),
            agent: Some(request.agent),
            invocation_id: Some(request.invocation_id),
            activation_id: Some(request.activation_id),
            revision: None,
            actor: None,
        };
        self.records
            .append(NetworkEvent::Proposal {
                id: id.clone(),
                state: ProposalState::Pending,
                host: view.host.clone(),
                ports: view.ports.clone(),
                reason: view.reason.clone(),
                agent: view.agent.clone(),
                invocation_id: view.invocation_id.clone(),
                activation_id: view.activation_id.clone(),
                actor: None,
                command_id: None,
                revision: None,
            })
            .await
            .map_err(|error| {
                EgressPolicyError::Unavailable(format!("recording the proposal failed: {error:?}"))
            })?;
        let mut book = self.proposal_book();
        // Another call may have recorded the same request meanwhile; both
        // lines stay in the record, and both wait on the first one kept.
        if let Some((view, outcome)) = book.joinable(&view.host, &view.ports) {
            return Ok(Proposed {
                view,
                created: false,
                outcome,
            });
        }
        let outcome = book.insert(view.clone());
        Ok(Proposed {
            view,
            created: true,
            outcome,
        })
    }

    /// A person approves a pending proposal: the ordinary per-Session allow
    /// of its host and ports in the `session` scope, recorded with the
    /// person as actor and the proposal's id, then the proposal's outcome.
    /// The decision runs to its end even when the caller stops waiting, so
    /// the record and the proposal never disagree and the proposal is never
    /// left "being decided".
    pub async fn approve_proposal(
        &self,
        id: &str,
        actor: &str,
        command_id: &str,
    ) -> Result<(u64, String), EgressPolicyError> {
        let this = self.strong()?;
        let (id, actor, command_id) = (id.to_string(), actor.to_string(), command_id.to_string());
        tokio::spawn(async move { this.approve_proposal_now(&id, &actor, &command_id).await })
            .await
            .map_err(|error| {
                EgressPolicyError::Unavailable(format!("approving the proposal failed: {error}"))
            })?
    }

    async fn approve_proposal_now(
        &self,
        id: &str,
        actor: &str,
        command_id: &str,
    ) -> Result<(u64, String), EgressPolicyError> {
        let (deciding, host, ports) = Deciding::begin(&self.proposals, id)?;
        let (revision, digest) = self
            .change_policy(
                EgressScope::Session,
                PolicyOp::Allow,
                &host,
                Some(ports.clone()),
                actor,
                command_id,
                Some(id),
            )
            .await?;
        deciding.finish(ProposalState::Approved, actor, Some(revision));
        self.record_decision(
            id,
            ProposalState::Approved,
            &host,
            &ports,
            actor,
            command_id,
            Some(revision),
        )
        .await;
        Ok((revision, digest))
    }

    /// A person rejects a pending proposal. Nothing about the policy changes.
    /// Like an approval, it runs to its end even when the caller stops
    /// waiting.
    pub async fn reject_proposal(
        &self,
        id: &str,
        actor: &str,
        command_id: &str,
    ) -> Result<(), EgressPolicyError> {
        let this = self.strong()?;
        let (id, actor, command_id) = (id.to_string(), actor.to_string(), command_id.to_string());
        tokio::spawn(async move { this.reject_proposal_now(&id, &actor, &command_id).await })
            .await
            .map_err(|error| {
                EgressPolicyError::Unavailable(format!("rejecting the proposal failed: {error}"))
            })?
    }

    async fn reject_proposal_now(
        &self,
        id: &str,
        actor: &str,
        command_id: &str,
    ) -> Result<(), EgressPolicyError> {
        if command_id.is_empty() || command_id.len() > 128 {
            return Err(EgressPolicyError::Invalid(
                "command_id must be 1-128 characters".into(),
            ));
        }
        let (deciding, host, ports) = Deciding::begin(&self.proposals, id)?;
        self.records
            .append_control(Self::decision_event(
                id,
                ProposalState::Rejected,
                &host,
                &ports,
                actor,
                command_id,
                None,
            ))
            .await
            .map_err(|error| {
                EgressPolicyError::Unavailable(format!("recording the rejection failed: {error:?}"))
            })?;
        deciding.finish(ProposalState::Rejected, actor, None);
        Ok(())
    }

    fn decision_event(
        id: &str,
        state: ProposalState,
        host: &str,
        ports: &[u16],
        actor: &str,
        command_id: &str,
        revision: Option<u64>,
    ) -> NetworkEvent {
        NetworkEvent::Proposal {
            id: id.to_string(),
            state,
            host: host.to_string(),
            ports: ports.to_vec(),
            reason: None,
            agent: None,
            invocation_id: None,
            activation_id: None,
            actor: Some(actor.chars().take(128).collect()),
            command_id: Some(command_id.to_string()),
            revision,
        }
    }

    /// Record an approval's outcome. The `policy` line with the proposal's
    /// id already decided it, so a failure here only loses the summary line.
    #[allow(clippy::too_many_arguments)]
    async fn record_decision(
        &self,
        id: &str,
        state: ProposalState,
        host: &str,
        ports: &[u16],
        actor: &str,
        command_id: &str,
        revision: Option<u64>,
    ) {
        if let Err(error) = self
            .records
            .append_control(Self::decision_event(
                id, state, host, ports, actor, command_id, revision,
            ))
            .await
        {
            tracing::warn!(session = %self.session_id, ?error, "recording a proposal's approval failed");
        }
    }

    /// Every proposal this decision point keeps, pending ones first.
    pub fn proposals(&self) -> Vec<ProposalView> {
        self.proposal_book().views()
    }

    /// One proposal, if kept.
    pub fn proposal(&self, id: &str) -> Option<ProposalView> {
        self.proposal_book().view(id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn change_policy(
        &self,
        scope: EgressScope,
        op: PolicyOp,
        host: &str,
        ports: Option<Vec<u16>>,
        actor: &str,
        command_id: &str,
        proposal_id: Option<&str>,
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
        let config = self.policy_config();
        let workspaces = (self.route_settings.workspaces)();
        let (revision, policy, routes, session_rules, removed_ids) = {
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
            let (policy, routes) = Self::compile_scope(&config, scope, &session_rules, &workspaces)
                .map_err(EgressPolicyError::Invalid)?;
            (revision, policy, routes, session_rules, removed_ids)
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
                    proposal_id: proposal_id.map(str::to_string),
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
            scope_state.routes = routes;
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

    /// Whether `scope` still admits what `verdict` allowed: the rule that
    /// matched still matches this host and port (a reload can give a rule id
    /// another meaning), or the same route still covers them, and every
    /// private address is still in a listed range.
    fn still_admits(&self, scope: &ScopeState, open: &OpenRequest, verdict: &Verdict) -> bool {
        let Some(rule) = verdict.rule.as_deref() else {
            return false;
        };
        let host = match netaddr::parse_ip_literal(&open.host) {
            Some(ip) => ip.to_string(),
            None => match netaddr::normalize_host_name(&open.host) {
                Ok(name) => name,
                Err(_) => return false,
            },
        };
        let policy = &scope.policy;
        let matched = match &verdict.route {
            Some(route) => scope
                .routes
                .find(&host, open.port)
                .is_some_and(|current| current == *route),
            None => {
                // A route that now covers the host decides instead.
                scope.routes.find(&host, open.port).is_none()
                    && policy.admits(rule, &host, open.port)
            }
        };
        matched
            && verdict.addrs.iter().all(|addr| {
                !matches!((self.classify)(*addr), AddrClass::Private(_))
                    || policy.allows_private(*addr)
            })
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

    /// Decide one request against `scope`. On a route's host and port (a
    /// name, never an address) the route decides: only a `CONNECT` from a
    /// process kind it serves is admitted, as a relay; elsewhere the
    /// allowlist decides and admits a tunnel.
    async fn verdict(
        &self,
        open: &OpenRequest,
        scope: EgressScope,
        kind: Option<BindingKind>,
    ) -> Verdict {
        let Some((policy, revision, routes)) = self
            .state()
            .scopes
            .get(&scope)
            .map(|state| (state.policy.clone(), state.revision, state.routes.clone()))
        else {
            return Verdict::deny(403, "not_allowed", &open.host, open.port);
        };
        let deny = |status, reason| Verdict {
            revision: Some(revision),
            ..Verdict::deny(status, reason, &open.host, open.port)
        };
        let (rule, addrs, route) = if let Some(ip) = netaddr::parse_ip_literal(&open.host) {
            // A literal needs no resolution, so a never-allowed address is
            // named as such even when no range lists it.
            if (self.classify)(ip).is_forbidden() || self.is_host_gateway(ip) {
                return Verdict {
                    addrs: vec![ip],
                    ..deny(403, "forbidden_destination")
                };
            }
            match policy.match_ip(ip, open.port) {
                Some(rule) => (rule.id.clone(), vec![ip], None),
                None => return deny(403, "not_allowed"),
            }
        } else {
            let Ok(name) = netaddr::normalize_host_name(&open.host) else {
                return deny(400, "invalid_host");
            };
            let route = routes.find(&name, open.port);
            let rule = match &route {
                Some(route) => {
                    let refuse = |reason| Verdict {
                        rule: Some(route.label()),
                        ..deny(403, reason)
                    };
                    // The route reads requests only inside TLS it ends.
                    if open.kind != RequestKind::Connect {
                        return refuse("tls_required");
                    }
                    if !kind.is_some_and(|kind| route.allows_binding(kind)) {
                        return refuse("route_not_for_binding");
                    }
                    route.label()
                }
                None => match policy.match_name(&name, open.port) {
                    Some(rule) => rule.id.clone(),
                    None => return deny(403, "not_allowed"),
                },
            };
            // Only now, after the allowlist or a route matched, does the
            // name resolve.
            let mut addrs = match self.resolver.resolve(&name, open.port).await {
                Ok(addrs) if !addrs.is_empty() => addrs,
                _ => {
                    return Verdict {
                        rule: route.as_ref().map(|route| route.label()),
                        ..deny(502, "resolve_failed")
                    }
                }
            };
            let mut seen = HashSet::new();
            addrs.retain(|addr| seen.insert(*addr));
            addrs.truncate(MAX_ALLOW_ADDRS);
            (rule, addrs, route)
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
            decision: match route {
                Some(_) => Decision::Relay,
                None => Decision::Allow {
                    addrs: addrs.clone(),
                },
            },
            reason: None,
            status: None,
            rule: Some(rule),
            addrs,
            revision: Some(revision),
            route,
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
            attempt_id: spec.attempt_id.clone(),
        };
        let env_file = if spec.kind == GrantKind::Browser {
            None
        } else {
            let dir = self
                .env_dir
                .as_ref()
                .ok_or("no directory for egress env files")?;
            let name = format!("egress-{tag}.env");
            let contents =
                env_file_contents(&token) + &self.route_env(spec.kind, spec.trust_mounted);
            dir.atomic_write_with_mode(&name, contents.as_bytes(), 0o600)
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
        let kind = caller.binding.as_ref().map(|binding| binding.kind);
        let mut verdict = match refusal {
            None => self.verdict(&open, scope, kind).await,
            Some((status, reason)) => Verdict::deny(status, reason, &open.host, open.port),
        };
        let key = (open.generation, open.id);
        if verdict.admitted() {
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
                    self.still_admits(scope_state, &open, &verdict),
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
                let relay = verdict.route.clone().map(|route| RelayTarget {
                    route,
                    addrs: verdict.addrs.clone(),
                    binding: caller.binding.clone(),
                    token_tag: caller.tag.clone(),
                });
                if relay.is_some() {
                    state.relays.insert(key, RelaySlot::Pending(None));
                }
                state.open.insert(
                    key,
                    OpenConnection {
                        token_hash: caller.hash.clone(),
                        rule_id: verdict.rule.clone().unwrap_or_default(),
                        scope,
                        host: open.host.trim_end_matches('.').to_ascii_lowercase(),
                        port: open.port,
                        relay,
                    },
                );
            }
        }
        let allowed = verdict.admitted();
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
            // The sidecar checked these bounds; keep the record's own.
            peer: open.peer.as_ref().map(|peer| {
                use axocoatl_session::network_record as record;
                let fits = |path: &String, max: usize| path.chars().count() <= max;
                record::PeerIdentity {
                    pid: peer.pid,
                    uid: peer.uid,
                    gid: peer.gid,
                    exe: peer
                        .exe
                        .clone()
                        .filter(|exe| fits(exe, record::MAX_RECORDED_PEER_PATH_CHARS)),
                    exe_sha256: peer.exe_sha256.clone(),
                    ancestors: peer
                        .ancestors
                        .iter()
                        .take(record::MAX_RECORDED_PEER_ANCESTORS)
                        .take_while(|path| fits(path, record::MAX_RECORDED_PEER_ANCESTOR_CHARS))
                        .cloned()
                        .collect(),
                    error: peer.error.clone(),
                }
            }),
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
            let mut state = self.state();
            state.open.remove(&key);
            state.relays.remove(&key);
            drop(state);
            return Decision::deny(
                503,
                "record_unavailable",
                hint("record_unavailable", &open.host, open.port),
            );
        }
        verdict.decision
    }

    async fn closed(&self, report: CloseReport) {
        let key = (report.generation, report.id);
        let report = {
            let mut state = self.state();
            state.open.remove(&key);
            match state.relays.remove(&key) {
                // The broker has the connection, or is about to: its close
                // is recorded when the broker ends, with the broker's reason.
                Some(RelaySlot::Pending(_)) => {
                    state.relays.insert(key, RelaySlot::Pending(Some(report)));
                    return;
                }
                Some(RelaySlot::Serving(_)) => {
                    state.relays.insert(key, RelaySlot::Serving(Some(report)));
                    return;
                }
                Some(RelaySlot::Done(error)) => with_broker_error(report, error),
                None => report,
            }
        };
        self.record_close(report).await;
    }

    async fn relay(&self, open: RelayOpen, stream: RelayStream) {
        let key = (open.generation, open.id);
        let target = {
            let mut state = self.state();
            if let Some(RelaySlot::Pending(close)) = state.relays.remove(&key) {
                state.relays.insert(key, RelaySlot::Serving(close));
            }
            state
                .open
                .get(&key)
                .and_then(|connection| connection.relay.clone())
        };
        let error = match (target, self.route_broker(), self.this.upgrade()) {
            (Some(target), Some(broker), Some(this)) => {
                let context = RelayContext {
                    session: self.session_id.clone(),
                    conn: format!("g{}:{}", open.generation, open.id),
                    route: target.route,
                    host: open.open.host.clone(),
                    port: open.open.port,
                    addrs: target.addrs,
                    binding: target.binding,
                    token_tag: target.token_tag,
                };
                let sink: Arc<dyn BrokerRecordSink> = Arc::new(RouteRecordSink { egress: this });
                broker.serve(context, stream, sink).await.error
            }
            // Closed or revoked before the broker could take it.
            _ => {
                drop(stream);
                Some("route_closed: the connection ended before the route served it".into())
            }
        };
        self.finish_relay(key, error).await;
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
        self.record_sidecar_event(event).await;
    }
}

/// `report` with the broker's reason for ending the connection first.
fn with_broker_error(mut report: CloseReport, error: Option<String>) -> CloseReport {
    if let Some(error) = error {
        report.error = Some(match report.error.take() {
            Some(proxy) => format!("{error}; {proxy}"),
            None => error,
        });
    }
    report
}

/// The broker's `request` and `response` events go to the Session's
/// network record, like its connections.
struct RouteRecordSink {
    egress: Arc<SessionEgress>,
}

#[async_trait::async_trait]
impl BrokerRecordSink for RouteRecordSink {
    async fn append(&self, event: NetworkEvent) -> Result<(), String> {
        self.egress
            .record_open(event)
            .await
            .map(|_| ())
            .map_err(|RecordFailure::Unavailable(error)| error)
    }
}

impl SessionEgress {
    /// The broker ended a relayed connection: record its close now if the
    /// sidecar reported it already, or keep the reason for when it does.
    async fn finish_relay(&self, key: (u32, u64), error: Option<String>) {
        let report = {
            let mut state = self.state();
            match state.relays.remove(&key) {
                Some(RelaySlot::Serving(Some(report))) => Some(with_broker_error(report, error)),
                Some(RelaySlot::Serving(None)) => {
                    state.relays.insert(key, RelaySlot::Done(error));
                    None
                }
                Some(other) => {
                    state.relays.insert(key, other);
                    None
                }
                None => None,
            }
        };
        if let Some(report) = report {
            self.record_close(report).await;
        }
    }

    /// Environment a credential's env file adds for egress routes, for a
    /// process kind some route serves: each such route's placeholders and,
    /// when the process's container mounts the trust files, the variables
    /// that point TLS clients at them.
    fn route_env(&self, kind: GrantKind, trust_mounted: bool) -> String {
        let binding = match kind {
            GrantKind::Agent | GrantKind::Setup | GrantKind::Terminal => binding_kind(kind),
            GrantKind::Provisioning | GrantKind::Browser => return String::new(),
        };
        let Some(routes) = self
            .state()
            .scopes
            .get(&EgressScope::Session)
            .map(|scope| scope.routes.clone())
        else {
            return String::new();
        };
        let served: Vec<&Arc<Route>> = routes
            .routes()
            .iter()
            .filter(|route| route.allows_binding(binding))
            .collect();
        if served.is_empty() {
            return String::new();
        }
        let mut contents = String::new();
        if trust_mounted {
            for (name, value) in crate::egress_broker::trust::trust_env() {
                contents.push_str(&format!("{name}={value}\n"));
            }
        }
        for route in served {
            for name in &route.env_placeholders {
                contents.push_str(&format!("{name}=axocoatl-route:{}\n", route.host));
            }
        }
        contents
    }

    /// A sidecar generation ended: its connections are gone. A relay the
    /// broker is serving ends with the channel and records its close
    /// itself; one the broker never took, or that ended without a close, is
    /// let go. Returns the closes such relays held, to record.
    fn forget_generation(&self, generation: u32) -> Vec<CloseReport> {
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
        let lost: Vec<(u32, u64)> = current
            .relays
            .iter()
            .filter(|((connection_generation, _), slot)| {
                *connection_generation == generation && !matches!(slot, RelaySlot::Serving(_))
            })
            .map(|(key, _)| *key)
            .collect();
        lost.into_iter()
            .filter_map(|key| match current.relays.remove(&key) {
                Some(RelaySlot::Pending(close)) => close,
                _ => None,
            })
            .collect()
    }

    async fn record_close(&self, report: CloseReport) {
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

    async fn record_sidecar_event(&self, event: SidecarEvent) {
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
            let held = self.forget_generation(generation);
            for report in held {
                self.record_close(report).await;
            }
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
#[path = "session_egress_attempt_tests.rs"]
mod attempt_tests;

#[cfg(test)]
#[path = "session_egress_route_tests.rs"]
pub(crate) mod route_tests;

#[cfg(test)]
#[path = "session_egress_tests.rs"]
pub(crate) mod tests;
