//! A Session's network record: every egress decision, connection close,
//! route request and response, policy change, web-tool call and browser
//! call, in order, one JSON line each. Screenshots the browser tools take
//! are kept beside it, by digest, in its `screenshots/` directory; they are
//! for people and never reach a model.
//!
//! The record is append-only and keeps every event for the Session's life;
//! only each line and each screenshot is bounded. [`NetworkRecord::append`]
//! returns only after a complete `write(2)` of the whole line to the active
//! segment, so a caller that waits for it before acting has a write-ahead
//! record of that action in the file. Durability across an operating-system
//! crash comes from [`NetworkRecord::sync`], which the owner calls every
//! 200 ms while the record is dirty and on close, and from sealing, which
//! writes a full active segment to an immutable, synced file. How segments
//! are kept is described in `network_record_segments.rs`.
//!
//! On open, a torn or unparseable last line (an interrupted append) is cut off
//! and a `sidecar{state: "recovered"}` event says how many bytes were removed.
//! Any other damage refuses the open. Sequence numbers continue from the last
//! good line; a gap is counted, never renumbered.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};

#[path = "network_record_segments.rs"]
mod segments;

use segments::{damaged, Chain, Header, Sealed, Tally, ACTIVE_FILE, SEGMENTS_DIR};
pub use segments::{SegmentLimits, DEFAULT_SEGMENT_BYTES, DEFAULT_SEGMENT_EVENTS};

/// The record's file inside its component directory.
pub const NETWORK_RECORD_FILE: &str = "network-record.v1.jsonl";
/// Line format version.
pub const NETWORK_RECORD_VERSION: u32 = 1;
/// Longest line, newline included.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// Most lines one read returns.
pub const MAX_READ_LIMIT: usize = 1000;
/// Longest recorded request path, in characters.
pub const MAX_RECORDED_PATH_CHARS: usize = 256;
/// Most addresses one `open` event carries.
pub const MAX_RECORDED_ADDRS: usize = 16;

/// Which policy a credential is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressScope {
    Session,
    Provisioning,
    Browser,
}

impl EgressScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Provisioning => "provisioning",
            Self::Browser => "browser",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    Config,
    SessionAllow,
    SessionRevoke,
    /// `axocoatl network reload` or `POST /api/network/reload` applied the
    /// configuration file's allowlists to a running Session.
    ConfigReload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOp {
    Allow,
    Revoke,
}

/// A per-Session allow or revoke, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyChange {
    pub op: PolicyOp,
    pub host: String,
    #[serde(default)]
    pub ports: Vec<u16>,
    /// The person's command id, so a resend is recognized after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// The Agent's proposal this allow approved, when it approved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarState {
    Starting,
    Ready,
    ChannelLost,
    Restarting,
    Failed,
    Stopped,
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnbindReason {
    Settled,
    TerminalClosed,
    SetupDone,
    ProvisioningDone,
    BrowserDone,
    SessionStopped,
}

/// Where an Agent's request for a host stands. Only a person moves it out
/// of `pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Approved,
    Rejected,
}

impl ProposalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// Longest reason an Agent may give for a proposed host, in bytes.
pub const MAX_PROPOSAL_REASON_BYTES: usize = 1024;
/// Most ports one proposal names.
pub const MAX_PROPOSAL_PORTS: usize = 16;

/// Whether `value` is a proposal id: `prop_` and 16 lowercase hex digits.
pub fn is_proposal_id(value: &str) -> bool {
    value.strip_prefix("prop_").is_some_and(|hex| {
        hex.len() == 16
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
#[path = "network_record_proposal_tests.rs"]
mod proposal_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

/// How the client asked: an HTTPS `CONNECT` tunnel or a plain-HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnKind {
    Connect,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseOutcome {
    Closed,
    Reset,
    UpstreamFailed,
    Interrupted,
    Revoked,
    IdleTimeout,
}

/// How a request on a route ended (`response` events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    /// The whole response reached the client.
    Completed,
    /// The upstream could not be reached, refused the request or broke off.
    UpstreamFailed,
    /// The response carried the route's credential; the connection was
    /// closed before that part was passed on.
    CredentialReflected,
    /// A compressed response on a credentialed route was refused.
    EncodedResponse,
    /// The request body was larger than the route's `max_request_bytes`.
    TooLarge,
    /// The client went away before the response was complete.
    ClientClosed,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Longest method a `request` event keeps.
pub const MAX_RECORDED_METHOD_CHARS: usize = 32;
/// Longest rule, reason or credential name a `request` event keeps.
pub const MAX_RECORDED_REQUEST_LABEL_CHARS: usize = 128;

#[cfg(all(test, unix))]
#[path = "network_record_request_tests.rs"]
mod request_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTool {
    WebSearch,
    WebFetch,
}

/// The browser tool that made a `browser` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTool {
    Browser,
    BrowserCheck,
}

/// Directory, beside the record, that holds screenshots by digest.
pub const SCREENSHOT_DIR: &str = "screenshots";
/// Largest screenshot kept.
pub const MAX_SCREENSHOT_BYTES: usize = 1024 * 1024;
/// Longest failure reason a `browser` event keeps.
pub const MAX_BROWSER_ERROR_CHARS: usize = 500;
/// Longest URL a `browser` event keeps.
pub const MAX_RECORDED_URL_CHARS: usize = 2048;

/// A stored screenshot, named by the SHA-256 of its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotRef {
    pub sha256: String,
    /// `image/jpeg` or `image/png`.
    pub media_type: String,
    pub bytes: u64,
}

fn screenshot_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        _ => None,
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// Axocoatl 1.2.0 wrote this when the record reached its cap. The record
    /// has no cap now; the kind is kept so those records still read.
    RecordFull,
    MaxConnections,
    RestartBudget,
    /// Refusals of connections without a valid credential that were counted
    /// instead of recorded one by one; the detail starts with the count.
    UnrecordedRefusals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    Agent,
    Setup,
    Provisioning,
    Terminal,
    Browser,
}

/// What an egress credential belongs to. The fields that apply depend on
/// `kind`; absent fields are omitted from the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressBinding {
    pub kind: BindingKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// `"{invocation_id}:{index}"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_index: Option<u32>,
    /// The Ways attempt whose container presented the credential. Attempts
    /// under `network: egress` share their Session's proxy and record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
}

impl EgressBinding {
    pub fn new(kind: BindingKind) -> Self {
        Self {
            kind,
            invocation_id: None,
            activation_id: None,
            node_id: None,
            agent: None,
            process: None,
            terminal_id: None,
            setup_index: None,
            attempt_id: None,
        }
    }
}

#[cfg(test)]
#[path = "network_record_attempt_tests.rs"]
mod attempt_tests;

/// Longest URL recorded for one search result in a `web` event's `sources`.
pub const MAX_RECORDED_SOURCE_URL_BYTES: usize = 512;
/// Longest URL recorded in a `web` event's `url`, `final_url` or `redirects`.
pub const MAX_RECORDED_WEB_URL_BYTES: usize = 1024;

/// A search result's source id and URL, as recorded in a `web` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSource {
    pub id: String,
    pub url: String,
    /// The URL was longer than [`MAX_RECORDED_SOURCE_URL_BYTES`] and was cut.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub url_truncated: bool,
}

/// Longest program path an `open` event's `peer` keeps.
pub const MAX_RECORDED_PEER_PATH_CHARS: usize = 1024;
/// Longest ancestor path an `open` event's `peer` keeps.
pub const MAX_RECORDED_PEER_ANCESTOR_CHARS: usize = 256;
/// Most ancestors an `open` event's `peer` keeps.
pub const MAX_RECORDED_PEER_ANCESTORS: usize = 8;

/// The program behind a connection, as the Session container's init process
/// found it when the connection opened: the process, its executable and that
/// file's SHA-256, its user and group, and its parents' executables (nearest
/// first). What could not be found is left out, and `error` says why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PeerIdentity {
    fn is_valid(&self) -> bool {
        let path = |value: &str, max: usize| {
            !value.is_empty()
                && value.chars().count() <= max
                && !value.chars().any(char::is_control)
        };
        self.exe
            .as_deref()
            .is_none_or(|exe| path(exe, MAX_RECORDED_PEER_PATH_CHARS))
            && self.exe_sha256.as_deref().is_none_or(is_sha256)
            && self.ancestors.len() <= MAX_RECORDED_PEER_ANCESTORS
            && self
                .ancestors
                .iter()
                .all(|ancestor| path(ancestor, MAX_RECORDED_PEER_ANCESTOR_CHARS))
            && self.error.as_deref().is_none_or(|error| {
                !error.is_empty()
                    && error.len() <= 64
                    && error.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
            })
    }
}

#[cfg(test)]
#[path = "network_record_peer_tests.rs"]
mod peer_tests;

