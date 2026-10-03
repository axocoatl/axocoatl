//! The egress decision interface between Session sandboxes and the daemon.
//!
//! A sandbox that runs under `network: egress` holds an [`EgressAttachment`].
//! Its sidecar proxy reports every request over the control channel
//! ([`crate::egress_control`]); the [`EgressAuthority`] decides, records and
//! mints the credentials that processes present to the proxy.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use axocoatl_exec::egress::protocol::{DaemonFrame, MAX_DATA_BYTES, RELAY_WINDOW_BYTES};
use base64::Engine;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub use axocoatl_exec::egress::protocol::{CloseOutcome, PeerIdentity, RequestKind};

/// One request the sidecar is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// Sidecar generation; ids restart with each sidecar process.
    pub generation: u32,
    pub id: u64,
    pub kind: RequestKind,
    /// As the client wrote it: a name, an IPv4 literal or `[v6]`.
    pub host: String,
    pub port: u16,
    /// Hex SHA-256 of the presented credential.
    pub auth: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    /// The program behind the connection, when the request came through the
    /// sidecar's identity socket.
    pub peer: Option<PeerIdentity>,
}

/// The authority's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Connect to these addresses, in order.
    Allow { addrs: Vec<IpAddr> },
    /// Answer the client with this status, reason code and hint.
    Deny {
        status: u16,
        reason: String,
        hint: String,
    },
    /// Answer a `connect` request with `200 Connection Established` and carry
    /// its bytes to [`EgressAuthority::relay`] over the control channel. The
    /// sidecar connects nowhere.
    Relay,
}

impl Decision {
    pub fn deny(status: u16, reason: &str, hint: impl Into<String>) -> Self {
        Self::Deny {
            status,
            reason: reason.to_string(),
            hint: hint.into(),
        }
    }
}

/// A connection the authority answered with [`Decision::Relay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOpen {
    pub generation: u32,
    pub id: u64,
    pub open: OpenRequest,
}

/// Bytes the daemon reads before it credits them back to the sidecar.
pub(crate) const RELAY_CREDIT_BATCH: u32 = RELAY_WINDOW_BYTES / 4;

/// Why a relayed connection ended before its client finished sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelayEnd {
    /// The sidecar closed it (the client reset or left, or it was revoked).
    Closed,
    /// The control channel ended.
    ChannelLost,
}

#[derive(Debug)]
pub(crate) struct RelayState {
    inbound: VecDeque<u8>,
    /// Bytes read from `inbound` and not yet credited back.
    consumed: u32,
    /// The client finished sending.
    eof: bool,
    ended: Option<RelayEnd>,
    read_waker: Option<Waker>,
    /// Bytes the sidecar still accepts from the daemon.
    send_credit: u32,
    write_waker: Option<Waker>,
    /// The daemon finished sending (`eof` sent).
    write_closed: bool,
    /// The stream was dropped: further client bytes are discarded.
    dropped: bool,
}

/// One relayed connection's state, shared by its [`RelayStream`] and the
/// control loop.
#[derive(Debug)]
pub(crate) struct RelayShared {
    state: Mutex<RelayState>,
}

impl RelayShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RelayState {
                inbound: VecDeque::new(),
                consumed: 0,
                eof: false,
                ended: None,
                read_waker: None,
                send_credit: RELAY_WINDOW_BYTES,
                write_waker: None,
                write_closed: false,
                dropped: false,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, RelayState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn wake(state: &mut RelayState) {
        if let Some(waker) = state.read_waker.take() {
            waker.wake();
        }
        if let Some(waker) = state.write_waker.take() {
            waker.wake();
        }
    }

    /// Client bytes from the sidecar. Returns the credit to grant at once
    /// when the stream was dropped (its bytes are discarded), or an error
    /// when the sidecar sent past its window.
    pub(crate) fn received(&self, bytes: Vec<u8>) -> Result<Option<u32>, String> {
        let mut state = self.lock();
        if state.eof {
            return Err("the egress proxy sent relay data after its end".into());
        }
        let outstanding = state.inbound.len() as u64 + u64::from(state.consumed);
        if outstanding + bytes.len() as u64 > u64::from(RELAY_WINDOW_BYTES) {
            return Err("the egress proxy sent more than its relay window".into());
        }
        if state.dropped {
            return Ok(Some(bytes.len() as u32));
        }
        state.inbound.extend(bytes);
        Self::wake(&mut state);
        Ok(None)
    }

    /// The client finished sending.
    pub(crate) fn received_eof(&self) {
        let mut state = self.lock();
        state.eof = true;
        Self::wake(&mut state);
    }

    /// The sidecar accepts `bytes` more.
    pub(crate) fn credited(&self, bytes: u32) -> Result<(), String> {
        let mut state = self.lock();
        let credit = u64::from(state.send_credit) + u64::from(bytes);
        if credit > u64::from(RELAY_WINDOW_BYTES) {
            return Err("the egress proxy granted more than the relay window".into());
        }
        state.send_credit = credit as u32;
        Self::wake(&mut state);
        Ok(())
    }

