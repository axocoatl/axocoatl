//! A Session's network record: every egress decision, connection close,
//! policy change and web-tool call, in order, one JSON line each.
//!
//! The record is append-only. [`NetworkRecord::append`] returns only after a
//! complete `write(2)` of the whole line to the file, so a caller that waits
//! for it before acting has a write-ahead record of that action in the file.
//! Durability across an operating-system crash comes from [`NetworkRecord::sync`],
//! which the owner calls every 200 ms while the record is dirty and on close.
//!
//! On open, a torn or unparseable last line (an interrupted append) is cut off
//! and a `sidecar{state: "recovered"}` event says how many bytes were removed.
//! Any other damage refuses the open. Sequence numbers continue from the last
//! good line; a gap is counted, never renumbered.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};

/// The record's file inside its component directory.
pub const NETWORK_RECORD_FILE: &str = "network-record.v1.jsonl";
/// Line format version.
pub const NETWORK_RECORD_VERSION: u32 = 1;
/// Longest line, newline included.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// Default event cap (`sandbox.egress.record_max_events`).
pub const DEFAULT_MAX_EVENTS: u64 = 50_000;
/// Default byte cap.
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Events past the cap reserved for `limit`, `sidecar`, `policy` and `unbind`,
/// so the record always explains why it stopped.
pub const CONTROL_HEADROOM_EVENTS: u64 = 64;
/// Bytes reserved for those events.
pub const CONTROL_HEADROOM_BYTES: u64 = CONTROL_HEADROOM_EVENTS * MAX_LINE_BYTES as u64;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebTool {
    WebSearch,
    WebFetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    RecordFull,
    MaxConnections,
    RestartBudget,
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
        }
    }
}

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
            Self::WebRequest { .. } => "web_request",
            Self::Web { .. } => "web",
            Self::Limit { .. } => "limit",
        }
    }

    /// Events that may use the reserved headroom past the cap.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Self::Policy { .. } | Self::Sidecar { .. } | Self::Unbind { .. } | Self::Limit { .. }
        )
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
            Self::Open { conn, .. } | Self::Close { conn, .. }
                if conn.is_empty() || conn.len() > 32 =>
            {
                invalid("conn must be 1-32 bytes")
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

/// Caps for one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLimits {
    pub max_events: u64,
    pub max_bytes: u64,
}