/// One record event. The wire form is the contract for the network API and UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkEvent {
    Policy {
        scope: EgressScope,
        revision: u64,
        /// SHA-256 hex of the canonical compiled policy.
        digest: String,
        source: PolicySource,
        /// Rendered rules, such as `"registry.npmjs.org:443 (preset npm)"`.
        rules: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<PolicyChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
    },
    Sidecar {
        state: SidecarState,
        generation: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        container: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Bind {
        /// First 16 hex of SHA-256 of the credential; never the credential.
        token: String,
        binding: EgressBinding,
        scope: EgressScope,
    },
    Unbind {
        token: String,
        reason: UnbindReason,
    },
    Open {
        /// `"g{generation}:{id}"`.
        conn: String,
        /// The program that opened the connection, in Sessions whose init
        /// process reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<PeerIdentity>,
        decision: Decision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        host: String,
        port: u16,
        conn_kind: ConnKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        /// Query stripped, at most 256 characters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default)]
        addrs: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<EgressBinding>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<EgressScope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_revision: Option<u64>,
    },
    Close {
        conn: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ip: Option<String>,
        up: u64,
        down: u64,
        ms: u64,
        outcome: CloseOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// One HTTP request on a route, written before the request is sent
    /// upstream; a refused request is recorded with the reason.
    Request {
        conn: String,
        /// 1 for the connection's first request.
        seq_in_conn: u64,
        method: String,
        /// Query stripped, at most 256 characters.
        path: String,
        /// The request's `Host`.
        host: String,
        /// The route rule that allowed it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        decision: Decision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The name of the credential added; never its value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
    },
    /// How an allowed route request ended.
    Response {
        conn: String,
        seq_in_conn: u64,
        /// The status the client got.
        status: u16,
        /// Request body bytes sent upstream.
        up: u64,
        /// Response body bytes passed to the client.
        down: u64,
        ms: u64,
        outcome: ResponseOutcome,
        /// `Set-Cookie` and `Set-Cookie2` fields a credentialed route removed
        /// from the response's headers and trailers before the client got it.
        #[serde(default, skip_serializing_if = "is_zero")]
        cookies_dropped: u32,
    },
    /// Written before a web tool's request leaves this computer, so a request
    /// whose result cannot be recorded is still in the record. The `web`
    /// event with the same `invocation_id` says how it ended.
    WebRequest {
        tool: WebTool,
        invocation_id: String,
        activation_id: String,
        agent: String,
        /// The URL `web_fetch` requests, bounded to
        /// [`MAX_RECORDED_WEB_URL_BYTES`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        url_truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_bytes: Option<u32>,
    },
    Web {
        tool: WebTool,
        invocation_id: String,
        activation_id: String,
        agent: String,
        decision: Decision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Bounded to [`MAX_RECORDED_WEB_URL_BYTES`], like `final_url` and
        /// each redirect; the `_truncated` flags say when one was cut.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        url_truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_url: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        final_url_truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default)]
        redirects: Vec<String>,
        /// A redirect was cut, or more were followed than are recorded.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redirects_truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_bytes: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        results: Option<u32>,
        #[serde(default)]
        unresponsive_engines: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_sha256: Option<String>,
        #[serde(default)]
        source_ids: Vec<String>,
        /// The URL behind each search result's source id, so a citation can
        /// be joined to its page. URLs are bounded to
        /// [`MAX_RECORDED_SOURCE_URL_BYTES`]. Empty for `web_fetch`, whose
        /// `url` and `final_url` say the same.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<WebSource>,
        retrieved_at_ms: u64,
        ms: u64,
    },
    Limit {
        what: LimitKind,
        detail: String,
    },
    /// One `browser` or `browser_check` call. App traffic inside the browser
    /// container does not pass the egress proxy and is summarized here;
    /// declared-host traffic appears as `open`/`close` under the call's
    /// browser binding.
    Browser {
        tool: BrowserTool,
        invocation_id: String,
        activation_id: String,
        agent: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        /// `browser_check`: the test file run and its status.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        test_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check_status: Option<String>,
        /// Requests the browser could not make because no route allowed them.
        #[serde(default)]
        blocked: u32,
        /// The tag of the egress credential the call used, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        screenshot: Option<ScreenshotRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        screenshot_dropped: Option<String>,
        /// Why the call failed before it produced a result: the runner, the
        /// container or the script failed, or the call was cancelled or ran
        /// out of time. Such a call may still have changed the app's state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        ms: u64,
    },
    /// An Agent asked for a host with `request_network_access`
    /// (`state: pending`), or a person approved or rejected that request.
    /// An approval is also a `policy` event whose `change.proposal_id` names
    /// the proposal; that event is what allows the host.
    Proposal {
        /// `prop_` and 16 hex digits.
        id: String,
        state: ProposalState,
        host: String,
        #[serde(default)]
        ports: Vec<u16>,
        /// The Agent's reason, at most [`MAX_PROPOSAL_REASON_BYTES`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invocation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation_id: Option<String>,
        /// Who decided: always a person (`human`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<String>,
        /// The person's command id for the decision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        /// The `session` policy revision an approval created.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<u64>,
    },
}

impl NetworkEvent {
    /// The kind name used on the wire.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Policy { .. } => "policy",
            Self::Sidecar { .. } => "sidecar",
            Self::Bind { .. } => "bind",
            Self::Unbind { .. } => "unbind",
            Self::Open { .. } => "open",
            Self::Close { .. } => "close",
            Self::Request { .. } => "request",
            Self::Response { .. } => "response",
            Self::WebRequest { .. } => "web_request",
            Self::Web { .. } => "web",
            Self::Limit { .. } => "limit",
            Self::Browser { .. } => "browser",
            Self::Proposal { .. } => "proposal",
        }
    }

    /// The egress sidecar generation the event names: a `sidecar` event's
    /// `generation`, or the `g{generation}` an `open` or `close` event's
    /// `conn` starts with. A reopened Session's sidecar takes the next one.
    pub fn generation(&self) -> Option<u32> {
        match self {
            Self::Sidecar { generation, .. } => Some(*generation),
            Self::Open { conn, .. } | Self::Close { conn, .. } => conn
                .strip_prefix('g')
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(generation, _)| generation.parse().ok()),
            _ => None,
        }
    }

    /// Bounds a writer must respect. Token fields must be tags, never secrets.
    pub fn validate(&self) -> Result<(), NetworkRecordError> {
        fn tag(value: &str) -> bool {
            value.len() == 16
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        let invalid = |reason: &'static str| Err(NetworkRecordError::InvalidEvent(reason));
        match self {
            Self::Bind { token, .. } | Self::Unbind { token, .. } if !tag(token) => {
                invalid("token must be a 16-hex tag")
            }
            Self::Open {
                token: Some(token), ..
            } if !tag(token) => invalid("token must be a 16-hex tag"),
            Self::Open { host, .. } if host.is_empty() || host.len() > 253 => {
                invalid("host must be 1-253 bytes")
            }
            Self::Open {
                path: Some(path), ..
            } if path.chars().count() > MAX_RECORDED_PATH_CHARS => {
                invalid("path must be at most 256 characters")
            }
            Self::Open { addrs, .. } if addrs.len() > MAX_RECORDED_ADDRS => {
                invalid("at most 16 addresses")
            }
            Self::Open {
                peer: Some(peer), ..
            } if !peer.is_valid() => invalid("peer identity is out of bounds"),
            Self::Open { conn, .. } | Self::Close { conn, .. }
                if conn.is_empty() || conn.len() > 32 =>
            {
                invalid("conn must be 1-32 bytes")
            }
            Self::Request { conn, .. } | Self::Response { conn, .. }
                if conn.is_empty() || conn.len() > 32 =>
            {
                invalid("conn must be 1-32 bytes")
            }
            Self::Request {
                method,
                path,
                host,
                rule,
                reason,
                credential,
                ..
            } => {
                let long = |value: &Option<String>| {
                    value.as_ref().is_some_and(|value| {
                        value.chars().count() > MAX_RECORDED_REQUEST_LABEL_CHARS
                    })
                };
                if host.is_empty() || host.len() > 253 {
                    invalid("host must be 1-253 bytes")
                } else if method.is_empty() || method.chars().count() > MAX_RECORDED_METHOD_CHARS {
                    invalid("method must be 1-32 characters")
                } else if path.chars().count() > MAX_RECORDED_PATH_CHARS {
                    invalid("path must be at most 256 characters")
                } else if long(rule) || long(reason) || long(credential) {
                    invalid(
                        "request rule, reason and credential name must be at most 128 characters",
                    )
                } else {
                    Ok(())
                }
            }
            Self::Policy {
                change:
                    Some(PolicyChange {
                        command_id: Some(command_id),
                        ..
                    }),
                ..
            } if command_id.is_empty() || command_id.len() > 128 => {
                invalid("command_id must be 1-128 bytes")
            }
            Self::Policy {
                change:
                    Some(PolicyChange {
                        proposal_id: Some(id),
                        ..
                    }),
                ..
            } if !is_proposal_id(id) => invalid("proposal_id must be prop_ and 16 hex digits"),
            Self::Proposal {
                id,
                host,
                ports,
                reason,
                agent,
                invocation_id,
                activation_id,
                actor,
                command_id,
                ..
            } => {
                let long = |value: &Option<String>, max: usize| {
                    value.as_ref().is_some_and(|value| value.len() > max)
                };
                if !is_proposal_id(id) {
                    invalid("proposal id must be prop_ and 16 hex digits")
                } else if host.is_empty() || host.len() > 253 {
                    invalid("host must be 1-253 bytes")
                } else if ports.is_empty() || ports.len() > MAX_PROPOSAL_PORTS {
                    invalid("a proposal names 1-16 ports")
                } else if long(reason, MAX_PROPOSAL_REASON_BYTES) {
                    invalid("a proposal's reason must be at most 1024 bytes")
                } else if long(agent, 128)
                    || long(invocation_id, 128)
                    || long(activation_id, 128)
                    || long(actor, 128)
                    || long(command_id, 128)
                {
                    invalid("proposal identities must be at most 128 bytes")
                } else {
                    Ok(())
                }
            }
            Self::Browser {
                url,
                final_url,
                test_path,
                check_status,
                screenshot_dropped,
                token,
                error,
                ..
            } => {
                let long = |value: &Option<String>, max: usize| {
                    value
                        .as_ref()
                        .is_some_and(|value| value.chars().count() > max)
                };
                if long(url, MAX_RECORDED_URL_CHARS) || long(final_url, MAX_RECORDED_URL_CHARS) {
                    return invalid("browser URLs must be at most 2048 characters");
                }
                if long(test_path, 512) || long(check_status, 32) || long(screenshot_dropped, 200) {
                    return invalid("browser test path or status is too long");
                }
                if long(error, MAX_BROWSER_ERROR_CHARS) {
                    return invalid("a browser failure reason must be at most 500 characters");
                }
                if token.as_ref().is_some_and(|token| !tag(token)) {
                    return invalid("token must be a 16-hex tag");
                }
                match self {
                    Self::Browser {
                        screenshot: Some(shot),
                        ..
                    } if !is_sha256(&shot.sha256)
                        || screenshot_extension(&shot.media_type).is_none()
                        || shot.bytes as usize > MAX_SCREENSHOT_BYTES =>
                    {
                        invalid("screenshot must name a stored JPEG or PNG by SHA-256")
                    }
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }
}

/// One stored line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkLine {
    pub v: u32,
    pub seq: u64,
    pub ts_ms: u64,
    pub event: NetworkEvent,
}

/// Counts for the API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStats {
    pub events: u64,
    /// Bytes of the recorded events' lines.
    pub bytes: u64,
    pub last_seq: u64,
    /// Places where `seq` skipped a value.
    pub gaps: u64,
    /// The highest egress sidecar generation the record names
    /// ([`NetworkEvent::generation`]).
    pub max_generation: u32,
}

impl RecordStats {
    fn of(tally: &Tally) -> Self {
        Self {
            events: tally.totals.events,
            bytes: tally.totals.bytes,
            last_seq: tally.last_seq,
            gaps: tally.totals.gaps,
            max_generation: tally.totals.max_generation,
        }
    }
}

/// Lines of some event kinds, from [`NetworkRecord::read_kinds_after`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindPage {
    /// Matching lines, oldest first.
    pub lines: Vec<NetworkLine>,
    /// Every line through this sequence number was searched; pass it back as
    /// `after` to continue.
    pub next_after: Option<u64>,
    /// The search reached the end of the record.
    pub done: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkRecordError {
    #[error("network record I/O: {0}")]
    Io(#[from] io::Error),
    #[error("network record line is longer than 16 KiB")]
    LineTooLong,
    #[error("invalid network event: {0}")]
    InvalidEvent(&'static str),
    #[error("network record is damaged at line {line}: {reason}")]
    Damaged { line: u64, reason: String },
    #[error("network record write is uncertain; reopen it")]
    Poisoned,
}

/// A sealed segment's events, by sequence number: `(seq, start, end)` of
/// each line in its file, newline excluded.
struct CachedSegment {
    index: u64,
    lines: Vec<(u64, u32, u32)>,
}

/// Single-writer handle to one Session's record.
pub struct NetworkRecord {
    namespace: OwnedExecutionNamespace,
    limits: SegmentLimits,
    /// The Session journal every segment header names.
    meta: serde_json::Value,
    /// Append handle to the active segment.
    file: File,
    header: Header,
    header_len: u64,
    /// `(seq, offset)` of each event in the active segment.
    active_index: Vec<(u64, u32)>,
    active_len: u64,
    /// The sealed segments' directory, once one exists.
    segments: Option<SecureDir>,
    /// The same directory for writing, once this writer sealed a segment.
    segment_namespace: Option<OwnedExecutionNamespace>,
    /// One summary per sealed segment.
    sealed: Vec<Sealed>,
    /// The whole record's totals.
    tally: Tally,
    dirty: bool,
    poisoned: bool,
    /// The sealed segment read last, so paging through it reads only the
    /// lines asked for.
    cache: Mutex<Option<CachedSegment>>,
}

impl std::fmt::Debug for NetworkRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NetworkRecord")
            .field("stats", &self.stats())
            .field("sealed", &self.sealed.len())
            .finish_non_exhaustive()
    }
}

/// A record in Axocoatl 1.2.0's single file, as loaded.
struct Loaded {
    index: Vec<(u64, u64)>,
    good_bytes: u64,
    torn_bytes: u64,
    tally: Tally,
}

fn load(bytes: &[u8]) -> Result<Loaded, NetworkRecordError> {
    let mut loaded = Loaded {
        index: Vec::new(),
        good_bytes: 0,
        torn_bytes: 0,
        tally: Tally::default(),
    };
    let mut position = 0usize;
    let mut line_number = 0u64;
    while position < bytes.len() {
        line_number += 1;
        let Some(length) = bytes[position..].iter().position(|byte| *byte == b'\n') else {
            loaded.torn_bytes = (bytes.len() - position) as u64;
            break;
        };
        let end = position + length;
        let is_last = end + 1 == bytes.len();
        let Some(line) = segments::parse_any(&bytes[position..end]) else {
            if is_last {
                loaded.torn_bytes = (bytes.len() - position) as u64;
                break;
            }
            return Err(NetworkRecordError::Damaged {
                line: line_number,
                reason: "unparseable line before the end of the record".into(),
            });
        };
        let counted = (line.v == NETWORK_RECORD_VERSION)
            .then(|| {
                loaded
                    .tally
                    .count(line.seq, length + 1, line.event.generation())
                    .ok()
            })
            .flatten();
        if counted.is_none() {
            return Err(NetworkRecordError::Damaged {
                line: line_number,
                reason: "unknown version or non-increasing sequence".into(),
            });
        }
        loaded.index.push((line.seq, position as u64));
        position = end + 1;
        loaded.good_bytes = position as u64;
    }
    Ok(loaded)
}

/// The newest lines of event kind `kind` in `bytes` that `keep` accepts, at
/// most `max`, oldest first. See [`NetworkRecord::read_existing_matching`].
fn matching_lines(
    bytes: &[u8],
    kind: &str,
    keep: impl Fn(&NetworkEvent) -> bool,
    max: usize,
) -> Result<Vec<NetworkLine>, NetworkRecordError> {
    // Complete lines end with a newline; anything after the last one is a
    // torn tail.
    let complete = memchr::memrchr(b'\n', bytes).map_or(0, |end| end + 1);
    let body = &bytes[..complete];
    let needle = format!("\"kind\":\"{kind}\"");
    let mut newest_first = Vec::new();
    // The start of the line already read, so a second match inside it is
    // skipped.
    let mut read_from = body.len();
    for found in memchr::memmem::rfind_iter(body, needle.as_bytes()) {
        if newest_first.len() >= max {
            break;
        }
        if found >= read_from {
            continue;
        }
        let start = memchr::memrchr(b'\n', &body[..found]).map_or(0, |newline| newline + 1);
        let end = found + memchr::memchr(b'\n', &body[found..]).unwrap_or(body.len() - found);
        read_from = start;
        let Some(line) = segments::parse_line(&body[start..end]) else {
            // An unparseable last line is an interrupted append.
            if end + 1 == complete {
                continue;
            }
            return Err(NetworkRecordError::Damaged {
                line: 0,
                reason: format!("unparseable {kind} line before the end of the record"),
            });
        };
        // The text can also match a nested `kind` field of another event.
        if line.event.kind() == kind && keep(&line.event) {
            newest_first.push(line);
        }
    }
    newest_first.reverse();
    Ok(newest_first)
}

/// What the Session journal is, as every segment header names it.
fn record_meta(identity: &crate::execution_store::DurableSessionIdentity) -> serde_json::Value {
    serde_json::json!({
        "journal_id": identity.journal_id(),
        "owner": identity.owner(),
    })
}

fn recovered(torn_bytes: u64) -> NetworkEvent {
    NetworkEvent::Sidecar {
        state: SidecarState::Recovered,
        generation: 0,
        container: None,
        detail: Some(format!("torn {torn_bytes} bytes")),
    }
}

fn encode(seq: u64, ts_ms: u64, event: NetworkEvent) -> Result<Vec<u8>, NetworkRecordError> {
    let mut line = serde_json::to_vec(&NetworkLine {
        v: NETWORK_RECORD_VERSION,
        seq,
        ts_ms,
        event,
    })
    .map_err(io::Error::other)?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(NetworkRecordError::LineTooLong);
    }
    Ok(line)
}