    /// The connection is over: reads end (with an error unless the client
    /// had finished) and writes fail.
    pub(crate) fn end(&self, why: RelayEnd) {
        let mut state = self.lock();
        state.ended.get_or_insert(why);
        Self::wake(&mut state);
    }
}

/// The client's bytes of a relayed connection, as a tokio stream. Reading
/// takes the client's bytes and credits them back to the sidecar; writing
/// sends bytes to the client within the sidecar's credit, so a slow client
/// makes `poll_write` wait. `poll_shutdown` (or dropping the stream) ends the
/// client's receiving side; bytes the client sends after a drop are
/// discarded. When the sidecar closes the connection (the client reset or
/// left, or it was revoked) or the control channel ends, reads that are not
/// already at the client's end fail with `ConnectionReset` and writes with
/// `BrokenPipe`.
#[derive(Debug)]
pub struct RelayStream {
    id: u64,
    shared: Arc<RelayShared>,
    frames: crate::egress_control::Outgoing,
}

impl RelayStream {
    pub(crate) fn new(
        id: u64,
        shared: Arc<RelayShared>,
        frames: crate::egress_control::Outgoing,
    ) -> Self {
        Self { id, shared, frames }
    }

    /// The connection id, `(generation, id)` with [`RelayOpen`].
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl AsyncRead for RelayStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.shared.lock();
        if !state.inbound.is_empty() {
            let count = buffer.remaining().min(state.inbound.len());
            let (front, back) = state.inbound.as_slices();
            let first = count.min(front.len());
            buffer.put_slice(&front[..first]);
            buffer.put_slice(&back[..count - first]);
            state.inbound.drain(..count);
            state.consumed += count as u32;
            let grant = (state.consumed >= RELAY_CREDIT_BATCH).then(|| {
                let grant = state.consumed;
                state.consumed = 0;
                grant
            });
            drop(state);
            if let Some(bytes) = grant {
                self.frames.send(DaemonFrame::Credit { id: self.id, bytes });
            }
            return Poll::Ready(Ok(()));
        }
        if state.eof || buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(end) = state.ended {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                match end {
                    RelayEnd::Closed => "the relayed connection was closed",
                    RelayEnd::ChannelLost => "the egress control channel ended",
                },
            )));
        }
        state.read_waker = Some(context.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for RelayStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut state = self.shared.lock();
        if state.ended.is_some() || state.write_closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if state.send_credit == 0 {
            state.write_waker = Some(context.waker().clone());
            return Poll::Pending;
        }
        let count = bytes
            .len()
            .min(state.send_credit as usize)
            .min(MAX_DATA_BYTES);
        state.send_credit -= count as u32;
        drop(state);
        let frame = DaemonFrame::Data {
            id: self.id,
            b: base64::engine::general_purpose::STANDARD.encode(&bytes[..count]),
        };
        if self.frames.send(frame) {
            Poll::Ready(Ok(count))
        } else {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.shared.lock();
        if !state.write_closed {
            state.write_closed = true;
            let ended = state.ended.is_some();
            drop(state);
            if !ended {
                self.frames.send(DaemonFrame::Eof { id: self.id });
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for RelayStream {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.dropped = true;
        // Credit what was read but not yet credited, and what is still
        // buffered, so a client that keeps sending is not stuck.
        let unread = state.consumed + state.inbound.len() as u32;
        state.consumed = 0;
        state.inbound.clear();
        let send_eof = !state.write_closed && state.ended.is_none();
        state.write_closed = true;
        let live = state.ended.is_none() && !state.eof;
        drop(state);
        if send_eof {
            self.frames.send(DaemonFrame::Eof { id: self.id });
        }
        if live && unread > 0 {
            self.frames.send(DaemonFrame::Credit {
                id: self.id,
                bytes: unread,
            });
        }
    }
}

/// How an allowed connection ended, as the sidecar (or the control loop, on
/// a lost channel) reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    pub generation: u32,
    pub id: u64,
    pub ip: Option<IpAddr>,
    pub up: u64,
    pub down: u64,
    pub ms: u64,
    pub outcome: CloseOutcome,
    pub error: Option<String>,
}

/// Sidecar lifecycle changes worth recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEvent {
    Starting {
        generation: u32,
        container: Option<String>,
    },
    Ready {
        generation: u32,
        container: Option<String>,
    },
    ChannelLost {
        generation: u32,
        detail: String,
    },
    Restarting {
        generation: u32,
    },
    Failed {
        generation: u32,
        detail: String,
    },
    /// The restart budget is spent; the sidecar stays down.
    BudgetSpent {
        generation: u32,
        detail: String,
    },
    Stopped {
        generation: u32,
    },
}

/// What a credential is for. Agent, setup and terminal credentials use the
/// Session policy; provisioning uses the distribution presets; the browser
/// uses the browser policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrantKind {
    Agent,
    Setup,
    Provisioning,
    Terminal,
    Browser,
}