impl Default for RecordLimits {
    fn default() -> Self {
        Self {
            max_events: DEFAULT_MAX_EVENTS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl RecordLimits {
    fn read_ceiling(&self) -> usize {
        (self.max_bytes + CONTROL_HEADROOM_BYTES + MAX_LINE_BYTES as u64) as usize
    }
}

/// Counts for the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStats {
    pub events: u64,
    pub bytes: u64,
    pub last_seq: u64,
    /// Ordinary events are refused; only control events still fit.
    pub full: bool,
    /// Places where `seq` skipped a value.
    pub gaps: u64,
    pub max_events: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkRecordError {
    #[error("network record I/O: {0}")]
    Io(#[from] io::Error),
    #[error("network record is full")]
    Full,
    #[error("network record line is longer than 16 KiB")]
    LineTooLong,
    #[error("only policy, sidecar, unbind and limit events may use the reserved headroom")]
    NotControl,
    #[error("invalid network event: {0}")]
    InvalidEvent(&'static str),
    #[error("network record is damaged at line {line}: {reason}")]
    Damaged { line: u64, reason: String },
    #[error("network record write is uncertain; reopen it")]
    Poisoned,
    #[error("network record limits must be positive")]
    InvalidLimits,
}

/// Single-writer handle to one Session's record.
pub struct NetworkRecord {
    namespace: OwnedExecutionNamespace,
    file: File,
    limits: RecordLimits,
    /// `(seq, byte offset)` of every line, in order.
    index: Vec<(u64, u64)>,
    bytes: u64,
    last_seq: u64,
    gaps: u64,
    dirty: bool,
    poisoned: bool,
}

impl std::fmt::Debug for NetworkRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NetworkRecord")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

struct Loaded {
    index: Vec<(u64, u64)>,
    good_bytes: u64,
    torn_bytes: u64,
    last_seq: u64,
    gaps: u64,
}

fn load(bytes: &[u8]) -> Result<Loaded, NetworkRecordError> {
    let mut loaded = Loaded {
        index: Vec::new(),
        good_bytes: 0,
        torn_bytes: 0,
        last_seq: 0,
        gaps: 0,
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
        let parsed = (length < MAX_LINE_BYTES)
            .then(|| serde_json::from_slice::<NetworkLine>(&bytes[position..end]).ok())
            .flatten();
        let Some(line) = parsed else {
            if is_last {
                loaded.torn_bytes = (bytes.len() - position) as u64;
                break;
            }
            return Err(NetworkRecordError::Damaged {
                line: line_number,
                reason: "unparseable line before the end of the record".into(),
            });
        };
        if line.v != NETWORK_RECORD_VERSION || line.seq <= loaded.last_seq {
            return Err(NetworkRecordError::Damaged {
                line: line_number,
                reason: "unknown version or non-increasing sequence".into(),
            });
        }
        if loaded.last_seq != 0 && line.seq != loaded.last_seq + 1 {
            loaded.gaps += 1;
        }
        loaded.index.push((line.seq, position as u64));
        loaded.last_seq = line.seq;
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
        let parsed = (end - start < MAX_LINE_BYTES)
            .then(|| serde_json::from_slice::<NetworkLine>(&body[start..end]).ok())
            .flatten()
            .filter(|line| line.v == NETWORK_RECORD_VERSION);
        let Some(line) = parsed else {
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

impl NetworkRecord {
    /// Open or create the record in its component namespace, recovering a
    /// torn tail.
    pub fn open(
        namespace: OwnedExecutionNamespace,
        limits: RecordLimits,
    ) -> Result<Self, NetworkRecordError> {
        if limits.max_events == 0 || limits.max_bytes == 0 {
            return Err(NetworkRecordError::InvalidLimits);
        }
        namespace.require_root(&ExecutionComponent::NetworkRecord)?;
        let primary = Path::new(NETWORK_RECORD_FILE);
        let bytes = match namespace.read_limited(primary, limits.read_ceiling()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // An initialized component whose file is gone must never look
                // like a new, empty record.
                namespace.check_journal_creation(primary)?;
                Vec::new()
            }
            Err(error) => return Err(error.into()),
        };
        let loaded = load(&bytes)?;
        let file = namespace.open_append(primary)?;
        if loaded.torn_bytes > 0 {
            file.set_len(loaded.good_bytes)?;
        }
        file.sync_all()?;
        namespace.mark_journal_initialized(primary)?;
        let mut record = Self {
            namespace,
            file,
            limits,
            index: loaded.index,
            bytes: loaded.good_bytes,
            last_seq: loaded.last_seq,
            gaps: loaded.gaps,
            dirty: false,
            poisoned: false,
        };
        if loaded.torn_bytes > 0 {
            record.append_control(
                now_ms(),
                NetworkEvent::Sidecar {
                    state: SidecarState::Recovered,
                    generation: 0,
                    container: None,
                    detail: Some(format!("torn {} bytes", loaded.torn_bytes)),
                },
            )?;
            record.sync()?;
        }
        Ok(record)
    }

    /// Append an ordinary event. `Err(Full)` past the cap.
    pub fn append(&mut self, ts_ms: u64, event: NetworkEvent) -> Result<u64, NetworkRecordError> {
        self.append_within(ts_ms, event, 0, 0)
    }

    /// Append a `policy`, `sidecar`, `unbind` or `limit` event, using the
    /// reserved headroom once the ordinary cap is reached.
    pub fn append_control(
        &mut self,
        ts_ms: u64,
        event: NetworkEvent,
    ) -> Result<u64, NetworkRecordError> {
        if !event.is_control() {
            return Err(NetworkRecordError::NotControl);
        }
        self.append_within(
            ts_ms,
            event,
            CONTROL_HEADROOM_EVENTS,
            CONTROL_HEADROOM_BYTES,
        )
    }

    fn append_within(
        &mut self,
        ts_ms: u64,
        event: NetworkEvent,
        extra_events: u64,
        extra_bytes: u64,
    ) -> Result<u64, NetworkRecordError> {
        if self.poisoned {
            return Err(NetworkRecordError::Poisoned);
        }
        event.validate()?;
        let seq = self.last_seq + 1;
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
        let events = self.index.len() as u64;
        if events >= self.limits.max_events + extra_events
            || self.bytes + line.len() as u64 > self.limits.max_bytes + extra_bytes
        {
            return Err(NetworkRecordError::Full);
        }
        if let Err(error) = self.file.write_all(&line) {
            // A partial line may now end the file; only a reopen may decide.
            self.poisoned = true;
            return Err(error.into());
        }
        self.index.push((seq, self.bytes));
        self.bytes += line.len() as u64;
        self.last_seq = seq;
        self.dirty = true;
        Ok(seq)
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
        let start = match after {
            Some(after) => self.index.partition_point(|(seq, _)| *seq <= after),
            None => 0,
        };
        if limit == 0 || start >= self.index.len() {
            return Ok(Vec::new());
        }
        let end = (start + limit).min(self.index.len());
        let from = self.index[start].1;
        let to = self
            .index
            .get(end)
            .map_or(self.bytes, |(_, offset)| *offset);
        let mut file = self
            .namespace
            .open_read(NETWORK_RECORD_FILE, self.limits.read_ceiling())?;
        file.seek(SeekFrom::Start(from))?;
        let mut bytes = vec![0; (to - from) as usize];
        file.read_exact(&mut bytes)?;
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice(line).map_err(|error| NetworkRecordError::Damaged {
                    line: 0,
                    reason: error.to_string(),
                })
            })
            .collect()
    }

    /// Read a record without opening a writer: no lock, no recovery, no
    /// creation. A torn tail is skipped, not cut. `Ok(None)` when the Session
    /// has no record.
    pub fn read_existing(
        canonical: &crate::execution_store::SessionExecutionStore,
        after: Option<u64>,
        limit: usize,
        limits: RecordLimits,
    ) -> Result<Option<(Vec<NetworkLine>, RecordStats)>, NetworkRecordError> {
        let bytes = match canonical.read_existing_component(
            &ExecutionComponent::NetworkRecord,
            Path::new(NETWORK_RECORD_FILE),
            limits.read_ceiling(),
        ) {
            Ok(bytes) => bytes,
            Err(crate::execution_store::ExecutionStoreError::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(io::Error::other(error.to_string()).into()),
        };
        let loaded = load(&bytes)?;
        let events = loaded.index.len() as u64;
        let stats = RecordStats {
            events,
            bytes: loaded.good_bytes,
            last_seq: loaded.last_seq,
            full: events >= limits.max_events || loaded.good_bytes >= limits.max_bytes,
            gaps: loaded.gaps,
            max_events: limits.max_events,
        };
        let limit = limit.min(MAX_READ_LIMIT);
        let start = match after {
            Some(after) => loaded.index.partition_point(|(seq, _)| *seq <= after),
            None => 0,
        };
        let lines = loaded.index[start.min(loaded.index.len())..]
            .iter()
            .take(limit)
            .map(|(_, offset)| {
                let from = *offset as usize;
                let to = from
                    + bytes[from..]
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .unwrap_or(bytes.len() - from);
                serde_json::from_slice(&bytes[from..to]).map_err(|error| {
                    NetworkRecordError::Damaged {
                        line: 0,
                        reason: error.to_string(),
                    }
                })
            })
            .collect::<Result<Vec<NetworkLine>, _>>()?;
        Ok(Some((lines, stats)))
    }

    /// The newest stored lines of event kind `kind` that `keep` accepts, at
    /// most `max`, oldest first, read without opening a writer. Like
    /// [`Self::read_existing`] it takes no lock, recovers nothing, creates
    /// nothing and skips a torn tail. `Ok(None)` when the Session has no
    /// record.
    ///
    /// Only lines that may be of that kind are parsed: the file is searched
    /// once for `"kind":"<kind>"`, which the writer's compact JSON puts at the
    /// start of every event of that kind (an escaped string value cannot
    /// contain it), and each match is parsed and its kind checked. Other lines
    /// are not validated, so a damaged line of another kind is not reported
    /// here; opening the writer still refuses one.
    pub fn read_existing_matching(
        canonical: &crate::execution_store::SessionExecutionStore,
        limits: RecordLimits,
        kind: &str,
        keep: impl Fn(&NetworkEvent) -> bool,
        max: usize,
    ) -> Result<Option<Vec<NetworkLine>>, NetworkRecordError> {
        let bytes = match canonical.read_existing_component(
            &ExecutionComponent::NetworkRecord,
            Path::new(NETWORK_RECORD_FILE),
            limits.read_ceiling(),
        ) {
            Ok(bytes) => bytes,
            Err(crate::execution_store::ExecutionStoreError::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(io::Error::other(error.to_string()).into()),
        };
        Ok(Some(matching_lines(&bytes, kind, keep, max)?))
    }

    pub fn stats(&self) -> RecordStats {
        let events = self.index.len() as u64;
        RecordStats {
            events,
            bytes: self.bytes,
            last_seq: self.last_seq,
            full: events >= self.limits.max_events || self.bytes >= self.limits.max_bytes,
            gaps: self.gaps,
            max_events: self.limits.max_events,
        }
    }

    pub fn limits(&self) -> RecordLimits {
        self.limits
    }
}

impl Drop for NetworkRecord {
    fn drop(&mut self) {
        if self.dirty && !self.poisoned {
            let _ = self.file.sync_data();
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
    use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use crate::turn_contract::SessionId;
    use std::sync::Arc;

    fn setup() -> (
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

    fn open(store: &SessionExecutionStore, limits: RecordLimits) -> NetworkRecord {
        NetworkRecord::open(
            store
                .component_namespace(ExecutionComponent::NetworkRecord)
                .unwrap(),
            limits,
        )
        .unwrap()
    }

    fn file_path(store: &SessionExecutionStore) -> std::path::PathBuf {
        store
            .path()
            .parent()
            .unwrap()
            .join("network-record")
            .join(NETWORK_RECORD_FILE)
    }

    fn open_event(id: u64, host: &str) -> NetworkEvent {
        NetworkEvent::Open {
            conn: format!("g1:{id}"),
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

    fn limit_event() -> NetworkEvent {
        NetworkEvent::Limit {
            what: LimitKind::RecordFull,
            detail: "cap reached".into(),
        }
    }

    #[test]
    fn append_and_read_back_in_order() {
        let (_root, _ownership, store) = setup();
        let mut record = open(&store, RecordLimits::default());
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

        // The wire form is the documented contract.
        let text = std::fs::read_to_string(file_path(&store)).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["v"], 1);
        assert_eq!(first["event"]["kind"], "open");
        assert_eq!(first["event"]["decision"], "allow");
        assert_eq!(first["event"]["conn_kind"], "connect");
        assert_eq!(first["event"]["binding"]["kind"], "agent");
        assert!(first["event"]["binding"].get("terminal_id").is_none());
        assert!(first["event"].get("reason").is_none());
        let stats = record.stats();
        assert_eq!(
            (stats.events, stats.last_seq, stats.gaps, stats.full),
            (2, 2, 0, false)
        );
        assert_eq!(stats.bytes, text.len() as u64);
    }

    #[test]
    fn read_existing_needs_no_writer_and_skips_a_torn_tail() {
        let (_root, _ownership, store) = setup();
        assert!(
            NetworkRecord::read_existing(&store, None, 10, RecordLimits::default())
                .unwrap()
                .is_none()
        );
        {
            let mut record = open(&store, RecordLimits::default());
            for id in 0..4 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
            // Works while the writer holds its lock.
            let (lines, stats) =
                NetworkRecord::read_existing(&store, Some(1), 2, RecordLimits::default())
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
        let (lines, stats) =
            NetworkRecord::read_existing(&store, None, 100, RecordLimits::default())
                .unwrap()
                .unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(stats.bytes, good as u64);
        // The read changed nothing.
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn sequence_continues_across_reopen() {
        let (_root, _ownership, store) = setup();
        {
            let mut record = open(&store, RecordLimits::default());
            for id in 0..5 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
        }
        let mut record = open(&store, RecordLimits::default());
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
        let _record = open(&store, RecordLimits::default());
        assert!(store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .is_err());
    }

    #[test]
    fn torn_tail_is_cut_and_recorded() {
        for tail in [&b"{\"v\":1,\"seq\":4,\"ts"[..], b"not json\n", b"\n"] {
            let (_root, _ownership, store) = setup();
            {
                let mut record = open(&store, RecordLimits::default());
                for id in 0..3 {
                    record.append(id, open_event(id, "a.example")).unwrap();
                }
            }
            let path = file_path(&store);
            let good = std::fs::read(&path).unwrap();
            let mut torn = good.clone();
            torn.extend_from_slice(tail);
            std::fs::write(&path, &torn).unwrap();
            let record = open(&store, RecordLimits::default());
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
            let mut record = open(&store, RecordLimits::default());
            for id in 0..3 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
        }
        let path = file_path(&store);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "garbage";
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let error = NetworkRecord::open(
            store
                .component_namespace(ExecutionComponent::NetworkRecord)
                .unwrap(),
            RecordLimits::default(),
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
            let mut record = open(&store, RecordLimits::default());
            record.append(1, limit_event()).unwrap();
        }
        std::fs::remove_file(file_path(&store)).unwrap();
        assert!(NetworkRecord::open(
            store
                .component_namespace(ExecutionComponent::NetworkRecord)
                .unwrap(),
            RecordLimits::default(),
        )
        .is_err());
    }

    #[test]
    fn sequence_gaps_are_counted_not_renumbered() {
        let (_root, _ownership, store) = setup();
        {
            let mut record = open(&store, RecordLimits::default());
            for id in 0..3 {
                record.append(id, open_event(id, "a.example")).unwrap();
            }
        }
        let path = file_path(&store);
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(index, _)| *index != 1)
            .map(|(_, line)| line)
            .collect();
        std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();
        let mut record = open(&store, RecordLimits::default());
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
    fn cap_refuses_ordinary_events_but_keeps_control_headroom() {
        let (_root, _ownership, store) = setup();
        let mut record = open(
            &store,
            RecordLimits {
                max_events: 3,
                max_bytes: DEFAULT_MAX_BYTES,
            },
        );
        for id in 0..3 {
            record.append(id, open_event(id, "a.example")).unwrap();
        }
        assert!(record.stats().full);
        assert!(matches!(
            record.append(3, open_event(3, "a.example")),
            Err(NetworkRecordError::Full)
        ));
        // A control event is still refused through the ordinary path...
        assert!(matches!(
            record.append(3, limit_event()),
            Err(NetworkRecordError::Full)
        ));
        // ...fits through the control path, which refuses ordinary kinds.
        assert_eq!(record.append_control(3, limit_event()).unwrap(), 4);
        assert!(matches!(
            record.append_control(3, open_event(4, "a.example")),
            Err(NetworkRecordError::NotControl)
        ));
        for _ in 1..CONTROL_HEADROOM_EVENTS {
            record
                .append_control(
                    4,
                    NetworkEvent::Unbind {
                        token: "0123456789abcdef".into(),
                        reason: UnbindReason::Settled,
                    },
                )
                .unwrap();
        }
        assert!(matches!(
            record.append_control(5, limit_event()),
            Err(NetworkRecordError::Full)
        ));
        assert_eq!(record.stats().events, 3 + CONTROL_HEADROOM_EVENTS);
    }

    #[test]
    fn byte_cap_applies_too() {
        let (_root, _ownership, store) = setup();
        let mut record = open(
            &store,
            RecordLimits {
                max_events: 1000,
                max_bytes: 600,
            },
        );
        let mut accepted = 0;
        while record.append(1, open_event(accepted, "a.example")).is_ok() {
            accepted += 1;
        }
        assert!(accepted >= 1 && record.stats().bytes <= 600, "{accepted}");
        assert!(record.append_control(2, limit_event()).is_ok());
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
        let mut record = open(&store, RecordLimits::default());
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
            record.append_control(1, huge),
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
        let mut record = open(&store, RecordLimits::default());
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
    fn ten_thousand_appends_finish_quickly() {
        let (_root, _ownership, store) = setup();
        let mut record = open(&store, RecordLimits::default());
        let started = std::time::Instant::now();
        for id in 0..10_000 {
            record
                .append(id, open_event(id, "registry.npmjs.org"))
                .unwrap();
        }
        record.sync().unwrap();
        let elapsed = started.elapsed();
        eprintln!("network record: 10000 appends + 1 sync in {elapsed:?}");
        // The 2 s bound is for an optimized build (measured about 0.43 s on an
        // M-series Mac); unoptimized serialization alone takes about 2 s.
        let bound = if cfg!(debug_assertions) { 8 } else { 2 };
        assert!(
            elapsed < std::time::Duration::from_secs(bound),
            "{elapsed:?}"
        );
        let tail = record.read_after(Some(9_000), 1000).unwrap();
        assert_eq!(tail.len(), 1000);
        assert_eq!(tail.last().unwrap().seq, 10_000);
    }

    fn web_event(activation: &str) -> NetworkEvent {
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
        assert!(NetworkRecord::read_existing_matching(
            &store,
            RecordLimits::default(),
            "web",
            |_| true,
            10
        )
        .unwrap()
        .is_none());
        let mut record = open(&store, RecordLimits::default());
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
            NetworkRecord::read_existing_matching(&store, RecordLimits::default(), "web", keep, max)
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
        let requests = NetworkRecord::read_existing_matching(
            &store,
            RecordLimits::default(),
            "web_request",
            |_| true,
            100,
        )
        .unwrap()
        .unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|line| line.event.kind() == "web_request"));
    }

    #[test]
    fn a_full_record_is_searched_for_its_web_lines_quickly() {
        // A record at its default event cap, nearly all egress lines, with a
        // web event every 500 lines: the projection's read parses only those.
        let (_root, _ownership, store) = setup();
        drop(open(&store, RecordLimits::default()));
        let mut bytes = Vec::new();
        let mut web = 0;
        for seq in 1..=DEFAULT_MAX_EVENTS {
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
        std::fs::write(file_path(&store), &bytes).unwrap();
        let started = std::time::Instant::now();
        let lines = NetworkRecord::read_existing_matching(
            &store,
            RecordLimits::default(),
            "web",
            |_| true,
            usize::MAX,
        )
        .unwrap()
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(lines.len(), web);
        assert_eq!(lines.last().unwrap().seq, DEFAULT_MAX_EVENTS);
        eprintln!(
            "network record: {web} web lines found in {} MiB of {DEFAULT_MAX_EVENTS} events in {elapsed:?}",
            bytes.len() >> 20
        );
        let bound = if cfg!(debug_assertions) { 2000 } else { 250 };
        assert!(elapsed.as_millis() < bound, "{elapsed:?}");
    }

    #[test]
    fn a_damaged_line_of_the_kind_read_is_reported_and_others_are_not_read() {
        let (_root, _ownership, store) = setup();
        let mut record = open(&store, RecordLimits::default());
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
        let read = |kind: &str| {
            NetworkRecord::read_existing_matching(
                &store,
                RecordLimits::default(),
                kind,
                |_| true,
                10,
            )
        };
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
        let mut record = open(&store, RecordLimits::default());
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