/// Read `from..to` of a file.
fn read_range(mut file: File, from: u64, to: u64) -> Result<Vec<u8>, NetworkRecordError> {
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = vec![0; (to - from) as usize];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn parse_all(bytes: &[u8]) -> Result<Vec<NetworkLine>, NetworkRecordError> {
    segments::lines(bytes)
        .map(|(_, line)| {
            segments::parse_line(line).ok_or_else(|| damaged("a recorded line does not parse"))
        })
        .collect()
}

impl NetworkRecord {
    /// Open or create the record in its component namespace, recovering a
    /// torn tail or an interrupted seal, and moving a record Axocoatl 1.2.0
    /// kept in one file into segments.
    pub fn open(namespace: OwnedExecutionNamespace) -> Result<Self, NetworkRecordError> {
        Self::open_with(namespace, SegmentLimits::default())
    }

    /// [`Self::open`], sealing the active segment at `limits` instead of the
    /// defaults. Segments written with other limits read the same.
    pub fn open_with(
        namespace: OwnedExecutionNamespace,
        limits: SegmentLimits,
    ) -> Result<Self, NetworkRecordError> {
        let limits = limits.validate()?;
        namespace.require_root(&ExecutionComponent::NetworkRecord)?;
        let meta = record_meta(namespace.identity());
        let primary = Path::new(NETWORK_RECORD_FILE);
        let bytes = match namespace.read_limited(primary, segments::LEGACY_FILE_CEILING) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // An initialized component whose file is gone must never look
                // like a new, empty record.
                namespace.check_journal_creation(primary)?;
                // A new record starts as Axocoatl 1.2.0's empty single file
                // and is moved into segments below like any other.
                namespace.open_append(primary)?.sync_all()?;
                Vec::new()
            }
            Err(error) => return Err(error.into()),
        };
        namespace.mark_journal_initialized(primary)?;
        if !segments::is_head(&bytes)? {
            let loaded = load(&bytes)?;
            let mut legacy = bytes[..loaded.good_bytes as usize].to_vec();
            if loaded.torn_bytes > 0 {
                legacy.extend(encode(
                    loaded.tally.last_seq + 1,
                    now_ms(),
                    recovered(loaded.torn_bytes),
                )?);
            }
            segments::migrate(&namespace, NETWORK_RECORD_FILE, &legacy, &meta, limits)?;
        }
        Self::open_segments(namespace, meta, limits)
    }

    fn open_segments(
        namespace: OwnedExecutionNamespace,
        meta: serde_json::Value,
        limits: SegmentLimits,
    ) -> Result<Self, NetworkRecordError> {
        let root = namespace.secure_dir()?;
        let segment_dir = segments::segments_dir(&root)?;
        let count = segments::sealed_count(segment_dir.as_ref())?;
        let mut chain = Chain::default();
        let mut sealed = Vec::with_capacity(count as usize);
        if let Some(segment_dir) = &segment_dir {
            for index in 0..count {
                let read = segments::read_sealed(segment_dir, index)?;
                sealed.push(segments::verify_sealed(&read, &mut chain, &meta)?);
            }
        }
        let bytes = match namespace.read_limited(ACTIVE_FILE, segments::SEGMENT_FILE_CEILING) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(damaged("the active segment is missing"));
            }
            Err(error) => return Err(error.into()),
        };
        let mut active = segments::read_active(&bytes)?;
        let mut torn_bytes = active.torn_bytes as u64;
        match &segment_dir {
            Some(segment_dir) if count > 0 && active.header.index + 1 == count => {
                // The last segment was sealed from this active segment, which
                // was not replaced yet. The sealed copy holds every event the
                // active segment holds, and any whose write the system lost
                // before it was synced.
                let last = segments::read_sealed(segment_dir, count - 1)?;
                if !last.bytes[..last.events.end].starts_with(&bytes[..active.good_len]) {
                    return Err(damaged(
                        "the active segment differs from the segment sealed from it",
                    ));
                }
                // Opening counted the sealed events without parsing them; the
                // next header also needs the generations this one names.
                let last_generation = segments::tally_sealed(&last)?.totals.max_generation;
                chain.tally.totals.max_generation =
                    chain.tally.totals.max_generation.max(last_generation);
                let line = segments::header_line(&chain.header(&meta))?;
                namespace.atomic_write(ACTIVE_FILE, &line)?;
                active = segments::read_active(&line)?;
                torn_bytes = 0;
            }
            _ => chain.check(&active.header, &meta)?,
        }
        let file = namespace.open_append(ACTIVE_FILE)?;
        if torn_bytes > 0 {
            file.set_len(active.good_len as u64)?;
        }
        file.sync_all()?;
        let mut record = Self {
            namespace,
            limits,
            meta,
            file,
            header: active.header,
            header_len: active.header_len as u64,
            active_index: active.index,
            active_len: active.good_len as u64,
            segments: segment_dir,
            segment_namespace: None,
            sealed,
            tally: active.tally,
            dirty: false,
            poisoned: false,
            cache: Mutex::new(None),
        };
        if torn_bytes > 0 {
            record.append(now_ms(), recovered(torn_bytes))?;
            record.sync()?;
        }
        Ok(record)
    }

    /// Append an event and return its sequence number once its line is
    /// written. A full active segment is sealed first.
    pub fn append(&mut self, ts_ms: u64, event: NetworkEvent) -> Result<u64, NetworkRecordError> {
        if self.poisoned {
            return Err(NetworkRecordError::Poisoned);
        }
        event.validate()?;
        let seq = self.tally.last_seq + 1;
        let generation = event.generation();
        let line = encode(seq, ts_ms, event)?;
        if self.limits.full(
            self.active_index.len() as u64,
            self.active_len - self.header_len,
        ) {
            self.seal()?;
        }
        if let Err(error) = self.file.write_all(&line) {
            // A partial line may now end the file; only a reopen may decide.
            self.poisoned = true;
            return Err(error.into());
        }
        self.active_index.push((seq, self.active_len as u32));
        self.active_len += line.len() as u64;
        self.tally
            .count(seq, line.len(), generation)
            .map_err(damaged)?;
        self.dirty = true;
        Ok(seq)
    }

    /// Seal the active segment and start the next one. The sealed copy is
    /// published and synced first, so every event written to the active
    /// segment is durable in it; the new active segment then replaces the
    /// old one. A crash between the two is completed by the next open. Any
    /// failure leaves the record refusing writes until it is reopened.
    fn seal(&mut self) -> Result<(), NetworkRecordError> {
        let result = self.seal_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn seal_inner(&mut self) -> Result<(), NetworkRecordError> {
        let bytes = self
            .namespace
            .read_limited(ACTIVE_FILE, segments::SEGMENT_FILE_CEILING)?;
        if bytes.len() as u64 != self.active_len {
            return Err(damaged("the active segment changed while it was open"));
        }
        let (sealed_bytes, digest) = segments::sealed_file(&bytes, self.active_index.len() as u64)?;
        if self.segment_namespace.is_none() {
            self.segment_namespace = Some(self.namespace.child(SEGMENTS_DIR)?);
        }
        let segment_dir = self
            .segment_namespace
            .as_ref()
            .ok_or_else(|| damaged("the sealed segments are missing"))?;
        segment_dir.atomic_write(segments::sealed_name(self.header.index), &sealed_bytes)?;
        if self.segments.is_none() {
            self.segments = Some(segment_dir.secure_dir()?);
        }
        let chain = Chain {
            index: self.header.index + 1,
            previous: Some(digest.clone()),
            tally: self.tally,
        };
        let header = chain.header(&self.meta);
        let line = segments::header_line(&header)?;
        self.namespace.atomic_write(ACTIVE_FILE, &line)?;
        self.file = self.namespace.open_append(ACTIVE_FILE)?;
        self.sealed.push(Sealed {
            index: self.header.index,
            last_seq: self.tally.last_seq,
            digest,
        });
        self.header = header;
        self.header_len = line.len() as u64;
        self.active_index.clear();
        self.active_len = line.len() as u64;
        self.dirty = false;
        Ok(())
    }

    /// Flush completed appends to stable storage and re-check the record's
    /// ownership. Cheap when nothing changed.
    pub fn sync(&mut self) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("network record write is uncertain"));
        }
        if self.dirty {
            if let Err(error) = self.file.sync_data() {
                self.poisoned = true;
                return Err(error);
            }
            self.dirty = false;
        }
        self.namespace.verify_ambient_identity()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Lines with `seq > after`, at most `limit` (capped at 1000).
    pub fn read_after(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<NetworkLine>, NetworkRecordError> {
        let limit = limit.min(MAX_READ_LIMIT);
        let after = after.unwrap_or(0);
        let mut lines = Vec::new();
        if limit == 0 {
            return Ok(lines);
        }
        self.namespace.verify_ambient_identity()?;
        let first = self
            .sealed
            .partition_point(|segment| segment.last_seq <= after);
        for segment in &self.sealed[first..] {
            if lines.len() >= limit {
                return Ok(lines);
            }
            self.sealed_lines_after(segment, after, limit, &mut lines)?;
        }
        let start = self.active_index.partition_point(|(seq, _)| *seq <= after);
        let end = (start + limit - lines.len()).min(self.active_index.len());
        if start < end {
            let from = self.active_index[start].1 as u64;
            let to = self
                .active_index
                .get(end)
                .map_or(self.active_len, |(_, offset)| *offset as u64);
            let file = self
                .namespace
                .open_read(ACTIVE_FILE, segments::SEGMENT_FILE_CEILING)?;
            lines.extend(parse_all(&read_range(file, from, to)?)?);
        }
        Ok(lines)
    }

    /// Lines of one sealed segment with `seq > after`, until `lines` holds
    /// `limit`. The segment is read whole and checked against its digest
    /// once; later pages of it read only their lines.
    fn sealed_lines_after(
        &self,
        segment: &Sealed,
        after: u64,
        limit: usize,
        lines: &mut Vec<NetworkLine>,
    ) -> Result<(), NetworkRecordError> {
        let segment_dir = self
            .segments
            .as_ref()
            .ok_or_else(|| damaged("the sealed segments are missing"))?;
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| damaged("the segment cache lock was poisoned"))?;
        let wanted = |cached: &CachedSegment| {
            let start = cached.lines.partition_point(|(seq, _, _)| *seq <= after);
            start..(start + limit - lines.len()).min(cached.lines.len())
        };
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.index == segment.index)
        {
            let range = wanted(cached);
            if range.is_empty() {
                return Ok(());
            }
            let from = cached.lines[range.start].1 as u64;
            let to = cached.lines[range.end - 1].2 as u64;
            let file = segment_dir.open_file_limited(
                segments::sealed_name(segment.index),
                segments::SEGMENT_FILE_CEILING,
            )?;
            let bytes = read_range(file, from, to)?;
            for &(seq, start, end) in &cached.lines[range] {
                let line = segments::parse_line(
                    &bytes[(start as u64 - from) as usize..(end as u64 - from) as usize],
                )
                .filter(|line| line.seq == seq)
                .ok_or_else(|| damaged("a sealed segment changed after it was opened"))?;
                lines.push(line);
            }
            return Ok(());
        }
        let read = segments::read_sealed(segment_dir, segment.index)?;
        if read.digest != segment.digest {
            return Err(damaged("a sealed segment changed after it was opened"));
        }
        let base = read.events.start;
        let mut index = Vec::new();
        for (at, line) in segments::lines(read.region()) {
            let seq = match segments::line_seq(line) {
                Some(seq) => seq,
                None => {
                    segments::parse_line(line)
                        .ok_or_else(|| damaged("a recorded line does not parse"))?
                        .seq
                }
            };
            let start = (base + at) as u32;
            index.push((seq, start, start + line.len() as u32));
        }
        let cached = CachedSegment {
            index: segment.index,
            lines: index,
        };
        for &(_, start, end) in &cached.lines[wanted(&cached)] {
            lines.push(
                segments::parse_line(&read.bytes[start as usize..end as usize])
                    .ok_or_else(|| damaged("a recorded line does not parse"))?,
            );
        }
        *cache = Some(cached);
        Ok(())
    }

    /// The lines with `seq > after` whose event is one of `kinds`, at most
    /// `max` (capped at 1000), searching at most one segment per call, so a
    /// caller can go through the whole record while appends continue
    /// between calls. Only lines that may be of those kinds are parsed (see
    /// [`Self::read_existing_matching`]).
    pub fn read_kinds_after(
        &self,
        after: Option<u64>,
        kinds: &[&str],
        max: usize,
    ) -> Result<KindPage, NetworkRecordError> {
        self.namespace.verify_ambient_identity()?;
        let max = max.clamp(1, MAX_READ_LIMIT);
        let from = after.unwrap_or(0);
        let mut lines = Vec::new();
        let first = self
            .sealed
            .partition_point(|segment| segment.last_seq <= from);
        if let Some(segment) = self.sealed.get(first) {
            let segment_dir = self
                .segments
                .as_ref()
                .ok_or_else(|| damaged("the sealed segments are missing"))?;
            let read = segments::read_sealed(segment_dir, segment.index)?;
            if read.digest != segment.digest {
                return Err(damaged("a sealed segment changed after it was opened"));
            }
            let stopped = segments::kind_lines_after(read.region(), kinds, from, max, &mut lines)?;
            return Ok(KindPage {
                lines,
                next_after: Some(stopped.unwrap_or(segment.last_seq)),
                done: false,
            });
        }
        let start = self.active_index.partition_point(|(seq, _)| *seq <= from);
        if let Some((_, offset)) = self.active_index.get(start) {
            let file = self
                .namespace
                .open_read(ACTIVE_FILE, segments::SEGMENT_FILE_CEILING)?;
            let bytes = read_range(file, *offset as u64, self.active_len)?;
            if let Some(seq) = segments::kind_lines_after(&bytes, kinds, from, max, &mut lines)? {
                return Ok(KindPage {
                    lines,
                    next_after: Some(seq),
                    done: false,
                });
            }
        }
        Ok(KindPage {
            lines,
            next_after: (self.tally.last_seq > 0 || after.is_some())
                .then_some(self.tally.last_seq.max(from)),
            done: true,
        })
    }

    /// Read a record without opening a writer: no lock, no recovery, no
    /// creation. A torn tail is skipped, not cut. `Ok(None)` when the Session
    /// has no record.
    pub fn read_existing(
        canonical: &crate::execution_store::SessionExecutionStore,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Option<(Vec<NetworkLine>, RecordStats)>, NetworkRecordError> {
        let limit = limit.min(MAX_READ_LIMIT);
        let after = after.unwrap_or(0);
        match Existing::open(canonical)? {
            None => Ok(None),
            Some(Existing::Single(bytes)) => {
                let loaded = load(&bytes)?;
                let start = loaded.index.partition_point(|(seq, _)| *seq <= after);
                let lines = loaded.index[start..]
                    .iter()
                    .take(limit)
                    .map(|(_, offset)| {
                        let from = *offset as usize;
                        let to = from
                            + memchr::memchr(b'\n', &bytes[from..]).unwrap_or(bytes.len() - from);
                        segments::parse_line(&bytes[from..to])
                            .ok_or_else(|| damaged("a recorded line does not parse"))
                    })
                    .collect::<Result<Vec<NetworkLine>, _>>()?;
                Ok(Some((lines, RecordStats::of(&loaded.tally))))
            }
            Some(Existing::Segmented(snapshot)) => {
                let lines = snapshot.lines_after(after, limit)?;
                Ok(Some((lines, RecordStats::of(&snapshot.tally))))
            }
        }
    }

    /// The newest stored lines of event kind `kind` that `keep` accepts, at
    /// most `max`, oldest first, read without opening a writer. Like
    /// [`Self::read_existing`] it takes no lock, recovers nothing, creates
    /// nothing and skips a torn tail. `Ok(None)` when the Session has no
    /// record. Segments are read newest first, one at a time, until `max`
    /// lines are found.
    ///
    /// Only lines that may be of that kind are parsed: each segment is
    /// searched once for `"kind":"<kind>"`, which the writer's compact JSON
    /// puts at the start of every event of that kind (an escaped string value
    /// cannot contain it), and each match is parsed and its kind checked.
    /// Other lines are not validated, so a damaged line of another kind is
    /// not reported here; opening the writer still refuses one.
    pub fn read_existing_matching(
        canonical: &crate::execution_store::SessionExecutionStore,
        kind: &str,
        keep: impl Fn(&NetworkEvent) -> bool,
        max: usize,
    ) -> Result<Option<Vec<NetworkLine>>, NetworkRecordError> {
        match Existing::open(canonical)? {
            None => Ok(None),
            Some(Existing::Single(bytes)) => Ok(Some(matching_lines(&bytes, kind, keep, max)?)),
            Some(Existing::Segmented(snapshot)) => Ok(Some(snapshot.matching(kind, &keep, max)?)),
        }
    }

    /// Keep a screenshot beside the record, named by its SHA-256. The bytes
    /// must be a JPEG or PNG of at most [`MAX_SCREENSHOT_BYTES`]. Storing
    /// the same bytes again returns the same reference. Nothing limits how
    /// many a Session keeps, and nothing lists them.
    pub fn store_screenshot(
        &mut self,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<ScreenshotRef, NetworkRecordError> {
        let extension = screenshot_extension(media_type).ok_or(
            NetworkRecordError::InvalidEvent("screenshots are JPEG or PNG"),
        )?;
        let magic = match extension {
            "jpg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
            _ => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        };
        if !magic || bytes.len() > MAX_SCREENSHOT_BYTES {
            return Err(NetworkRecordError::InvalidEvent(
                "a screenshot must be a JPEG or PNG of at most 1 MiB",
            ));
        }
        if self.poisoned {
            return Err(NetworkRecordError::Poisoned);
        }
        use sha2::Digest;
        let sha256 = format!("{:x}", sha2::Sha256::digest(bytes));
        let name = format!("{sha256}.{extension}");
        let directory = self.namespace.child(SCREENSHOT_DIR)?;
        let reference = ScreenshotRef {
            sha256,
            media_type: media_type.to_string(),
            bytes: bytes.len() as u64,
        };
        if directory.is_file(&name)? {
            return Ok(reference);
        }
        directory.atomic_write(&name, bytes)?;
        directory.sync_all()?;
        Ok(reference)
    }

    /// Read a stored screenshot without opening a writer. `Ok(None)` when the
    /// Session has no record or no such screenshot.
    pub fn read_screenshot_existing(
        canonical: &crate::execution_store::SessionExecutionStore,
        sha256: &str,
    ) -> Result<Option<(String, Vec<u8>)>, NetworkRecordError> {
        if !is_sha256(sha256) {
            return Ok(None);
        }
        for media_type in ["image/jpeg", "image/png"] {
            let name = format!(
                "{sha256}.{}",
                screenshot_extension(media_type).unwrap_or("x")
            );
            match canonical.read_existing_component_file(
                &ExecutionComponent::NetworkRecord,
                Path::new(NETWORK_RECORD_FILE),
                Path::new(SCREENSHOT_DIR),
                Path::new(&name),
                MAX_SCREENSHOT_BYTES,
            ) {
                Ok(bytes) => return Ok(Some((media_type.to_string(), bytes))),
                Err(crate::execution_store::ExecutionStoreError::Io(error))
                    if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io::Error::other(error.to_string()).into()),
            }
        }
        Ok(None)
    }

    pub fn stats(&self) -> RecordStats {
        RecordStats::of(&self.tally)
    }

    /// How many segments are sealed.
    pub fn sealed_segments(&self) -> usize {
        self.sealed.len()
    }
}