/// Liveness check for a long-lived credential such as a terminal's. A
/// credential whose check fails is unbound at its next use.
pub type Liveness = Arc<dyn Fn() -> bool + Send + Sync>;

/// Who a credential is minted for. Fields that do not apply stay `None`.
#[derive(Clone)]
pub struct GrantSpec {
    pub kind: GrantKind,
    pub invocation_id: Option<String>,
    pub activation_id: Option<String>,
    pub node_id: Option<String>,
    pub agent: Option<String>,
    pub process: Option<String>,
    pub terminal_id: Option<String>,
    pub setup_index: Option<u32>,
    pub liveness: Option<Liveness>,
}

impl GrantSpec {
    pub fn new(kind: GrantKind) -> Self {
        Self {
            kind,
            invocation_id: None,
            activation_id: None,
            node_id: None,
            agent: None,
            process: None,
            terminal_id: None,
            setup_index: None,
            liveness: None,
        }
    }
}

impl fmt::Debug for GrantSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrantSpec")
            .field("kind", &self.kind)
            .field("invocation_id", &self.invocation_id)
            .field("activation_id", &self.activation_id)
            .field("agent", &self.agent)
            .field("terminal_id", &self.terminal_id)
            .field("setup_index", &self.setup_index)
            .finish_non_exhaustive()
    }
}

/// A proxy URL that carries a credential. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySecret(String);

impl ProxySecret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProxySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProxySecret(<redacted>)")
    }
}

/// A live credential. Dropping it unbinds the credential, closes the
/// connections opened with it and deletes its env file.
pub struct EgressGrant {
    /// 0600 env file for `podman exec --env-file`; `None` for the browser.
    pub env_file: Option<PathBuf>,
    /// First 16 hex of the credential's SHA-256.
    pub token_tag: String,
    /// The browser's proxy URL, passed only on the driver's stdin.
    pub proxy_url_for_stdin: Option<ProxySecret>,
    /// Unbinds the credential when dropped.
    #[allow(dead_code)]
    guard: Box<dyn Send + Sync>,
}

impl EgressGrant {
    pub fn new(
        env_file: Option<PathBuf>,
        token_tag: String,
        proxy_url_for_stdin: Option<ProxySecret>,
        guard: Box<dyn Send + Sync>,
    ) -> Self {
        Self {
            env_file,
            token_tag,
            proxy_url_for_stdin,
            guard,
        }
    }
}

impl fmt::Debug for EgressGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressGrant")
            .field("env_file", &self.env_file)
            .field("token_tag", &self.token_tag)
            .field("proxy_url_for_stdin", &self.proxy_url_for_stdin)
            .finish_non_exhaustive()
    }
}

/// Environment for one supervised process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv<'a> {
    pub env_file: Option<&'a Path>,
}

/// The daemon's policy decision point for one Session.
#[async_trait::async_trait]
pub trait EgressAuthority: Send + Sync + fmt::Debug {
    /// Mint a credential, record its binding and write its env file. The
    /// binding is recorded before the credential is returned.
    async fn grant(&self, spec: GrantSpec) -> Result<EgressGrant, String>;
    /// Decide one request. Must record the decision before returning.
    async fn decide(&self, open: OpenRequest) -> Decision;
    /// Record a finished connection.
    async fn closed(&self, report: CloseReport);
    /// Record a sidecar lifecycle change.
    async fn sidecar_event(&self, event: SidecarEvent);
    /// Serve a connection this authority answered with [`Decision::Relay`].
    /// The default ends it at once: the client reads end of input.
    async fn relay(&self, open: RelayOpen, stream: RelayStream) {
        drop((open, stream));
    }
    /// Use this control channel to revoke connections. Each sidecar
    /// generation attaches its own.
    fn attach_control(&self, _handle: crate::egress_control::ControlHandle) {}
    /// The generation a newly started sidecar takes. Connection ids restart
    /// with every sidecar process, so an authority whose record outlives one
    /// sidecar returns one more than the last generation it recorded, which
    /// keeps `(generation, id)` unique in that record.
    fn first_generation(&self) -> u32 {
        1
    }
    /// Never allow these addresses, whatever the policy lists: the gateways
    /// of the sidecar's network, which lead to the host running Podman.
    fn forbid_destinations(&self, _addrs: &[IpAddr]) {}
}

/// What a sandbox needs to run under `network: egress`.
#[derive(Clone, Debug)]
pub struct EgressAttachment {
    pub authority: Arc<dyn EgressAuthority>,
    /// Podman network for the sidecar; `None` is Podman's default network.
    pub sidecar_network: Option<String>,
    pub max_connections: u32,
    /// Extra `key=value` labels for the sidecar and its volumes (tests mark
    /// their objects with `io.axocoatl.test`).
    pub labels: Vec<String>,
}

impl EgressAttachment {
    pub fn new(authority: Arc<dyn EgressAuthority>) -> Self {
        Self {
            authority,
            sidecar_network: None,
            max_connections: axocoatl_exec::egress::protocol::DEFAULT_MAX_CONNECTIONS,
            labels: Vec::new(),
        }
    }
}