impl Drop for NetworkRecord {
    fn drop(&mut self) {
        if self.dirty && !self.poisoned {
            let _ = self.file.sync_data();
        }
    }
}

/// Times a reader starts over when a writer sealed a segment under it.
const READ_ATTEMPTS: usize = 4;

/// A record as a reader without a writer finds it.
enum Existing {
    /// Axocoatl 1.2.0's single file, not migrated yet.
    Single(Vec<u8>),
    Segmented(Box<Snapshot>),
}

/// The segmented record at one moment: how many segments were sealed, the
/// active segment, and the totals through its last complete event.
struct Snapshot {
    segments: Option<SecureDir>,
    count: u64,
    active_bytes: Vec<u8>,
    /// `None` when the active segment was sealed and not replaced yet: its
    /// events are in the last sealed segment.
    active: Option<segments::ActiveRead>,
    tally: Tally,
}

enum Attempt {
    Changed,
    Failed(NetworkRecordError),
}

impl From<NetworkRecordError> for Attempt {
    fn from(error: NetworkRecordError) -> Self {
        Self::Failed(error)
    }
}

impl From<io::Error> for Attempt {
    fn from(error: io::Error) -> Self {
        Self::Failed(error.into())
    }
}

impl Existing {
    fn open(
        canonical: &crate::execution_store::SessionExecutionStore,
    ) -> Result<Option<Self>, NetworkRecordError> {
        let not_found = |error: &crate::execution_store::ExecutionStoreError| {
            matches!(error, crate::execution_store::ExecutionStoreError::Io(error)
                if error.kind() == io::ErrorKind::NotFound)
        };
        let root = match canonical.existing_component_root(
            &ExecutionComponent::NetworkRecord,
            Path::new(NETWORK_RECORD_FILE),
        ) {
            Ok(root) => root,
            Err(error) if not_found(&error) => return Ok(None),
            Err(error) => return Err(io::Error::other(error.to_string()).into()),
        };
        let meta = record_meta(
            &canonical
                .identity()
                .map_err(|error| io::Error::other(error.to_string()))?,
        );
        for _ in 0..READ_ATTEMPTS {
            let primary =
                match root.read_limited(NETWORK_RECORD_FILE, segments::LEGACY_FILE_CEILING) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
            if !segments::is_head(&primary)? {
                return Ok(Some(Self::Single(primary)));
            }
            match Snapshot::take(&root, &meta) {
                Ok(snapshot) => {
                    root.verify_ambient_identity()?;
                    canonical
                        .identity()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    return Ok(Some(Self::Segmented(Box::new(snapshot))));
                }
                Err(Attempt::Changed) => continue,
                Err(Attempt::Failed(error)) => return Err(error),
            }
        }
        Err(damaged("the record kept changing while it was being read"))
    }
}

impl Snapshot {
    fn take(root: &SecureDir, meta: &serde_json::Value) -> Result<Self, Attempt> {
        let segment_dir = segments::segments_dir(root)?;
        let count = segments::sealed_count(segment_dir.as_ref())?;
        let active_bytes = match root.read_limited(ACTIVE_FILE, segments::SEGMENT_FILE_CEILING) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(damaged("the active segment is missing").into());
            }
            Err(error) => return Err(error.into()),
        };
        let active = segments::read_active(&active_bytes)?;
        if active.header.meta != *meta {
            return Err(damaged("the active segment belongs to another record").into());
        }
        let index = active.header.index;
        if index == count {
            return Ok(Self {
                segments: segment_dir,
                count,
                tally: active.tally,
                active: Some(active),
                active_bytes,
            });
        }
        if index > count {
            // Sealed after the listing above; start over.
            return Err(Attempt::Changed);
        }
        match &segment_dir {
            Some(segment_dir) if index + 1 == count => {
                // Sealed and not replaced yet: a writer is sealing it, or
                // stopped while it did. The sealed copy holds its events.
                let last = segments::read_sealed(segment_dir, count - 1)?;
                let tally = segments::tally_sealed(&last)?;
                Ok(Self {
                    segments: Some(segment_dir.clone()),
                    count,
                    active_bytes: Vec::new(),
                    active: None,
                    tally,
                })
            }
            _ => Err(damaged("the segment chain is broken").into()),
        }
    }

    /// The active segment's complete events, if it holds the newest ones.
    fn active_region(&self) -> Option<(&segments::ActiveRead, &[u8])> {
        self.active.as_ref().map(|active| {
            (
                active,
                &self.active_bytes[active.header_len..active.good_len],
            )
        })
    }

    fn segment_dir(&self) -> Result<&SecureDir, NetworkRecordError> {
        self.segments
            .as_ref()
            .ok_or_else(|| damaged("the sealed segments are missing"))
    }

    fn lines_after(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<Vec<NetworkLine>, NetworkRecordError> {
        let mut lines = Vec::new();
        if limit == 0 {
            return Ok(lines);
        }
        let active_first = self
            .active
            .as_ref()
            .map_or(self.tally.last_seq + 1, |active| active.header.first_seq);
        let mut previous = None;
        if self.count > 0 && after + 1 < active_first {
            let segment_dir = self.segment_dir()?;
            // The last segment whose first sequence number is at most
            // `after + 1` holds the first line after `after`, if any does.
            let (mut low, mut high) = (0, self.count);
            while low < high {
                let middle = low + (high - low) / 2;
                if segments::read_sealed_header(segment_dir, middle)?.first_seq <= after + 1 {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            for index in low.saturating_sub(1)..self.count {
                if lines.len() >= limit {
                    return Ok(lines);
                }
                let read = segments::read_sealed(segment_dir, index)?;
                if previous.is_some() && read.header.previous != previous {
                    return Err(damaged("the segment chain is broken"));
                }
                segments::lines_after(read.region(), after, limit, &mut lines)?;
                previous = Some(read.digest);
            }
        }
        if let Some((active, region)) = self.active_region() {
            if previous.is_some() && active.header.previous != previous {
                return Err(damaged("the segment chain is broken"));
            }
            segments::lines_after(region, after, limit, &mut lines)?;
        }
        Ok(lines)
    }

    fn matching(
        &self,
        kind: &str,
        keep: &impl Fn(&NetworkEvent) -> bool,
        max: usize,
    ) -> Result<Vec<NetworkLine>, NetworkRecordError> {
        // Newest segment first; each segment's lines oldest first.
        let mut found: Vec<Vec<NetworkLine>> = Vec::new();
        let mut total = 0;
        let mut newer_previous = None;
        if let Some((active, region)) = self.active_region() {
            let lines = matching_lines(region, kind, keep, max)?;
            total += lines.len();
            found.push(lines);
            newer_previous = Some(active.header.previous.clone());
        }
        let mut index = self.count;
        while total < max && index > 0 {
            index -= 1;
            let read = segments::read_sealed(self.segment_dir()?, index)?;
            if let Some(previous) = &newer_previous {
                if previous.as_ref() != Some(&read.digest) {
                    return Err(damaged("the segment chain is broken"));
                }
            }
            let lines = matching_lines(read.region(), kind, keep, max - total)?;
            total += lines.len();
            found.push(lines);
            newer_previous = Some(read.header.previous.clone());
        }
        Ok(found.into_iter().rev().flatten().collect())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
#[path = "network_record_segment_tests.rs"]
mod segment_tests;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
    use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use crate::turn_contract::SessionId;
    use std::sync::Arc;

    pub(super) fn setup() -> (
        tempfile::TempDir,
        Arc<UpgradedFormatOwnership>,
        SessionExecutionStore,
    ) {
        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let store = SessionExecutionStore::open(
            ownership.clone(),
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: SessionId::new("session").unwrap(),
            },
        )
        .unwrap();
        (root, ownership, store)
    }

    pub(super) fn open(store: &SessionExecutionStore) -> NetworkRecord {
        NetworkRecord::open(
            store
                .component_namespace(ExecutionComponent::NetworkRecord)
                .unwrap(),
        )
        .unwrap()
    }

    /// The active segment, where new events are appended.
    pub(super) fn file_path(store: &SessionExecutionStore) -> std::path::PathBuf {
        store
            .path()
            .parent()
            .unwrap()
            .join("network-record")
            .join(ACTIVE_FILE)
    }

    /// Leave `bytes` as Axocoatl 1.2.0 did: the whole record in the primary
    /// file of an initialized component, and nothing beside it.
    pub(super) fn single_file_record(store: &SessionExecutionStore, bytes: &[u8]) {
        drop(open(store));
        let directory = file_path(store).parent().unwrap().to_path_buf();
        std::fs::remove_file(directory.join(ACTIVE_FILE)).unwrap();
        let _ = std::fs::remove_dir_all(directory.join(SEGMENTS_DIR));
        std::fs::write(directory.join(NETWORK_RECORD_FILE), bytes).unwrap();
    }

    /// The active segment's header line, newline included.
    pub(super) fn header_len(store: &SessionExecutionStore) -> usize {
        let bytes = std::fs::read(file_path(store)).unwrap();
        bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1
    }

    pub(super) fn open_event(id: u64, host: &str) -> NetworkEvent {
        NetworkEvent::Open {
            conn: format!("g1:{id}"),
            peer: None,
            decision: Decision::Allow,
            reason: None,
            status: None,
            rule: Some("preset:npm/registry.npmjs.org".into()),
            host: host.into(),
            port: 443,
            conn_kind: ConnKind::Connect,
            method: None,
            path: None,
            addrs: vec!["104.16.0.1".into()],
            token: Some("0123456789abcdef".into()),
            binding: Some(EgressBinding {
                invocation_id: Some("inv-1".into()),
                activation_id: Some("act-1".into()),
                agent: Some("writer".into()),
                ..EgressBinding::new(BindingKind::Agent)
            }),
            scope: Some(EgressScope::Session),
            policy_revision: Some(1),
        }
    }

    pub(super) fn limit_event() -> NetworkEvent {
        NetworkEvent::Limit {
            what: LimitKind::RecordFull,
            detail: "cap reached".into(),
        }
    }

    fn browser_event(screenshot: Option<ScreenshotRef>) -> NetworkEvent {
        NetworkEvent::Browser {
            tool: BrowserTool::Browser,
            invocation_id: "inv-7".into(),
            activation_id: "act-2".into(),
            agent: "qa-scout".into(),
            ok: false,
            url: Some("http://localhost:8765/".into()),
            final_url: Some("http://localhost:8765/cart".into()),
            status: Some(200),
            test_path: None,
            check_status: None,
            blocked: 1,
            token: None,
            screenshot,
            screenshot_dropped: None,
            error: None,
            ms: 1834,
        }
    }

    #[test]
    fn browser_events_and_screenshots_are_kept_beside_the_record() {
        let (_root, _ownership, store) = setup();
        let mut record = open(&store);
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg.extend(std::iter::repeat_n(9u8, 1000));
        let shot = record.store_screenshot("image/jpeg", &jpeg).unwrap();
        assert_eq!(shot.bytes, 1004);
        assert_eq!(shot.media_type, "image/jpeg");
        // The same bytes are stored once.
        assert_eq!(record.store_screenshot("image/jpeg", &jpeg).unwrap(), shot);
        let seq = record.append(5, browser_event(Some(shot.clone()))).unwrap();
        let lines = record.read_after(Some(seq - 1), 10).unwrap();
        assert_eq!(lines[0].event, browser_event(Some(shot.clone())));
        let wire = serde_json::to_value(&lines[0]).unwrap();
        assert_eq!(wire["event"]["kind"], "browser");
        assert_eq!(wire["event"]["tool"], "browser");
        assert_eq!(wire["event"]["screenshot"]["sha256"], shot.sha256);
        record.sync().unwrap();
        drop(record);

        let (media, bytes) = NetworkRecord::read_screenshot_existing(&store, &shot.sha256)
            .unwrap()
            .unwrap();
        assert_eq!((media.as_str(), bytes), ("image/jpeg", jpeg));
        assert!(
            NetworkRecord::read_screenshot_existing(&store, &"0".repeat(64))
                .unwrap()
                .is_none()
        );
        assert!(
            NetworkRecord::read_screenshot_existing(&store, "../network-record.v1.jsonl")
                .unwrap()
                .is_none()
        );

        let mut record = open(&store);
        for (media, bytes) in [
            ("image/svg+xml", b"<svg/>".to_vec()),
            ("image/jpeg", b"<svg/>".to_vec()),
            ("image/png", vec![0x89; MAX_SCREENSHOT_BYTES + 1]),
        ] {
            assert!(record.store_screenshot(media, &bytes).is_err(), "{media}");
        }
        let forged = ScreenshotRef {
            sha256: "not-a-digest".into(),
            media_type: "image/jpeg".into(),
            bytes: 4,
        };
        assert!(record.append(6, browser_event(Some(forged))).is_err());
        let mut long = browser_event(None);
        if let NetworkEvent::Browser { url, .. } = &mut long {
            *url = Some(format!(
                "http://localhost/{}",
                "a".repeat(MAX_RECORDED_URL_CHARS)
            ));
        }
        assert!(record.append(7, long).is_err());
    }

    #[test]
    fn screenshots_have_no_count_or_byte_budget() {
        // Axocoatl 1.2.0 counted the screenshots on disk at first use and
        // stopped keeping them at 2000 or 64 MiB. Leave more than both, as
        // a long Session would, and keep storing.
        use sha2::Digest;
        let (_root, _ownership, store) = setup();
        drop(open(&store));
        let directory = file_path(&store).parent().unwrap().join(SCREENSHOT_DIR);
        std::fs::create_dir(&directory).unwrap();
        let jpeg = |n: u32, len: usize| {
            let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0];
            bytes.extend_from_slice(&n.to_le_bytes());
            bytes.resize(len, 7);
            bytes
        };
        let kept = |bytes: &[u8]| {
            std::fs::write(
                directory.join(format!("{:x}.jpg", sha2::Sha256::digest(bytes))),
                bytes,
            )
            .unwrap();
        };
        for n in 0..2_000 {
            kept(&jpeg(n, 64));
        }
        for n in 0..65 {
            kept(&jpeg(10_000 + n, MAX_SCREENSHOT_BYTES));
        }
        let mut record = open(&store);
        let new = jpeg(20_000, 4096);
        let shot = record.store_screenshot("image/jpeg", &new).unwrap();
        assert_eq!(record.store_screenshot("image/jpeg", &new).unwrap(), shot);
        let mut large = b"\x89PNG\r\n\x1a\n".to_vec();
        large.resize(MAX_SCREENSHOT_BYTES, 9);
        let png = record.store_screenshot("image/png", &large).unwrap();
        drop(record);
        let read = |sha256: &str| {
            NetworkRecord::read_screenshot_existing(&store, sha256)
                .unwrap()
                .unwrap()
        };
        assert_eq!(read(&shot.sha256), ("image/jpeg".to_string(), new));
        assert_eq!(read(&png.sha256), ("image/png".to_string(), large));
        let old = jpeg(3, 64);
        assert_eq!(
            read(&format!("{:x}", sha2::Sha256::digest(&old))),
            ("image/jpeg".to_string(), old)
        );
    }

    #[test]
    fn append_and_read_back_in_order() {
        let (_root, _ownership, store) = setup();
        let mut record = open(&store);
        assert_eq!(
            record
                .append(10, open_event(1, "registry.npmjs.org"))
                .unwrap(),
            1
        );
        assert_eq!(
            record
                .append(
                    11,
                    NetworkEvent::Close {
                        conn: "g1:1".into(),
                        ip: Some("104.16.0.1".into()),
                        up: 512,
                        down: 4096,
                        ms: 30,
                        outcome: CloseOutcome::Closed,
                        error: None,
                    },
                )
                .unwrap(),
            2
        );
        assert!(record.is_dirty());
        record.sync().unwrap();
        assert!(!record.is_dirty());
        let lines = record.read_after(None, 200).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].seq, lines[0].ts_ms), (1, 10));
        assert_eq!(lines[0].event, open_event(1, "registry.npmjs.org"));
        assert_eq!(record.read_after(Some(1), 200).unwrap()[0].seq, 2);
        assert!(record.read_after(Some(2), 200).unwrap().is_empty());
        assert_eq!(record.read_after(None, 1).unwrap().len(), 1);
        assert!(record.read_after(None, 0).unwrap().is_empty());

        // The wire form is the documented contract. The active segment
        // starts with its header.
        let text = std::fs::read_to_string(file_path(&store)).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(first["v"], 1);
        assert_eq!(first["event"]["kind"], "open");
        assert_eq!(first["event"]["decision"], "allow");
        assert_eq!(first["event"]["conn_kind"], "connect");
        assert_eq!(first["event"]["binding"]["kind"], "agent");
        assert!(first["event"]["binding"].get("terminal_id").is_none());
        assert!(first["event"].get("reason").is_none());
        let stats = record.stats();
        assert_eq!((stats.events, stats.last_seq, stats.gaps), (2, 2, 0));
        assert_eq!(stats.bytes, (text.len() - header_len(&store)) as u64);
        assert_eq!(stats.max_generation, 1);
    }

    #[test]
    fn read_existing_needs_no_writer_and_skips_a_torn_tail() {
        let (_root, _ownership, store) = setup();
        assert!(NetworkRecord::read_existing(&store, None, 10)
            .unwrap()
            .is_none());
        {
            let mut record = open(&store);
            for id in 0..4 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
            // Works while the writer holds its lock.
            let (lines, stats) = NetworkRecord::read_existing(&store, Some(1), 2)
                .unwrap()
                .unwrap();
            assert_eq!(
                lines.iter().map(|line| line.seq).collect::<Vec<_>>(),
                [2, 3]
            );
            assert_eq!(stats.events, 4);
        }
        let path = file_path(&store);
        let mut bytes = std::fs::read(&path).unwrap();
        let good = bytes.len();
        bytes.extend_from_slice(b"{\"v\":1,");
        std::fs::write(&path, &bytes).unwrap();
        let (lines, stats) = NetworkRecord::read_existing(&store, None, 100)
            .unwrap()
            .unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(stats.bytes, (good - header_len(&store)) as u64);
        // The read changed nothing.
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn sequence_continues_across_reopen() {
        let (_root, _ownership, store) = setup();
        {
            let mut record = open(&store);
            for id in 0..5 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
        }
        let mut record = open(&store);
        assert_eq!(record.stats().last_seq, 5);
        assert_eq!(record.append(9, limit_event()).unwrap(), 6);
        let seqs: Vec<u64> = record
            .read_after(None, 100)
            .unwrap()
            .iter()
            .map(|line| line.seq)
            .collect();
        assert_eq!(seqs, (1..=6).collect::<Vec<_>>());
    }

    #[test]
    fn a_second_writer_is_refused_while_one_is_open() {
        let (_root, _ownership, store) = setup();
        let _record = open(&store);
        assert!(store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .is_err());
    }

    #[test]
    fn torn_tail_is_cut_and_recorded() {
        for tail in [&b"{\"v\":1,\"seq\":4,\"ts"[..], b"not json\n", b"\n"] {
            let (_root, _ownership, store) = setup();
            {
                let mut record = open(&store);
                for id in 0..3 {
                    record.append(id, open_event(id, "a.example")).unwrap();
                }
            }
            let path = file_path(&store);
            let good = std::fs::read(&path).unwrap();
            let mut torn = good.clone();
            torn.extend_from_slice(tail);
            std::fs::write(&path, &torn).unwrap();
            let record = open(&store);
            let lines = record.read_after(None, 100).unwrap();
            assert_eq!(lines.len(), 4, "{tail:?}");
            assert_eq!(lines[3].seq, 4);
            assert_eq!(
                lines[3].event,
                NetworkEvent::Sidecar {
                    state: SidecarState::Recovered,
                    generation: 0,
                    container: None,
                    detail: Some(format!("torn {} bytes", tail.len())),
                }
            );
            let after = std::fs::read(&path).unwrap();
            assert!(after.starts_with(&good));
            assert!(after.ends_with(b"\n"));
        }
    }

    #[test]
    fn damage_before_the_end_refuses_the_open() {
        let (_root, _ownership, store) = setup();
        {
            let mut record = open(&store);
            for id in 0..3 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
        }
        let path = file_path(&store);
        let text = std::fs::read_to_string(&path).unwrap();
        // The header is line 1; the first event, line 2, is damaged.
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "garbage";
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let error = NetworkRecord::open(
            store
                .component_namespace(ExecutionComponent::NetworkRecord)
                .unwrap(),
        )
        .unwrap_err();
        assert!(
            matches!(error, NetworkRecordError::Damaged { line: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_deleted_record_file_is_not_mistaken_for_a_new_record() {
        let (_root, _ownership, store) = setup();
        {
            let mut record = open(&store);
            record.append(1, limit_event()).unwrap();
        }
        let directory = file_path(&store).parent().unwrap().to_path_buf();
        let active = std::fs::read(file_path(&store)).unwrap();
        // Neither the head nor the active segment may go missing.
        std::fs::remove_file(file_path(&store)).unwrap();
        let reopen = || {
            NetworkRecord::open(
                store
                    .component_namespace(ExecutionComponent::NetworkRecord)
                    .unwrap(),
            )
        };
        assert!(reopen().is_err());
        std::fs::write(file_path(&store), active).unwrap();
        std::fs::remove_file(directory.join(NETWORK_RECORD_FILE)).unwrap();
        assert!(reopen().is_err());
    }

    #[test]
    fn sequence_gaps_are_counted_not_renumbered() {
        let (_root, _ownership, store) = setup();
        {
            let mut record = open(&store);
            for id in 0..3 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
        }
        let path = file_path(&store);
        let text = std::fs::read_to_string(&path).unwrap();
        // Line 0 is the header; drop the second event.
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(index, _)| *index != 2)
            .map(|(_, line)| line)
            .collect();
        std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();
        let mut record = open(&store);
        assert_eq!(record.stats().gaps, 1);
        assert_eq!(record.append(5, limit_event()).unwrap(), 4);
        let seqs: Vec<u64> = record
            .read_after(Some(1), 10)
            .unwrap()
            .iter()
            .map(|line| line.seq)
            .collect();
        assert_eq!(seqs, vec![3, 4]);
    }

    #[test]
    fn unknown_kinds_fields_and_secret_tokens_are_refused() {
        for line in [
            r#"{"v":1,"seq":1,"ts_ms":1,"event":{"kind":"exfiltrate","detail":"x"}}"#,
            r#"{"v":1,"seq":1,"ts_ms":1,"event":{"kind":"limit","what":"record_full","detail":"x","extra":1}}"#,
            r#"{"v":1,"seq":1,"ts_ms":1,"event":{"kind":"limit","what":"disk_full","detail":"x"}}"#,
            r#"{"v":1,"seq":1,"ts_ms":1,"extra":true,"event":{"kind":"limit","what":"record_full","detail":"x"}}"#,
        ] {
            assert!(serde_json::from_str::<NetworkLine>(line).is_err(), "{line}");
        }
        let parsed: NetworkLine = serde_json::from_str(
            r#"{"v":1,"seq":1,"ts_ms":1,"event":{"kind":"limit","what":"record_full","detail":"x"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.event, limit_event_with("x"));

        let (_root, _ownership, store) = setup();
        let mut record = open(&store);
        for event in [
            NetworkEvent::Unbind {
                token: "axe_rawsecretvalue".into(),
                reason: UnbindReason::Settled,
            },
            NetworkEvent::Bind {
                token: "0123456789ABCDEF".into(),
                binding: EgressBinding::new(BindingKind::Setup),
                scope: EgressScope::Session,
            },
            with_open(|path, _| *path = Some("x".repeat(257))),
            with_open(|_, addrs| *addrs = vec!["1.1.1.1".into(); 17]),
            open_event(1, ""),
        ] {
            assert!(matches!(
                record.append(1, event),
                Err(NetworkRecordError::InvalidEvent(_))
            ),);
        }
        let huge = NetworkEvent::Limit {
            what: LimitKind::RecordFull,
            detail: "x".repeat(MAX_LINE_BYTES),
        };
        assert!(matches!(
            record.append(1, huge),
            Err(NetworkRecordError::LineTooLong)
        ));
        assert_eq!(record.stats().events, 0);
    }

    fn with_open(change: impl FnOnce(&mut Option<String>, &mut Vec<String>)) -> NetworkEvent {
        let mut event = open_event(1, "a.example");
        if let NetworkEvent::Open { path, addrs, .. } = &mut event {
            change(path, addrs);
        }
        event
    }

    fn limit_event_with(detail: &str) -> NetworkEvent {
        NetworkEvent::Limit {
            what: LimitKind::RecordFull,
            detail: detail.into(),
        }
    }

    #[test]
    fn every_event_kind_round_trips() {
        let events = vec![
            NetworkEvent::Policy {
                scope: EgressScope::Session,
                revision: 3,
                digest: "ab".repeat(32),
                source: PolicySource::SessionAllow,
                rules: vec!["registry.npmjs.org:443 (preset npm)".into()],
                change: Some(PolicyChange {
                    op: PolicyOp::Allow,
                    host: "api.example.com".into(),
                    ports: vec![443],
                    command_id: Some("cmd-1".into()),
                    proposal_id: None,
                }),
                actor: Some("human".into()),
            },
            NetworkEvent::Sidecar {
                state: SidecarState::ChannelLost,
                generation: 2,
                container: Some("axo-egr-s".into()),
                detail: None,
            },
            NetworkEvent::Bind {
                token: "0123456789abcdef".into(),
                binding: EgressBinding {
                    terminal_id: Some("t1".into()),
                    ..EgressBinding::new(BindingKind::Terminal)
                },
                scope: EgressScope::Provisioning,
            },
            NetworkEvent::Unbind {
                token: "0123456789abcdef".into(),
                reason: UnbindReason::TerminalClosed,
            },
            open_event(9, "a.example"),
            NetworkEvent::Close {
                conn: "g2:9".into(),
                ip: None,
                up: 0,
                down: 0,
                ms: 0,
                outcome: CloseOutcome::Interrupted,
                error: Some("channel lost".into()),
            },
            NetworkEvent::WebRequest {
                tool: WebTool::WebFetch,
                invocation_id: "inv".into(),
                activation_id: "act".into(),
                agent: "researcher".into(),
                url: Some("http://10.0.0.1/".into()),
                url_truncated: false,
                query_sha256: None,
                query_bytes: None,
            },
            NetworkEvent::Web {
                tool: WebTool::WebFetch,
                invocation_id: "inv".into(),
                activation_id: "act".into(),
                agent: "researcher".into(),
                decision: Decision::Deny,
                reason: Some("private_destination".into()),
                url: Some("http://10.0.0.1/".into()),
                url_truncated: true,
                final_url: None,
                final_url_truncated: false,
                status: None,
                redirects: vec![],
                redirects_truncated: false,
                query_sha256: None,
                query_bytes: None,
                results: None,
                unresponsive_engines: vec![],
                bytes: None,
                content_sha256: None,
                text_sha256: None,
                source_ids: vec!["S1a2b3c4d".into()],
                sources: vec![WebSource {
                    id: "S1a2b3c4d".into(),
                    url: "https://example.com/".into(),
                    url_truncated: false,
                }],
                retrieved_at_ms: 5,
                ms: 1,
            },
            limit_event(),
        ];
        let (_root, _ownership, store) = setup();
        let mut record = open(&store);
        for event in &events {
            record.append(1, event.clone()).unwrap();
        }
        let read: Vec<NetworkEvent> = record
            .read_after(None, 100)
            .unwrap()
            .into_iter()
            .map(|line| line.event)
            .collect();
        assert_eq!(read, events);
        let kinds: Vec<&str> = read.iter().map(NetworkEvent::kind).collect();
        assert_eq!(
            kinds,
            [
                "policy",
                "sidecar",
                "bind",
                "unbind",
                "open",
                "close",
                "web_request",
                "web",
                "limit"
            ]
        );
    }

    #[test]
    fn unrecorded_refusals_have_their_wire_name() {
        let line = serde_json::to_string(&NetworkEvent::Limit {
            what: LimitKind::UnrecordedRefusals,
            detail: "3 refused".into(),
        })
        .unwrap();
        assert!(line.contains("\"what\":\"unrecorded_refusals\""), "{line}");
    }

    pub(super) fn web_event(activation: &str) -> NetworkEvent {
        NetworkEvent::Web {
            tool: WebTool::WebSearch,
            invocation_id: "inv".into(),
            activation_id: activation.into(),
            agent: "researcher".into(),
            decision: Decision::Allow,
            reason: None,
            url: None,
            url_truncated: false,
            final_url: None,
            final_url_truncated: false,
            status: None,
            redirects: vec![],
            redirects_truncated: false,
            query_sha256: Some("ab".repeat(32)),
            query_bytes: Some(4),
            results: Some(1),
            unresponsive_engines: vec![],
            bytes: None,
            content_sha256: None,
            text_sha256: None,
            source_ids: vec!["S1a2b3c4d".into()],
            sources: vec![WebSource {
                id: "S1a2b3c4d".into(),
                url: "https://example.com/".into(),
                url_truncated: false,
            }],
            retrieved_at_ms: 5,
            ms: 1,
        }
    }

    fn activation(line: &NetworkLine) -> String {
        match &line.event {
            NetworkEvent::Web { activation_id, .. } => activation_id.clone(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn read_existing_matching_keeps_the_newest_of_one_kind_and_skips_a_torn_tail() {
        let (_root, _ownership, store) = setup();
        assert!(
            NetworkRecord::read_existing_matching(&store, "web", |_| true, 10)
                .unwrap()
                .is_none()
        );
        let mut record = open(&store);
        for id in 0..2_500 {
            let event = if id % 1_000 == 7 {
                web_event(&format!("act-{id}"))
            } else if id % 1_000 == 8 {
                // A web_request line is another kind, not a `web` match.
                NetworkEvent::WebRequest {
                    tool: WebTool::WebFetch,
                    invocation_id: "inv".into(),
                    activation_id: format!("act-{id}"),
                    agent: "researcher".into(),
                    url: Some("https://example.com/".into()),
                    url_truncated: false,
                    query_sha256: None,
                    query_bytes: None,
                }
            } else {
                open_event(id, "registry.npmjs.org")
            };
            record.append(id, event).unwrap();
        }
        record.sync().unwrap();
        // An interrupted append leaves a torn last line; a reader skips it.
        std::fs::OpenOptions::new()
            .append(true)
            .open(file_path(&store))
            .unwrap()
            .write_all(b"{\"v\":1,\"seq\":99999,\"ts_ms\":1,\"event\":{\"kind\":\"web\"")
            .unwrap();
        let read = |keep: &dyn Fn(&NetworkEvent) -> bool, max: usize| {
            NetworkRecord::read_existing_matching(&store, "web", keep, max)
                .unwrap()
                .unwrap()
        };
        let all: Vec<String> = read(&|_| true, 100).iter().map(activation).collect();
        assert_eq!(all, ["act-7", "act-1007", "act-2007"]);
        // At the bound the newest are kept, oldest first.
        let newest: Vec<String> = read(&|_| true, 2).iter().map(activation).collect();
        assert_eq!(newest, ["act-1007", "act-2007"]);
        // `keep` filters before the bound counts.
        let one: Vec<String> = read(
            &|event| matches!(event, NetworkEvent::Web { activation_id, .. } if activation_id == "act-7"),
            1,
        )
        .iter()
        .map(activation)
        .collect();
        assert_eq!(one, ["act-7"]);
        let requests = NetworkRecord::read_existing_matching(&store, "web_request", |_| true, 100)
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|line| line.event.kind() == "web_request"));
    }

    #[test]
    fn a_large_record_is_searched_for_its_web_lines_quickly() {
        // A record at Axocoatl 1.2.0's default event cap, nearly all egress
        // lines, with a web event every 500 lines: the projection's read
        // parses only those, in the single file 1.2.0 left and again once a
        // writer moved it into segments.
        const EVENTS: u64 = 50_000;
        let (_root, _ownership, store) = setup();
        let mut bytes = Vec::new();
        let mut web = 0;
        for seq in 1..=EVENTS {
            let event = if seq % 500 == 0 {
                web += 1;
                web_event(&format!("act-{seq}"))
            } else {
                open_event(seq, "registry.npmjs.org")
            };
            serde_json::to_writer(
                &mut bytes,
                &NetworkLine {
                    v: NETWORK_RECORD_VERSION,
                    seq,
                    ts_ms: seq,
                    event,
                },
            )
            .unwrap();
            bytes.push(b'\n');
        }
        single_file_record(&store, &bytes);
        let search = |layout: &str| {
            let started = std::time::Instant::now();
            let lines = NetworkRecord::read_existing_matching(&store, "web", |_| true, usize::MAX)
                .unwrap()
                .unwrap();
            let elapsed = started.elapsed();
            assert_eq!(lines.len(), web);
            assert_eq!(lines.last().unwrap().seq, EVENTS);
            assert!(lines.windows(2).all(|pair| pair[0].seq < pair[1].seq));
            eprintln!(
                "network record: {web} web lines found in {} MiB of {EVENTS} events ({layout}) in {elapsed:?}",
                bytes.len() >> 20
            );
            let bound = if cfg!(debug_assertions) { 2000 } else { 250 };
            assert!(elapsed.as_millis() < bound, "{elapsed:?}");
        };
        search("single file");
        let mut record = open(&store);
        assert!(record.sealed_segments() > 1, "{record:?}");
        assert_eq!(record.stats().events, EVENTS);
        // 1.2.0 refused ordinary events at its 50,000-event cap and the
        // reserved control events 64 past it; the record keeps going.
        for seq in EVENTS + 1..=EVENTS + 100 {
            assert_eq!(
                record.append(seq, open_event(seq, "a.example")).unwrap(),
                seq
            );
        }
        assert_eq!(record.stats().events, EVENTS + 100);
        drop(record);
        search("segments");
    }

    #[test]
    fn a_damaged_line_of_the_kind_read_is_reported_and_others_are_not_read() {
        let (_root, _ownership, store) = setup();
        let mut record = open(&store);
        record.append(1, web_event("act-1")).unwrap();
        record.sync().unwrap();
        drop(record);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(file_path(&store))
            .unwrap();
        // Damage in a line of another kind is not this reader's to find.
        file.write_all(b"{\"v\":1,\"seq\":2,\"ts_ms\":1,\"event\":{\"kind\":\"open\",broken}\n")
            .unwrap();
        let read = |kind: &str| NetworkRecord::read_existing_matching(&store, kind, |_| true, 10);
        assert_eq!(read("web").unwrap().unwrap().len(), 1);
        // Damage in a line of the kind read, before the end, is reported.
        file.write_all(b"{\"v\":1,\"seq\":3,\"ts_ms\":1,\"event\":{\"kind\":\"web\",broken}\n")
            .unwrap();
        file.write_all(b"{\"v\":1,\"seq\":4,\"ts_ms\":1,\"event\":{\"kind\":\"limit\",\"what\":\"record_full\",\"detail\":\"x\"}}\n")
            .unwrap();
        assert!(matches!(
            read("web"),
            Err(NetworkRecordError::Damaged { .. })
        ));
    }

    #[test]
    fn sync_data_cost_per_thousand_events() {
        // Reported for the A5 measurement; asserts only that it works.
        let (_root, _ownership, store) = setup();
        let mut record = open(&store);
        let started = std::time::Instant::now();
        for id in 0..1000 {
            record
                .append(id, open_event(id, "registry.npmjs.org"))
                .unwrap();
            record.sync().unwrap();
        }
        eprintln!(
            "network record: 1000 appends each followed by sync in {:?}",
            started.elapsed()
        );
    }
}
