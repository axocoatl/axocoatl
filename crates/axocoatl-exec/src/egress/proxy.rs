//! The egress proxy (`--egress-proxy`): listens on a Unix socket, reports each
//! request to the daemon over the control channel, and connects only to the
//! addresses the daemon allows, or carries the connection's bytes to the
//! daemon when it answers `relay`.
//!
//! It fails closed: it stops on end of the control input, a malformed daemon
//! frame, `shutdown`, or no daemon frame for `HEARTBEAT_DEAD_MS`, and a
//! request with no decision within `DECISION_TIMEOUT_MS` is answered 503.
//! The process exits when [`run`] returns, which closes every connection.
//!
//! With an identity socket (`--identity-socket`), a second listener takes
//! connections that start with the identity line the Session container's
//! init process writes ([`PeerIdentity`]); the line is required there and
//! refused on the ordinary socket.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::http::{self, HeadError, ProxyRequest};
use super::never;
use super::protocol::{
    self, CloseOutcome, DaemonFrame, PeerIdentity, RequestKind, SidecarFrame, CONNECT_TIMEOUT_MS,
    DECISION_TIMEOUT_MS, EGRESS_PROTOCOL_VERSION, HEAD_TIMEOUT_MS, HEARTBEAT_DEAD_MS,
    IDLE_TIMEOUT_MS, MAX_DATA_BYTES, MAX_HEAD_BYTES, MAX_PEER_LINE_BYTES, MAX_RELAYS,
    PEER_LINE_PREFIX, PEER_LINE_TIMEOUT_MS, RELAY_WINDOW_BYTES,
};
use super::pump::{self, pump};

/// How long to wait for `hello_ack` after `hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Most revoke ids remembered for connections not yet registered.
const MAX_EARLY_REVOKES: usize = 4096;
/// Bytes written to a relayed client before they are credited back to the
/// daemon, unless the queue empties first.
const CREDIT_BATCH_BYTES: u64 = RELAY_WINDOW_BYTES as u64 / 4;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Proxy settings. Timeouts are fields so tests can shorten them; the binary
/// always uses [`ProxyConfig::new`].
#[derive(Clone)]
pub struct ProxyConfig {
    pub max_connections: usize,
    pub version: String,
    /// The sidecar's own refusal check. Only tests replace it.
    pub never: fn(IpAddr) -> bool,
    pub decision_timeout: Duration,
    pub heartbeat_dead: Duration,
    pub head_timeout: Duration,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    /// How long the identity socket waits for the identity line.
    pub peer_timeout: Duration,
    /// Most relayed connections at once.
    pub max_relays: usize,
}

impl ProxyConfig {
    pub fn new(max_connections: usize) -> Self {
        Self {
            max_connections,
            version: crate::protocol::SUPERVISOR_VERSION.to_string(),
            never: never::is_never,
            decision_timeout: Duration::from_millis(DECISION_TIMEOUT_MS),
            heartbeat_dead: Duration::from_millis(HEARTBEAT_DEAD_MS),
            head_timeout: Duration::from_millis(HEAD_TIMEOUT_MS),
            connect_timeout: Duration::from_millis(CONNECT_TIMEOUT_MS),
            idle_timeout: Duration::from_millis(IDLE_TIMEOUT_MS),
            peer_timeout: Duration::from_millis(PEER_LINE_TIMEOUT_MS),
            max_relays: MAX_RELAYS,
        }
    }
}

/// Why the proxy stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyExit {
    ControlClosed,
    Shutdown,
    HeartbeatLost,
    Protocol(String),
}

impl ProxyExit {
    /// Process exit status: 0 only for an orderly `shutdown`.
    pub fn status(&self) -> i32 {
        match self {
            Self::Shutdown => 0,
            _ => 1,
        }
    }
}

/// Make every Unix socket this process binds from now on connectable by
/// every user (mode 0666). The proxy and bridge entry points call it once,
/// before any listener exists.
///
/// The mode is set as the socket is created, never by a later `chmod` of its
/// path: the socket directory can belong to the container's user, who could
/// swap the new socket for a symlink before a `chmod` by path, and a root
/// forwarder would then change the mode of the link's target.
pub fn connectable_sockets_by_default() {
    // SAFETY: umask only replaces the process's file-creation mask.
    unsafe {
        libc::umask(0o111);
    }
}

/// Bind a Unix listener at `path`, replacing a stale socket but never any
/// other file. Its mode comes from the umask; see
/// [`connectable_sockets_by_default`].
pub fn bind_unix_listener(path: &Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    UnixListener::bind(path)
}

/// Where the proxy takes connections from: a Unix listener, or in tests any
/// other source of connected streams.
pub trait Acceptor: Send {
    /// Called once before serving.
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }
    /// A descriptor that polls readable while a connection is waiting.
    fn ready_fd(&self) -> RawFd;
    /// The next connection; `WouldBlock` when none is waiting.
    fn accept_stream(&self) -> io::Result<UnixStream>;
}

impl Acceptor for UnixListener {
    fn prepare(&self) -> io::Result<()> {
        self.set_nonblocking(true)
    }

    fn ready_fd(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn accept_stream(&self) -> io::Result<UnixStream> {
        self.accept().map(|(stream, _)| stream)
    }
}

/// Which listener a connection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The ordinary socket: an identity line is refused.
    Plain,
    /// The identity socket: an identity line is required.
    Identity,
}

/// One relayed connection's state, shared by its connection thread and the
/// control reader.
struct Relay {
    state: Mutex<RelayState>,
    /// Written by the control reader when the state changes.
    wake: UnixStream,
}

struct RelayState {
    /// Daemon bytes not yet written to the client.
    to_client: VecDeque<u8>,
    /// Bytes written to the client and not yet credited back.
    ungranted: u64,
    daemon_eof: bool,
    /// Client bytes the daemon still accepts.
    credit: u64,
    /// The daemon broke the protocol for this connection.
    failed: Option<String>,
}

impl Relay {
    fn new() -> io::Result<(Arc<Self>, UnixStream)> {
        let (wake, woken) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        woken.set_nonblocking(true)?;
        Ok((
            Arc::new(Self {
                state: Mutex::new(RelayState {
                    to_client: VecDeque::new(),
                    ungranted: 0,
                    daemon_eof: false,
                    credit: u64::from(RELAY_WINDOW_BYTES),
                    failed: None,
                }),
                wake,
            }),
            woken,
        ))
    }

    fn wake(&self) {
        // A full socket already holds a wake-up.
        let _ = (&self.wake).write(&[1]);
    }

    fn update(&self, change: impl FnOnce(&mut RelayState)) {
        change(&mut lock(&self.state));
        self.wake();
    }
}

enum Verdict {
    Allow(Vec<IpAddr>),
    Deny {
        status: u16,
        reason: String,
        hint: String,
    },
    Relay(Arc<Relay>, UnixStream),
    RelayRefused,
}

struct Connection {
    fds: Vec<RawFd>,
    revoked: Arc<AtomicBool>,
}

/// Frames waiting for the writer: control frames always go first; frames of
/// a relayed connection's byte stream keep their order behind them.
#[derive(Default)]
struct Outbox {
    control: VecDeque<Vec<u8>>,
    stream: VecDeque<Vec<u8>>,
}

struct Shared {
    config: ProxyConfig,
    out: Mutex<Box<dyn Write + Send>>,
    outbox: Mutex<Outbox>,
    outbox_ready: Condvar,
    pending: Mutex<HashMap<u64, mpsc::Sender<Verdict>>>,
    connections: Mutex<HashMap<u64, Connection>>,
    relays: Mutex<HashMap<u64, Arc<Relay>>>,
    early_revokes: Mutex<HashSet<u64>>,
    stop: AtomicBool,
    exit: Mutex<Option<ProxyExit>>,
    active: Mutex<usize>,
    slot_freed: Condvar,
    next_id: AtomicU64,
    last_daemon_frame: Mutex<Instant>,
    acked: AtomicBool,
}

impl Shared {
    fn stop(&self, exit: ProxyExit) {
        let mut current = lock(&self.exit);
        if current.is_none() {
            *current = Some(exit);
        }
        drop(current);
        self.stop.store(true, Ordering::Release);
        self.slot_freed.notify_all();
        // Under the outbox lock, so the writer cannot miss it.
        drop(lock(&self.outbox));
        self.outbox_ready.notify_all();
        // Fail closed: every open tunnel and relay ends now, and waiting
        // requests see their decision channel close.
        let connections = lock(&self.connections);
        for connection in connections.values() {
            connection.revoked.store(true, Ordering::Release);
            for fd in &connection.fds {
                pump::shutdown(*fd, libc::SHUT_RDWR);
            }
        }
        drop(connections);
        for relay in lock(&self.relays).values() {
            relay.wake();
        }
        lock(&self.pending).clear();
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn enqueue(&self, frame: &SidecarFrame, stream: bool) -> bool {
        let line = match protocol::encode_sidecar(frame) {
            Ok(line) => line,
            Err(error) => {
                self.stop(ProxyExit::Protocol(error));
                return false;
            }
        };
        if self.stopped() {
            return false;
        }
        let mut outbox = lock(&self.outbox);
        if stream {
            outbox.stream.push_back(line);
        } else {
            outbox.control.push_back(line);
        }
        drop(outbox);
        self.outbox_ready.notify_one();
        true
    }

    /// Queue a control frame. False once the proxy is stopping.
    fn send(&self, frame: &SidecarFrame) -> bool {
        self.enqueue(frame, false)
    }

    /// Queue a frame that must stay in order with a relayed connection's
    /// `data` and `eof` frames.
    fn send_stream(&self, frame: &SidecarFrame) -> bool {
        self.enqueue(frame, true)
    }

    fn revoke(&self, ids: &[u64]) {
        let connections = lock(&self.connections);
        let mut early = lock(&self.early_revokes);
        for id in ids {
            match connections.get(id) {
                Some(connection) => {
                    connection.revoked.store(true, Ordering::Release);
                    for fd in &connection.fds {
                        pump::shutdown(*fd, libc::SHUT_RDWR);
                    }
                    // A relay waiting only for the daemon notices at once.
                    if let Some(relay) = lock(&self.relays).get(id) {
                        relay.wake();
                    }
                }
                None if early.len() < MAX_EARLY_REVOKES => {
                    early.insert(*id);
                }
                None => {}
            }
        }
    }

    /// The relay a stream frame is for. A frame for a connection this proxy
    /// never opened is a protocol error; one for a relay that has ended is
    /// dropped.
    fn relay_for(&self, id: u64) -> Result<Option<Arc<Relay>>, ProxyExit> {
        if id == 0 || id >= self.next_id.load(Ordering::Acquire) {
            return Err(ProxyExit::Protocol(format!(
                "relay frame for connection {id}, which was never opened"
            )));
        }
        Ok(lock(&self.relays).get(&id).cloned())
    }
}

fn writer(shared: Arc<Shared>) {
    loop {
        let line = {
            let mut outbox = lock(&shared.outbox);
            loop {
                if shared.stopped() {
                    return;
                }
                if let Some(line) = outbox.control.pop_front() {
                    break line;
                }
                if let Some(line) = outbox.stream.pop_front() {
                    break line;
                }
                outbox = shared
                    .outbox_ready
                    .wait(outbox)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
        };
        let mut out = lock(&shared.out);
        if out.write_all(&line).and_then(|()| out.flush()).is_err() {
            drop(out);
            shared.stop(ProxyExit::ControlClosed);
            return;
        }
    }
}

fn control_reader(shared: Arc<Shared>, input: Box<dyn Read + Send>) {
    let mut reader = BufReader::new(input);
    loop {
        if shared.stopped() {
            return;
        }
        let line = match protocol::read_frame(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => return shared.stop(ProxyExit::ControlClosed),
            Err(error) => return shared.stop(ProxyExit::Protocol(error)),
        };
        let frame = match protocol::decode_daemon(&line) {
            Ok(frame) => frame,
            Err(error) => {
                return shared.stop(ProxyExit::Protocol(format!(
                    "malformed daemon frame: {error}"
                )))
            }
        };
        *lock(&shared.last_daemon_frame) = Instant::now();
        match frame {
            DaemonFrame::HelloAck { .. } => shared.acked.store(true, Ordering::Release),
            DaemonFrame::Allow { id, addrs } => {
                if let Some(waiter) = lock(&shared.pending).remove(&id) {
                    let _ = waiter.send(Verdict::Allow(addrs));
                }
            }
            DaemonFrame::Deny {
                id,
                status,
                reason,
                hint,
            } => {
                // A revoke that arrived first for this id has nothing left to
                // close; forget it so it cannot crowd out later ones.
                lock(&shared.early_revokes).remove(&id);
                if let Some(waiter) = lock(&shared.pending).remove(&id) {
                    let _ = waiter.send(Verdict::Deny {
                        status,
                        reason,
                        hint,
                    });
                }
            }
            DaemonFrame::Relay { id } => {
                let Some(waiter) = lock(&shared.pending).remove(&id) else {
                    // A late answer: the request already gave up and said so.
                    continue;
                };
                let mut relays = lock(&shared.relays);
                if relays.len() >= shared.config.max_relays {
                    drop(relays);
                    let _ = waiter.send(Verdict::RelayRefused);
                    continue;
                }
                match Relay::new() {
                    Ok((relay, woken)) => {
                        relays.insert(id, relay.clone());
                        drop(relays);
                        if waiter.send(Verdict::Relay(relay, woken)).is_err() {
                            lock(&shared.relays).remove(&id);
                        }
                    }
                    Err(_) => {
                        drop(relays);
                        let _ = waiter.send(Verdict::RelayRefused);
                    }
                }
            }
            DaemonFrame::Data { id, b } => {
                let relay = match shared.relay_for(id) {
                    Ok(relay) => relay,
                    Err(exit) => return shared.stop(exit),
                };
                let Some(relay) = relay else { continue };
                // Validated when decoded.
                let bytes = protocol::decode_data(&b).unwrap_or_default();
                relay.update(|state| {
                    if state.daemon_eof {
                        state.failed = Some("the daemon sent data after its end".into());
                    }
                    state.to_client.extend(&bytes);
                    if state.to_client.len() as u64 + state.ungranted
                        > u64::from(RELAY_WINDOW_BYTES)
                    {
                        state.failed = Some("the daemon sent more than its relay window".into());
                    }
                });
            }
            DaemonFrame::Eof { id } => {
                let relay = match shared.relay_for(id) {
                    Ok(relay) => relay,
                    Err(exit) => return shared.stop(exit),
                };
                if let Some(relay) = relay {
                    relay.update(|state| state.daemon_eof = true);
                }
            }
            DaemonFrame::Credit { id, bytes } => {
                let relay = match shared.relay_for(id) {
                    Ok(relay) => relay,
                    Err(exit) => return shared.stop(exit),
                };
                if let Some(relay) = relay {
                    relay.update(|state| {
                        state.credit += u64::from(bytes);
                        if state.credit > u64::from(RELAY_WINDOW_BYTES) {
                            state.failed =
                                Some("the daemon granted more than the relay window".into());
                        }
                    });
                }
            }
            DaemonFrame::Revoke { ids } => shared.revoke(&ids),
            DaemonFrame::Ping => {
                shared.send(&SidecarFrame::Pong);
            }
            DaemonFrame::Shutdown => return shared.stop(ProxyExit::Shutdown),
        }
    }
}

fn watchdog(shared: Arc<Shared>) {
    while !shared.stopped() {
        std::thread::sleep(Duration::from_millis(50).min(shared.config.heartbeat_dead / 4));
        let last = *lock(&shared.last_daemon_frame);
        if last.elapsed() > shared.config.heartbeat_dead {
            shared.stop(ProxyExit::HeartbeatLost);
        }
    }
}

/// Write a response and close our side, then briefly drain what the client
/// is still sending so the response is not lost to a reset.
fn respond(stream: &mut UnixStream, response: &[u8]) {
    let _ = stream.write_all(response);
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < 64 * 1024 {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(read) => drained += read,
        }
    }
}

enum HeadRead {
    Complete(Vec<u8>, usize),
    Refused(&'static str, &'static str),
    Gone,
}

/// What reading from the client found before its deadline.
enum Fill {
    More,
    TimedOut,
    Gone,
}

fn fill(stream: &mut UnixStream, buffer: &mut Vec<u8>, deadline: Instant) -> Fill {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Fill::TimedOut;
    }
    if stream.set_read_timeout(Some(remaining)).is_err() {
        return Fill::Gone;
    }
    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk) {
        Ok(0) => Fill::Gone,
        Ok(read) => {
            buffer.extend_from_slice(&chunk[..read]);
            Fill::More
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Fill::TimedOut
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Fill::More,
        Err(_) => Fill::Gone,
    }
}

const IDENTITY_NOT_ACCEPTED: &str =
    "Only the Session container's init process sends an identity line, on its own socket.";
const IDENTITY_REQUIRED: &str =
    "Connections on this socket start with the identity line the Session container's init process writes.";
const BAD_IDENTITY: &str = "The identity line is malformed or longer than 4 KiB.";

/// Starts like an identity line, as far as `buffer` goes.
fn looks_like_identity(buffer: &[u8]) -> bool {
    let shared = buffer.len().min(PEER_LINE_PREFIX.len());
    shared > 0 && buffer[..shared] == PEER_LINE_PREFIX[..shared]
}

fn read_head(stream: &mut UnixStream, timeout: Duration, mut buffer: Vec<u8>) -> HeadRead {
    let deadline = Instant::now() + timeout;
    loop {
        // A forged identity line on the ordinary socket is refused as such.
        if buffer.len() >= PEER_LINE_PREFIX.len() && buffer.starts_with(PEER_LINE_PREFIX) {
            return HeadRead::Refused("identity_not_accepted", IDENTITY_NOT_ACCEPTED);
        }
        if let Some(end) = http::find_head_end(&buffer) {
            if end > MAX_HEAD_BYTES {
                return HeadRead::Refused(HeadError::TooLarge.reason(), HeadError::TooLarge.hint());
            }
            return HeadRead::Complete(buffer, end);
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return HeadRead::Refused(HeadError::TooLarge.reason(), HeadError::TooLarge.hint());
        }
        match fill(stream, &mut buffer, deadline) {
            Fill::More => {}
            Fill::TimedOut => {
                return HeadRead::Refused(
                    "head_timeout",
                    "The request head did not arrive within 10 seconds.",
                )
            }
            Fill::Gone => return HeadRead::Gone,
        }
    }
}

/// A refusal's reason code and hint; `None` when the client left.
type Refusal = Option<(&'static str, &'static str)>;

/// Read the identity line an identity-socket connection must start with.
/// Returns the identity and the bytes after its line.
fn read_identity(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<(PeerIdentity, Vec<u8>), Refusal> {
    let deadline = Instant::now() + timeout;
    let mut buffer = Vec::with_capacity(512);
    loop {
        if !looks_like_identity(&buffer) && !buffer.is_empty() {
            return Err(Some(("identity_required", IDENTITY_REQUIRED)));
        }
        if let Some(end) = buffer.windows(2).position(|pair| pair == b"\r\n") {
            if end + 2 > MAX_PEER_LINE_BYTES {
                return Err(Some(("bad_identity", BAD_IDENTITY)));
            }
            let identity = PeerIdentity::parse_line(&buffer[..end])
                .map_err(|_| Some(("bad_identity", BAD_IDENTITY)))?;
            return Ok((identity, buffer.split_off(end + 2)));
        }
        if buffer.len() >= MAX_PEER_LINE_BYTES {
            return Err(Some(("bad_identity", BAD_IDENTITY)));
        }
        match fill(stream, &mut buffer, deadline) {
            Fill::More => {}
            Fill::TimedOut => return Err(Some(("identity_required", IDENTITY_REQUIRED))),
            Fill::Gone => return Err(None),
        }
    }
}

fn close_frame(
    id: u64,
    ip: Option<IpAddr>,
    started: Instant,
    report: pump::PumpReport,
) -> SidecarFrame {
    SidecarFrame::Close {
        id,
        ip,
        up: report.up,
        down: report.down,
        ms: started.elapsed().as_millis() as u64,
        outcome: report.outcome,
        error: report
            .error
            .map(|error| error.chars().take(protocol::MAX_DETAIL_CHARS).collect()),
    }
}

/// The answer for a connection revoked before its tunnel opened: its
/// credential or allow rule ended while it was decided or connected.
fn revoked_response(request: &ProxyRequest) -> Vec<u8> {
    http::refusal_response(
        403,
        "revoked",
        &request.host,
        request.port,
        "The credential or allow rule that admitted this connection ended before it opened; retry if it should still be allowed.",
    )
}

fn failed(outcome: CloseOutcome, error: &str) -> pump::PumpReport {
    pump::PumpReport {
        up: 0,
        down: 0,
        outcome,
        error: Some(error.to_string()),
    }
}

fn connect_any(
    addrs: &[IpAddr],
    port: u16,
    timeout: Duration,
) -> Result<(TcpStream, IpAddr), String> {
    let mut last = String::from("no address");
    for addr in addrs {
        match TcpStream::connect_timeout(&SocketAddr::new(*addr, port), timeout) {
            Ok(stream) => return Ok((stream, *addr)),
            Err(error) => last = format!("{addr}: {error}"),
        }
    }
    Err(last)
}

/// Wait for this request's decision. A decision that crosses the timeout is
/// still taken, so a relay the control reader set up is never orphaned.
fn await_verdict(
    shared: &Shared,
    id: u64,
    verdict: &mpsc::Receiver<Verdict>,
) -> Result<Verdict, mpsc::RecvTimeoutError> {
    match verdict.recv_timeout(shared.config.decision_timeout) {
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if lock(&shared.pending).remove(&id).is_some() {
                Err(mpsc::RecvTimeoutError::Timeout)
            } else {
                // The control reader took this request's waiter; its answer
                // is on the way.
                verdict
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            }
        }
        other => other,
    }
}

fn handle(shared: Arc<Shared>, mut client: UnixStream, origin: Origin) {
    let started = Instant::now();
    let (peer, initial) = match origin {
        Origin::Plain => (None, Vec::new()),
        Origin::Identity => match read_identity(&mut client, shared.config.peer_timeout) {
            Ok((peer, rest)) => (Some(peer), rest),
            Err(Some((reason, hint))) => {
                return respond(
                    &mut client,
                    &http::refusal_response(400, reason, "", 0, hint),
                )
            }
            Err(None) => return,
        },
    };
    let (buffer, end) = match read_head(&mut client, shared.config.head_timeout, initial) {
        HeadRead::Complete(buffer, end) => (buffer, end),
        HeadRead::Refused(reason, hint) => {
            return respond(
                &mut client,
                &http::refusal_response(400, reason, "", 0, hint),
            )
        }
        HeadRead::Gone => return,
    };
    let request: ProxyRequest = match http::parse_head(&buffer[..end]) {
        Ok(request) => request,
        Err(error) => {
            return respond(
                &mut client,
                &http::refusal_response(400, error.reason(), "", 0, error.hint()),
            )
        }
    };
    let leftover = &buffer[end..];
    let id = shared.next_id.fetch_add(1, Ordering::AcqRel);
    let (waiter, verdict) = mpsc::channel();
    lock(&shared.pending).insert(id, waiter);
    let sent = shared.send(&SidecarFrame::Open {
        id,
        kind: request.kind,
        host: request.host.clone(),
        port: request.port,
        auth: request.auth.clone(),
        method: request.method.clone(),
        path: request.path.clone(),
        peer,
    });
    let unavailable = |client: &mut UnixStream, reason: &str, hint: &str| {
        respond(
            client,
            &http::refusal_response(503, reason, &request.host, request.port, hint),
        )
    };
    if !sent {
        lock(&shared.pending).remove(&id);
        return unavailable(
            &mut client,
            "egress_stopped",
            "Axocoatl's egress proxy is stopping; retry shortly.",
        );
    }
    let verdict = match await_verdict(&shared, id, &verdict) {
        Ok(verdict) => verdict,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            unavailable(
                &mut client,
                "decision_timeout",
                "Axocoatl did not decide on this connection in time; retry shortly.",
            );
            shared.send(&close_frame(
                id,
                None,
                started,
                failed(CloseOutcome::Interrupted, "no decision within 10 seconds"),
            ));
            return;
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return unavailable(
                &mut client,
                "egress_stopped",
                "Axocoatl's egress proxy is stopping; retry shortly.",
            )
        }
    };
    let addrs = match verdict {
        Verdict::Deny {
            status,
            reason,
            hint,
        } => {
            return respond(
                &mut client,
                &http::refusal_response(status, &reason, &request.host, request.port, &hint),
            )
        }
        Verdict::Allow(addrs) => addrs,
        Verdict::RelayRefused => {
            lock(&shared.early_revokes).remove(&id);
            unavailable(
                &mut client,
                "relay_capacity",
                "Axocoatl's egress proxy is carrying as many relayed connections as it can; retry shortly.",
            );
            shared.send_stream(&close_frame(
                id,
                None,
                started,
                failed(
                    CloseOutcome::UpstreamFailed,
                    "the egress proxy carries no more relayed connections",
                ),
            ));
            return;
        }
        Verdict::Relay(relay, woken) => {
            return handle_relay(
                shared, client, id, &request, leftover, relay, woken, started,
            )
        }
    };
    if addrs.iter().any(|addr| (shared.config.never)(*addr)) {
        respond(
            &mut client,
            &http::refusal_response(
                403,
                "forbidden_destination",
                &request.host,
                request.port,
                "The destination is a loopback, link-local or other special address, which Axocoatl never allows.",
            ),
        );
        shared.send(&close_frame(
            id,
            None,
            started,
            failed(
                CloseOutcome::UpstreamFailed,
                "the egress proxy refused a forbidden destination",
            ),
        ));
        return;
    }
    // Revoked while the decision was on its way: never connect.
    let revoked_early = lock(&shared.early_revokes).remove(&id);
    if revoked_early {
        respond(&mut client, &revoked_response(&request));
        shared.send(&close_frame(
            id,
            None,
            started,
            failed(CloseOutcome::Revoked, "revoked before the tunnel opened"),
        ));
        return;
    }
    let (upstream, ip) = match connect_any(&addrs, request.port, shared.config.connect_timeout) {
        Ok(connected) => connected,
        Err(error) => {
            respond(
                &mut client,
                &http::refusal_response(
                    502,
                    "upstream_unreachable",
                    &request.host,
                    request.port,
                    &format!(
                        "{}:{} did not accept a connection.",
                        request.host, request.port
                    ),
                ),
            );
            shared.send(&close_frame(
                id,
                None,
                started,
                failed(CloseOutcome::UpstreamFailed, &error),
            ));
            return;
        }
    };
    let revoked = Arc::new(AtomicBool::new(false));
    let revoked_while_connecting = register(
        &shared,
        id,
        vec![client.as_raw_fd(), upstream.as_raw_fd()],
        &revoked,
    );
    let report = if revoked_while_connecting {
        // Never announce a tunnel that was revoked while it was being made.
        respond(&mut client, &revoked_response(&request));
        failed(CloseOutcome::Revoked, "revoked before the tunnel opened")
    } else if revoked.load(Ordering::Acquire) {
        failed(CloseOutcome::Revoked, "revoked before the tunnel opened")
    } else {
        let initial = match request.kind {
            RequestKind::Connect => {
                if client.write_all(http::CONNECT_ESTABLISHED).is_err() {
                    Vec::new()
                } else {
                    leftover.to_vec()
                }
            }
            RequestKind::Http => {
                let mut initial = request.upstream_head.clone().unwrap_or_default();
                initial.extend_from_slice(leftover);
                initial
            }
        };
        pump(
            client.as_raw_fd(),
            upstream.as_raw_fd(),
            &initial,
            shared.config.idle_timeout,
            &revoked,
        )
    };
    // Unregister before the descriptors close, so a revoke never shuts down
    // a reused descriptor.
    lock(&shared.connections).remove(&id);
    drop(upstream);
    drop(client);
    shared.send(&close_frame(
        id,
        Some(ip),
        started,
        interrupted_if_stopped(&shared, report),
    ));
}

/// Register a connection's descriptors for revoke and stop. True when a
/// revoke for it arrived before it was registered.
fn register(shared: &Shared, id: u64, fds: Vec<RawFd>, revoked: &Arc<AtomicBool>) -> bool {
    let mut connections = lock(&shared.connections);
    let early = lock(&shared.early_revokes).remove(&id);
    if early || shared.stopped() {
        revoked.store(true, Ordering::Release);
    }
    connections.insert(
        id,
        Connection {
            fds,
            revoked: revoked.clone(),
        },
    );
    early
}

fn interrupted_if_stopped(shared: &Shared, report: pump::PumpReport) -> pump::PumpReport {
    if shared.stopped() && report.outcome != CloseOutcome::Closed {
        pump::PumpReport {
            outcome: CloseOutcome::Interrupted,
            ..report
        }
    } else {
        report
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_relay(
    shared: Arc<Shared>,
    mut client: UnixStream,
    id: u64,
    request: &ProxyRequest,
    leftover: &[u8],
    relay: Arc<Relay>,
    woken: UnixStream,
    started: Instant,
) {
    let finish = |shared: &Shared, report: pump::PumpReport| {
        lock(&shared.relays).remove(&id);
        // Behind the relay's own data and end, so the daemon reads the
        // whole stream before its close.
        shared.send_stream(&close_frame(
            id,
            None,
            started,
            interrupted_if_stopped(shared, report),
        ));
    };
    if request.kind != RequestKind::Connect {
        lock(&shared.early_revokes).remove(&id);
        respond(
            &mut client,
            &http::refusal_response(
                502,
                "relay_not_supported",
                &request.host,
                request.port,
                "Axocoatl answered a plain-HTTP request with a relay, which only CONNECT requests support.",
            ),
        );
        return finish(
            &shared,
            failed(
                CloseOutcome::UpstreamFailed,
                "the daemon relayed a plain-HTTP request",
            ),
        );
    }
    let revoked = Arc::new(AtomicBool::new(false));
    if register(&shared, id, vec![client.as_raw_fd()], &revoked) {
        respond(&mut client, &revoked_response(request));
        lock(&shared.connections).remove(&id);
        return finish(
            &shared,
            failed(CloseOutcome::Revoked, "revoked before the relay opened"),
        );
    }
    let report = if revoked.load(Ordering::Acquire) {
        failed(CloseOutcome::Revoked, "revoked before the relay opened")
    } else if client.write_all(http::CONNECT_ESTABLISHED).is_err() {
        failed(
            CloseOutcome::Reset,
            "the client left before the relay opened",
        )
    } else {
        relay_pump(&shared, id, &client, leftover, &relay, &woken, &revoked)
    };
    lock(&shared.connections).remove(&id);
    drop(client);
    finish(&shared, report);
}

/// Carry a relayed connection's bytes between its client and the control
/// channel until both directions end, the connection is revoked or idle, or
/// either side fails.
fn relay_pump(
    shared: &Shared,
    id: u64,
    client: &UnixStream,
    leftover: &[u8],
    relay: &Relay,
    woken: &UnixStream,
    revoked: &AtomicBool,
) -> pump::PumpReport {
    let mut report = pump::PumpReport {
        up: 0,
        down: 0,
        outcome: CloseOutcome::Closed,
        error: None,
    };
    let fd = client.as_raw_fd();
    pump::no_sigpipe(fd);
    if let Err(error) = pump::set_nonblocking(fd) {
        report.outcome = CloseOutcome::Reset;
        report.error = Some(error.to_string());
        return report;
    }
    let send_data = |bytes: &[u8]| {
        shared.send_stream(&SidecarFrame::Data {
            id,
            b: protocol::base64_encode(bytes),
        })
    };
    // Bytes the client sent with its request head; within the first window.
    for chunk in leftover.chunks(MAX_DATA_BYTES) {
        send_data(chunk);
        report.up += chunk.len() as u64;
    }
    lock(&relay.state).credit -= leftover.len() as u64;
    let (mut client_eof, mut eof_sent, mut client_shut, mut hangup) = (false, false, false, false);
    let mut last_activity = Instant::now();
    let mut buffer = vec![0u8; MAX_DATA_BYTES];
    loop {
        if revoked.load(Ordering::Acquire) {
            report.outcome = CloseOutcome::Revoked;
            return report;
        }
        if shared.stopped() {
            report.outcome = CloseOutcome::Interrupted;
            return report;
        }
        let mut want_write = false;
        let mut grant = 0u64;
        let credit;
        {
            let mut state = lock(&relay.state);
            if let Some(error) = state.failed.take() {
                report.outcome = CloseOutcome::Reset;
                report.error = Some(error);
                return report;
            }
            // Daemon bytes to the client.
            while !state.to_client.is_empty() {
                let (front, _) = state.to_client.as_slices();
                match pump::send_some(fd, front) {
                    Ok(0) => break,
                    Ok(written) => {
                        state.to_client.drain(..written);
                        state.ungranted += written as u64;
                        report.down += written as u64;
                        last_activity = Instant::now();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        want_write = true;
                        break;
                    }
                    Err(error) => {
                        report.outcome = if revoked.load(Ordering::Acquire) {
                            CloseOutcome::Revoked
                        } else {
                            CloseOutcome::Reset
                        };
                        report.error = Some(error.to_string());
                        return report;
                    }
                }
            }
            if state.ungranted >= CREDIT_BATCH_BYTES
                || (state.ungranted > 0 && state.to_client.is_empty())
            {
                grant = state.ungranted;
                state.ungranted = 0;
            }
            if state.daemon_eof && state.to_client.is_empty() && !client_shut {
                pump::shutdown(fd, libc::SHUT_WR);
                client_shut = true;
            }
            credit = state.credit;
        }
        if grant > 0 {
            shared.send(&SidecarFrame::Credit {
                id,
                bytes: grant as u32,
            });
        }
        // Client bytes to the daemon, as far as its credit goes.
        let mut want_read = false;
        if !client_eof && credit > 0 {
            let room = (credit as usize).min(MAX_DATA_BYTES);
            // SAFETY: the pointer and length describe the writable buffer.
            let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), room) };
            if read > 0 {
                let read = read as usize;
                lock(&relay.state).credit -= read as u64;
                send_data(&buffer[..read]);
                report.up += read as u64;
                last_activity = Instant::now();
                continue;
            } else if read == 0 {
                client_eof = true;
            } else {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => want_read = true,
                    _ => {
                        report.outcome = if revoked.load(Ordering::Acquire) {
                            CloseOutcome::Revoked
                        } else {
                            CloseOutcome::Reset
                        };
                        report.error = Some(error.to_string());
                        return report;
                    }
                }
            }
        }
        if client_eof && !eof_sent {
            shared.send_stream(&SidecarFrame::Eof { id });
            eof_sent = true;
        }
        if eof_sent && client_shut {
            return report;
        }
        let remaining = shared
            .config
            .idle_timeout
            .saturating_sub(last_activity.elapsed());
        if remaining.is_zero() {
            report.outcome = CloseOutcome::IdleTimeout;
            return report;
        }
        let events = (if want_read { libc::POLLIN } else { 0 })
            | (if want_write { libc::POLLOUT } else { 0 });
        let mut fds = [
            libc::pollfd {
                // A client that hung up while nothing could be read or
                // written waits for the daemon instead of spinning.
                fd: if hangup && events == 0 { -1 } else { fd },
                events,
                revents: 0,
            },
            libc::pollfd {
                fd: woken.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let wait = remaining.min(Duration::from_millis(500)).as_millis() as libc::c_int;
        // SAFETY: fds is a live array of two pollfd values.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait.max(1)) };
        if ready > 0 {
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 && events == 0 {
                hangup = true;
            }
            if fds[1].revents & libc::POLLIN != 0 {
                let mut sink = [0u8; 64];
                while matches!((&*woken).read(&mut sink), Ok(read) if read > 0) {}
            }
        }
    }
}

/// Run the proxy on one listener until it must stop. See [`run_with`].
pub fn run(
    config: ProxyConfig,
    listener: UnixListener,
    control_in: Box<dyn Read + Send>,
    control_out: Box<dyn Write + Send>,
) -> ProxyExit {
    run_with(config, Box::new(listener), None, control_in, control_out)
}

/// Run the proxy until it must stop. Sends `hello`, waits for `hello_ack`,
/// then serves `plain` and, when given, the identity socket `identity`.
/// Never returns before the control channel or the daemon tells it to stop.
pub fn run_with(
    config: ProxyConfig,
    plain: Box<dyn Acceptor>,
    identity: Option<Box<dyn Acceptor>>,
    control_in: Box<dyn Read + Send>,
    control_out: Box<dyn Write + Send>,
) -> ProxyExit {
    let max_connections = config.max_connections.max(1);
    let shared = Arc::new(Shared {
        config,
        out: Mutex::new(control_out),
        outbox: Mutex::new(Outbox::default()),
        outbox_ready: Condvar::new(),
        pending: Mutex::new(HashMap::new()),
        connections: Mutex::new(HashMap::new()),
        relays: Mutex::new(HashMap::new()),
        early_revokes: Mutex::new(HashSet::new()),
        stop: AtomicBool::new(false),
        exit: Mutex::new(None),
        active: Mutex::new(0),
        slot_freed: Condvar::new(),
        next_id: AtomicU64::new(1),
        last_daemon_frame: Mutex::new(Instant::now()),
        acked: AtomicBool::new(false),
    });
    let writer_shared = shared.clone();
    let writer = std::thread::spawn(move || writer(writer_shared));
    let reader_shared = shared.clone();
    std::thread::spawn(move || control_reader(reader_shared, control_in));
    let watchdog_shared = shared.clone();
    std::thread::spawn(move || watchdog(watchdog_shared));
    shared.send(&SidecarFrame::Hello {
        protocol: EGRESS_PROTOCOL_VERSION,
        version: shared.config.version.clone(),
        max_connections: max_connections as u32,
    });
    let hello_deadline = Instant::now() + HELLO_TIMEOUT.min(shared.config.heartbeat_dead);
    while !shared.acked.load(Ordering::Acquire) && !shared.stopped() {
        if Instant::now() > hello_deadline {
            shared.stop(ProxyExit::Protocol("no hello_ack from the daemon".into()));
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let listeners: Vec<(Box<dyn Acceptor>, Origin)> = std::iter::once((plain, Origin::Plain))
        .chain(identity.map(|identity| (identity, Origin::Identity)))
        .collect();
    for (listener, _) in &listeners {
        if let Err(error) = listener.prepare() {
            shared.stop(ProxyExit::Protocol(format!("listener: {error}")));
        }
    }
    while !shared.stopped() {
        {
            let mut active = lock(&shared.active);
            while *active >= max_connections && !shared.stopped() {
                active = shared
                    .slot_freed
                    .wait_timeout(active, Duration::from_millis(200))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
        }
        if shared.stopped() {
            break;
        }
        let mut entries: Vec<libc::pollfd> = listeners
            .iter()
            .map(|(listener, _)| libc::pollfd {
                fd: listener.ready_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: entries is a live array of pollfd values of this length.
        unsafe { libc::poll(entries.as_mut_ptr(), entries.len() as libc::nfds_t, 100) };
        for ((listener, origin), entry) in listeners.iter().zip(&entries) {
            if entry.revents == 0 || shared.stopped() {
                continue;
            }
            match listener.accept_stream() {
                Ok(stream) => {
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    *lock(&shared.active) += 1;
                    let connection_shared = shared.clone();
                    let origin = *origin;
                    std::thread::spawn(move || {
                        handle(connection_shared.clone(), stream, origin);
                        let mut active = lock(&connection_shared.active);
                        *active -= 1;
                        connection_shared.slot_freed.notify_one();
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => shared.stop(ProxyExit::Protocol(format!("accept: {error}"))),
            }
        }
    }
    let _ = writer.join();
    let exit = lock(&shared.exit)
        .clone()
        .unwrap_or(ProxyExit::ControlClosed);
    if let ProxyExit::Protocol(detail) = &exit {
        let mut out = lock(&shared.out);
        if let Ok(line) = protocol::encode_sidecar(&SidecarFrame::Fatal {
            detail: detail.chars().take(protocol::MAX_DETAIL_CHARS).collect(),
        }) {
            let _ = out.write_all(&line).and_then(|()| out.flush());
        }
    }
    exit
}

/// `--egress-proxy --socket <path> [--identity-socket <path>]
/// [--max-connections N]` with the process's stdin and stdout as the control
/// channel. Returns the exit status.
pub fn main(socket: PathBuf, identity_socket: Option<PathBuf>, max_connections: usize) -> i32 {
    connectable_sockets_by_default();
    let bind = |path: &Path| {
        bind_unix_listener(path).map_err(|error| {
            eprintln!("egress proxy: cannot listen on {}: {error}", path.display());
        })
    };
    let Ok(listener) = bind(&socket) else {
        return 1;
    };
    let identity = match identity_socket.as_deref().map(bind).transpose() {
        Ok(identity) => identity.map(|listener| Box::new(listener) as Box<dyn Acceptor>),
        Err(()) => return 1,
    };
    let exit = run_with(
        ProxyConfig::new(max_connections),
        Box::new(listener),
        identity,
        Box::new(io::stdin()),
        Box::new(io::stdout()),
    );
    if exit != ProxyExit::Shutdown {
        eprintln!("egress proxy stopped: {exit:?}");
    }
    exit.status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    /// Hands the proxy connected socket pairs, so these tests bind no Unix
    /// socket path (a long `TMPDIR` cannot hold one on macOS).
    struct TestAcceptor {
        incoming: Mutex<mpsc::Receiver<UnixStream>>,
        ready: UnixStream,
    }

    struct Dialer {
        outgoing: mpsc::Sender<UnixStream>,
        ready: UnixStream,
    }

    impl Dialer {
        fn connect(&self) -> UnixStream {
            let (client, proxy_end) = UnixStream::pair().unwrap();
            if self.outgoing.send(proxy_end).is_ok() {
                (&self.ready).write_all(&[1]).unwrap();
            }
            client
        }
    }

    fn acceptor() -> (TestAcceptor, Dialer) {
        let (outgoing, incoming) = mpsc::channel();
        let (ready, signal) = UnixStream::pair().unwrap();
        ready.set_nonblocking(true).unwrap();
        (
            TestAcceptor {
                incoming: Mutex::new(incoming),
                ready,
            },
            Dialer {
                outgoing,
                ready: signal,
            },
        )
    }

    impl Acceptor for TestAcceptor {
        fn ready_fd(&self) -> RawFd {
            self.ready.as_raw_fd()
        }

        fn accept_stream(&self) -> io::Result<UnixStream> {
            let mut byte = [0u8; 1];
            match (&self.ready).read(&mut byte) {
                Ok(1) => {}
                Ok(_) => return Err(io::ErrorKind::BrokenPipe.into()),
                Err(error) => return Err(error),
            }
            lock(&self.incoming)
                .try_recv()
                .map_err(|_| io::ErrorKind::WouldBlock.into())
        }
    }

    struct FakeDaemon {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl FakeDaemon {
        fn frame(&mut self) -> SidecarFrame {
            let mut line = Vec::new();
            self.reader.read_until(b'\n', &mut line).unwrap();
            assert!(!line.is_empty(), "control channel closed");
            protocol::decode_sidecar(&line).unwrap()
        }

        fn frame_timeout(&mut self, timeout: Duration) -> Option<SidecarFrame> {
            self.reader
                .get_ref()
                .set_read_timeout(Some(timeout))
                .unwrap();
            let mut line = Vec::new();
            let result = match self.reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(protocol::decode_sidecar(&line).unwrap()),
            };
            self.reader.get_ref().set_read_timeout(None).unwrap();
            result
        }

        /// The next frame other than `credit`.
        fn frame_skipping_credit(&mut self) -> SidecarFrame {
            loop {
                let frame = self.frame();
                if !matches!(frame, SidecarFrame::Credit { .. }) {
                    return frame;
                }
            }
        }

        fn send(&mut self, frame: DaemonFrame) {
            self.writer
                .write_all(&protocol::encode_daemon(&frame).unwrap())
                .unwrap();
        }

        fn send_data(&mut self, id: u64, bytes: &[u8]) {
            for chunk in bytes.chunks(MAX_DATA_BYTES) {
                self.send(DaemonFrame::Data {
                    id,
                    b: protocol::base64_encode(chunk),
                });
            }
        }

        fn open(&mut self) -> (u64, String, u16, Option<String>) {
            match self.frame() {
                SidecarFrame::Open {
                    id,
                    host,
                    port,
                    auth,
                    ..
                } => (id, host, port, auth),
                other => panic!("expected open, got {other:?}"),
            }
        }

        /// Bytes of `data` frames for `id` until `count` arrived; other
        /// frames except `credit` fail the test.
        fn data(&mut self, id: u64, count: usize) -> Vec<u8> {
            let mut received = Vec::new();
            while received.len() < count {
                match self.frame_skipping_credit() {
                    SidecarFrame::Data { id: of, b } if of == id => {
                        received.extend(protocol::decode_data(&b).unwrap())
                    }
                    other => panic!("expected data, got {other:?}"),
                }
            }
            received
        }
    }

    struct Harness {
        daemon: FakeDaemon,
        plain: Dialer,
        identity: Dialer,
        proxy: std::thread::JoinHandle<ProxyExit>,
    }

    impl Harness {
        fn connect(&self) -> UnixStream {
            self.plain.connect()
        }
    }

    fn allow_loopback(_: IpAddr) -> bool {
        false
    }

    fn start(configure: impl FnOnce(&mut ProxyConfig)) -> Harness {
        let (plain, plain_dialer) = acceptor();
        let (identity, identity_dialer) = acceptor();
        let (daemon_end, proxy_end) = UnixStream::pair().unwrap();
        let mut config = ProxyConfig::new(8);
        config.never = allow_loopback;
        configure(&mut config);
        let proxy_in = proxy_end.try_clone().unwrap();
        let proxy = std::thread::spawn(move || {
            run_with(
                config,
                Box::new(plain),
                Some(Box::new(identity)),
                Box::new(proxy_in),
                Box::new(proxy_end),
            )
        });
        let mut daemon = FakeDaemon {
            reader: BufReader::new(daemon_end.try_clone().unwrap()),
            writer: daemon_end,
        };
        match daemon.frame() {
            SidecarFrame::Hello { protocol, .. } => assert_eq!(protocol, 2),
            other => panic!("expected hello, got {other:?}"),
        }
        daemon.send(DaemonFrame::HelloAck { protocol: 2 });
        Harness {
            daemon,
            plain: plain_dialer,
            identity: identity_dialer,
            proxy,
        }
    }

    fn basic(token: &str) -> String {
        protocol::base64_encode(format!("axo:{token}").as_bytes())
    }

    fn read_response(stream: &mut UnixStream) -> String {
        // macOS refuses a timeout on a socket whose peer has already shut it
        // down (EINVAL); the read below then returns at once.
        if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(10))) {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        }
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    }

    fn echo_server() -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut stream = stream.unwrap();
                let mut buffer = [0u8; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if stream.write_all(&buffer[..read]).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        (port, handle)
    }

    #[test]
    fn connects_only_after_allow_and_reports_the_close() {
        let mut harness = start(|_| {});
        let (port, upstream) = echo_server();
        let mut client = harness.connect();
        write!(
            client,
            "CONNECT localhost.test:{port} HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\nearly",
            basic("axe_secret")
        )
        .unwrap();
        let (id, host, open_port, auth) = harness.daemon.open();
        assert_eq!((host.as_str(), open_port), ("localhost.test", port));
        assert_eq!(auth, Some(protocol::credential_hash("axe_secret")));
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        let mut established = [0u8; 39];
        client.read_exact(&mut established).unwrap();
        assert_eq!(&established, http::CONNECT_ESTABLISHED);
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"early");
        client.write_all(b"ping").unwrap();
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"ping");
        client.shutdown(Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap();
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                ip,
                up,
                down,
                outcome,
                ..
            } => {
                assert_eq!(closed, id);
                assert_eq!(ip, Some("127.0.0.1".parse().unwrap()));
                assert_eq!((up, down), (9, 9));
                assert_eq!(outcome, CloseOutcome::Closed);
            }
            other => panic!("expected close, got {other:?}"),
        }
        upstream.join().unwrap();
        harness.daemon.send(DaemonFrame::Shutdown);
        assert_eq!(harness.proxy.join().unwrap(), ProxyExit::Shutdown);
    }

    #[test]
    fn plain_http_is_rewritten_and_sent_once() {
        let mut harness = start(|_| {});
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 1024];
            while http::find_head_end(&received).is_none() {
                let read = stream.read(&mut buffer).unwrap();
                received.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            String::from_utf8(received).unwrap()
        });
        let mut client = harness.connect();
        write!(
            client,
            "GET http://app.test:{port}/x?y=1 HTTP/1.1\r\nHost: app.test:{port}\r\nProxy-Authorization: Basic {}\r\nProxy-Connection: keep-alive\r\n\r\n",
            basic("t")
        )
        .unwrap();
        let open = harness.daemon.frame();
        let SidecarFrame::Open {
            id,
            kind,
            method,
            path,
            peer,
            ..
        } = open
        else {
            panic!("{open:?}")
        };
        assert_eq!(kind, RequestKind::Http);
        assert_eq!(peer, None);
        assert_eq!(
            (method.as_deref(), path.as_deref()),
            (Some("GET"), Some("/x"))
        );
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        // Like any client of a `Connection: close` response, close our side.
        drop(client);
        let head = upstream.join().unwrap();
        assert_eq!(
            head,
            format!("GET /x?y=1 HTTP/1.1\r\nHost: app.test:{port}\r\nConnection: close\r\n\r\n")
        );
        assert!(matches!(harness.daemon.frame(), SidecarFrame::Close { .. }));
    }

    #[test]
    fn deny_returns_the_status_and_body_and_never_connects() {
        let mut harness = start(|_| {});
        let mut client = harness.connect();
        client
            .write_all(b"CONNECT evil.test:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let (id, _, _, auth) = harness.daemon.open();
        assert_eq!(auth, None);
        harness.daemon.send(DaemonFrame::Deny {
            id,
            status: 407,
            reason: "no_credential".into(),
            hint: "This process has no egress credential.".into(),
        });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 407 "), "{response}");
        assert!(response.contains("Proxy-Authenticate: Basic realm=\"axocoatl-egress\""));
        assert!(response.contains("X-Axocoatl-Egress: denied; reason=no_credential"));
        assert!(response.contains("\"host\":\"evil.test\""));
        // No close follows a denial.
        assert!(harness
            .daemon
            .frame_timeout(Duration::from_millis(200))
            .is_none());

        let mut client = harness.connect();
        client
            .write_all(b"CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let (id, host, _, _) = harness.daemon.open();
        assert_eq!(host, "1.1.1.1");
        harness.daemon.send(DaemonFrame::Deny {
            id,
            status: 403,
            reason: "not_allowed".into(),
            hint: "1.1.1.1:443 is not in this Session's egress allowlist.".into(),
        });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 403 Forbidden"), "{response}");
    }

    #[test]
    fn the_sidecar_refuses_forbidden_addresses_itself() {
        let mut harness = start(|config| config.never = never::is_never);
        let mut client = harness.connect();
        client
            .write_all(b"CONNECT allowed.test:8080 HTTP/1.1\r\n\r\n")
            .unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["::ffff:127.0.0.1".parse().unwrap()],
        });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(response.contains("forbidden_destination"));
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                outcome,
                ip,
                ..
            } => {
                assert_eq!(
                    (closed, outcome, ip),
                    (id, CloseOutcome::UpstreamFailed, None)
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unreachable_upstream_gives_502() {
        let mut harness = start(|config| config.connect_timeout = Duration::from_millis(500));
        let closed_port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut client = harness.connect();
        write!(client, "CONNECT gone.test:{closed_port} HTTP/1.1\r\n\r\n").unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 502 "), "{response}");
        assert!(response.contains("upstream_unreachable"));
        assert!(matches!(
            harness.daemon.frame(),
            SidecarFrame::Close {
                outcome: CloseOutcome::UpstreamFailed,
                ..
            }
        ));
    }

    #[test]
    fn no_decision_in_time_gives_503() {
        let mut harness = start(|config| config.decision_timeout = Duration::from_millis(200));
        let mut client = harness.connect();
        client
            .write_all(b"CONNECT slow.test:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let (id, _, _, _) = harness.daemon.open();
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
        assert!(response.contains("decision_timeout"));
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                outcome,
                ..
            } => {
                assert_eq!((closed, outcome), (id, CloseOutcome::Interrupted));
            }
            other => panic!("{other:?}"),
        }
        // A late decision is ignored, a late relay too; data for that
        // connection is dropped rather than ending the proxy.
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        harness.daemon.send(DaemonFrame::Relay { id });
        harness.daemon.send_data(id, b"late");
        harness.daemon.send(DaemonFrame::Ping);
        assert_eq!(harness.daemon.frame(), SidecarFrame::Pong);
    }

    #[test]
    fn bad_heads_get_400_without_reaching_the_daemon() {
        let mut harness = start(|config| config.head_timeout = Duration::from_millis(300));
        for (request, reason) in [
            (
                &b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..],
                "not_a_proxy_request",
            ),
            (b"\x16\x03\x01garbage\r\n\r\n", "bad_request"),
            (
                b"GET https://a.test/ HTTP/1.1\r\nHost: a.test\r\n\r\n",
                "https_absolute_form",
            ),
            (b"CONNECT a.test HTTP/1.1\r\n\r\n", "bad_request"),
            (b"CONNECT a.test:443 HTTP/1.1\r\n", "head_timeout"),
        ] {
            let mut client = harness.connect();
            client.write_all(request).unwrap();
            let response = read_response(&mut client);
            assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
            assert!(response.contains(reason), "{reason}: {response}");
        }
        let mut client = harness.connect();
        let mut big = b"CONNECT a.test:443 HTTP/1.1\r\nX: ".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_HEAD_BYTES + 10));
        let _ = client.write_all(&big);
        let response = read_response(&mut client);
        assert!(response.contains("head_too_large"), "{response}");
        assert!(harness
            .daemon
            .frame_timeout(Duration::from_millis(200))
            .is_none());
    }

    #[test]
    fn revoke_closes_an_open_tunnel() {
        let mut harness = start(|_| {});
        let (port, upstream) = echo_server();
        let mut client = harness.connect();
        write!(client, "CONNECT a.test:{port} HTTP/1.1\r\n\r\n").unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        let mut established = [0u8; 39];
        client.read_exact(&mut established).unwrap();
        harness.daemon.send(DaemonFrame::Revoke { ids: vec![id] });
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                outcome,
                ..
            } => {
                assert_eq!((closed, outcome), (id, CloseOutcome::Revoked));
            }
            other => panic!("{other:?}"),
        }
        let mut rest = Vec::new();
        let _ = client.read_to_end(&mut rest);
        drop(client);
        upstream.join().unwrap();
    }

    /// The daemon may revoke a connection while its decision is still on the
    /// way (its credential ended mid-decision). The revoke is held until the
    /// allow arrives, and the proxy then refuses without connecting.
    #[test]
    fn a_revoke_that_overtakes_its_allow_refuses_without_connecting() {
        let mut harness = start(|_| {});
        // A revoke for a refused request is dropped with the refusal.
        let mut refused = harness.connect();
        write!(refused, "CONNECT b.test:443 HTTP/1.1\r\n\r\n").unwrap();
        let (refused_id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Revoke {
            ids: vec![refused_id],
        });
        harness.daemon.send(DaemonFrame::Deny {
            id: refused_id,
            status: 407,
            reason: "binding_ended".into(),
            hint: "ended".into(),
        });
        assert!(read_response(&mut refused).starts_with("HTTP/1.1 407"));

        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        upstream.set_nonblocking(true).unwrap();
        let port = upstream.local_addr().unwrap().port();
        let mut client = harness.connect();
        write!(client, "CONNECT a.test:{port} HTTP/1.1\r\n\r\nnever-sent").unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Revoke { ids: vec![id] });
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                outcome,
                ip,
                up,
                ..
            } => assert_eq!(
                (closed, outcome, ip, up),
                (id, CloseOutcome::Revoked, None, 0)
            ),
            other => panic!("{other:?}"),
        }
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("reason=revoked"), "{response}");
        assert_eq!(
            upstream.accept().map(|_| ()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        // The same holds for a relay: no 200, nothing carried.
        let mut client = harness.connect();
        write!(client, "CONNECT r.test:443 HTTP/1.1\r\n\r\nnever-sent").unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Revoke { ids: vec![id] });
        harness.daemon.send(DaemonFrame::Relay { id });
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                outcome,
                ip,
                up,
                ..
            } => assert_eq!(
                (closed, outcome, ip, up),
                (id, CloseOutcome::Revoked, None, 0)
            ),
            other => panic!("{other:?}"),
        }
        let response = read_response(&mut client);
        assert!(response.contains("reason=revoked"), "{response}");
    }

    #[test]
    fn end_of_control_input_stops_the_proxy_and_its_tunnels() {
        let mut harness = start(|_| {});
        let (port, upstream) = echo_server();
        let mut client = harness.connect();
        write!(client, "CONNECT a.test:{port} HTTP/1.1\r\n\r\n").unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
        let mut established = [0u8; 39];
        client.read_exact(&mut established).unwrap();
        harness.daemon.writer.shutdown(Shutdown::Write).unwrap();
        assert_eq!(harness.proxy.join().unwrap(), ProxyExit::ControlClosed);
        let mut rest = Vec::new();
        let _ = client.read_to_end(&mut rest);
        assert!(rest.is_empty());
        drop(client);
        upstream.join().unwrap();
        // New connections are no longer served.
        let mut late = harness.plain.connect();
        let _ = late.write_all(b"CONNECT a.test:443 HTTP/1.1\r\n\r\n");
        // macOS refuses a timeout once the other end is gone (EINVAL).
        let _ = late.set_read_timeout(Some(Duration::from_millis(500)));
        let mut buffer = [0u8; 16];
        assert!(!matches!(late.read(&mut buffer), Ok(n) if n > 0));
    }

    #[test]
    fn heartbeat_loss_and_malformed_frames_stop_the_proxy() {
        let harness = start(|config| config.heartbeat_dead = Duration::from_millis(300));
        assert_eq!(harness.proxy.join().unwrap(), ProxyExit::HeartbeatLost);

        let mut harness = start(|_| {});
        harness
            .daemon
            .writer
            .write_all(b"{\"t\":\"launch\"}\n")
            .unwrap();
        assert!(matches!(
            harness.proxy.join().unwrap(),
            ProxyExit::Protocol(_)
        ));
        let fatal = harness.daemon.frame();
        assert!(matches!(fatal, SidecarFrame::Fatal { .. }), "{fatal:?}");
    }

    #[test]
    fn pings_are_answered() {
        let mut harness = start(|_| {});
        harness.daemon.send(DaemonFrame::Ping);
        assert_eq!(harness.daemon.frame(), SidecarFrame::Pong);
    }

    #[test]
    fn stale_sockets_are_replaced_but_other_files_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("proxy.sock");
        drop(bind_unix_listener(&socket).unwrap());
        assert!(socket.exists());
        let again = bind_unix_listener(&socket).unwrap();
        drop(again);
        let file = dir.path().join("regular");
        std::fs::write(&file, b"keep").unwrap();
        assert!(bind_unix_listener(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    /// Open a relayed connection: the client sends CONNECT, the daemon
    /// answers `relay`, the client reads the 200.
    fn relayed(harness: &mut Harness, host: &str) -> (UnixStream, u64) {
        let mut client = harness.connect();
        write!(
            client,
            "CONNECT {host}:443 HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic("axe_relay")
        )
        .unwrap();
        let (id, opened, port, _) = harness.daemon.open();
        assert_eq!((opened.as_str(), port), (host, 443));
        harness.daemon.send(DaemonFrame::Relay { id });
        let mut established = [0u8; 39];
        client.read_exact(&mut established).unwrap();
        assert_eq!(&established, http::CONNECT_ESTABLISHED);
        (client, id)
    }

    fn payload(length: usize, seed: usize) -> Vec<u8> {
        (0..length)
            .map(|index| ((index * 7 + seed) % 251) as u8)
            .collect()
    }

    /// An echo on the daemon's side of one relayed connection, keeping both
    /// windows: it grants credit for what it takes and sends no more than
    /// the proxy has credited. Returns the close frame.
    fn echo_relay(daemon: &mut FakeDaemon, id: u64) -> SidecarFrame {
        let window = u64::from(RELAY_WINDOW_BYTES);
        let mut queued: VecDeque<u8> = VecDeque::new();
        let mut credit = window;
        let mut client_done = false;
        let mut sent_eof = false;
        loop {
            while credit > 0 && !queued.is_empty() {
                let take = (credit as usize).min(MAX_DATA_BYTES).min(queued.len());
                let chunk: Vec<u8> = queued.drain(..take).collect();
                daemon.send_data(id, &chunk);
                credit -= take as u64;
            }
            if client_done && queued.is_empty() && !sent_eof {
                daemon.send(DaemonFrame::Eof { id });
                sent_eof = true;
            }
            match daemon.frame() {
                SidecarFrame::Data { id: of, b } => {
                    assert_eq!(of, id);
                    let bytes = protocol::decode_data(&b).unwrap();
                    daemon.send(DaemonFrame::Credit {
                        id,
                        bytes: bytes.len() as u32,
                    });
                    queued.extend(bytes);
                }
                SidecarFrame::Credit { id: of, bytes } => {
                    assert_eq!(of, id);
                    credit += u64::from(bytes);
                    assert!(credit <= window, "the proxy granted past the window");
                }
                SidecarFrame::Eof { id: of } => {
                    assert_eq!(of, id);
                    client_done = true;
                }
                close @ SidecarFrame::Close { .. } => return close,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn a_relay_carries_a_mebibyte_each_way_intact() {
        let mut harness = start(|_| {});
        let (mut client, id) = relayed(&mut harness, "relay.test");
        let upload = payload(1024 * 1024, 3);
        let mut writer = client.try_clone().unwrap();
        let sent = upload.clone();
        let sender = std::thread::spawn(move || {
            writer.write_all(&sent).unwrap();
            writer.shutdown(Shutdown::Write).unwrap();
        });
        let reader = std::thread::spawn(move || {
            let mut received = Vec::new();
            client.read_to_end(&mut received).unwrap();
            received
        });
        let close = echo_relay(&mut harness.daemon, id);
        sender.join().unwrap();
        assert_eq!(reader.join().unwrap(), upload);
        match close {
            SidecarFrame::Close {
                id: closed,
                ip,
                up,
                down,
                outcome,
                ..
            } => assert_eq!(
                (closed, ip, up, down, outcome),
                (
                    id,
                    None,
                    upload.len() as u64,
                    upload.len() as u64,
                    CloseOutcome::Closed
                )
            ),
            other => panic!("{other:?}"),
        }
        // The relay is gone; a stream frame for it is dropped.
        harness.daemon.send_data(id, b"after the close");
        harness.daemon.send(DaemonFrame::Ping);
        assert_eq!(harness.daemon.frame(), SidecarFrame::Pong);
    }

    #[test]
    fn a_withheld_credit_stops_the_client_at_the_window() {
        let mut harness = start(|_| {});
        let (client, id) = relayed(&mut harness, "slow.test");
        let mut writer = client.try_clone().unwrap();
        let sender = std::thread::spawn(move || {
            let _ = writer.write_all(&payload(1024 * 1024, 5));
        });
        let window = RELAY_WINDOW_BYTES as usize;
        let first = harness.daemon.data(id, window);
        assert_eq!(first.len(), window, "never more than the window");
        assert_eq!(first, payload(1024 * 1024, 5)[..window]);
        // Nothing more without credit.
        assert!(harness
            .daemon
            .frame_timeout(Duration::from_millis(300))
            .is_none());
        harness
            .daemon
            .send(DaemonFrame::Credit { id, bytes: 65_536 });
        let more = harness.daemon.data(id, 65_536);
        assert_eq!(more.len(), 65_536);
        assert!(harness
            .daemon
            .frame_timeout(Duration::from_millis(300))
            .is_none());
        harness.daemon.send(DaemonFrame::Revoke { ids: vec![id] });
        match harness.daemon.frame_skipping_credit() {
            SidecarFrame::Close { outcome, up, .. } => {
                assert_eq!(
                    (outcome, up),
                    (CloseOutcome::Revoked, (window + 65_536) as u64)
                )
            }
            other => panic!("{other:?}"),
        }
        drop(client);
        sender.join().unwrap();
    }

    #[test]
    fn a_daemon_that_grants_past_the_window_resets_only_that_relay() {
        let mut harness = start(|_| {});
        let (mut client, id) = relayed(&mut harness, "overrun.test");
        // The proxy starts with the whole window; one more byte breaks it.
        harness.daemon.send(DaemonFrame::Credit { id, bytes: 1 });
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                outcome,
                error,
                ..
            } => {
                assert_eq!((closed, outcome), (id, CloseOutcome::Reset));
                assert!(error.unwrap().contains("window"));
            }
            other => panic!("{other:?}"),
        }
        let _ = client.read_to_end(&mut Vec::new());
        // Other connections go on.
        harness.daemon.send(DaemonFrame::Ping);
        assert_eq!(harness.daemon.frame(), SidecarFrame::Pong);
    }

    #[test]
    fn a_relay_half_closes_each_way() {
        let mut harness = start(|_| {});
        // The client finishes first and still reads the daemon's answer.
        let (mut client, id) = relayed(&mut harness, "first.test");
        client.write_all(b"request").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(harness.daemon.data(id, 7), b"request");
        assert_eq!(
            harness.daemon.frame_skipping_credit(),
            SidecarFrame::Eof { id }
        );
        harness.daemon.send_data(id, b"answer after your end");
        harness.daemon.send(DaemonFrame::Eof { id });
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"answer after your end");
        match harness.daemon.frame_skipping_credit() {
            SidecarFrame::Close {
                outcome, up, down, ..
            } => assert_eq!((outcome, up, down), (CloseOutcome::Closed, 7, 21)),
            other => panic!("{other:?}"),
        }

        // The daemon finishes first and still takes the client's bytes.
        let (mut client, id) = relayed(&mut harness, "second.test");
        harness.daemon.send_data(id, b"banner");
        harness.daemon.send(DaemonFrame::Eof { id });
        let mut banner = Vec::new();
        client.read_to_end(&mut banner).unwrap();
        assert_eq!(banner, b"banner");
        client.write_all(b"still sending").unwrap();
        assert_eq!(harness.daemon.data(id, 13), b"still sending");
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            harness.daemon.frame_skipping_credit(),
            SidecarFrame::Eof { id }
        );
        match harness.daemon.frame_skipping_credit() {
            SidecarFrame::Close {
                outcome, up, down, ..
            } => assert_eq!((outcome, up, down), (CloseOutcome::Closed, 13, 6)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn revoke_and_end_of_input_close_relays() {
        let mut harness = start(|_| {});
        let (mut client, id) = relayed(&mut harness, "revoked.test");
        client.write_all(b"x").unwrap();
        assert_eq!(harness.daemon.data(id, 1), b"x");
        harness.daemon.send(DaemonFrame::Revoke { ids: vec![id] });
        match harness.daemon.frame_skipping_credit() {
            SidecarFrame::Close {
                id: closed,
                ip,
                outcome,
                up,
                ..
            } => assert_eq!(
                (closed, ip, outcome, up),
                (id, None, CloseOutcome::Revoked, 1)
            ),
            other => panic!("{other:?}"),
        }
        let mut rest = Vec::new();
        let _ = client.read_to_end(&mut rest);
        assert!(rest.is_empty());

        let (mut client, _) = relayed(&mut harness, "interrupted.test");
        harness.daemon.writer.shutdown(Shutdown::Write).unwrap();
        assert_eq!(harness.proxy.join().unwrap(), ProxyExit::ControlClosed);
        // macOS refuses a timeout once the other end is gone (EINVAL).
        let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
        let mut rest = Vec::new();
        let ended = client.read_to_end(&mut rest);
        assert!(ended.is_ok(), "{ended:?}");
        assert!(rest.is_empty());
    }

    #[test]
    fn relays_past_the_limit_get_503() {
        let mut harness = start(|config| config.max_connections = MAX_RELAYS + 8);
        let mut open = Vec::new();
        for index in 0..MAX_RELAYS {
            open.push(relayed(&mut harness, &format!("r{index}.test")));
        }
        let mut client = harness.connect();
        client
            .write_all(b"CONNECT full.test:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Relay { id });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
        assert!(response.contains("relay_capacity"), "{response}");
        match harness.daemon.frame() {
            SidecarFrame::Close {
                id: closed,
                ip,
                outcome,
                ..
            } => assert_eq!(
                (closed, ip, outcome),
                (id, None, CloseOutcome::UpstreamFailed)
            ),
            other => panic!("{other:?}"),
        }
        drop(open);
    }

    #[test]
    fn a_relay_for_a_plain_http_request_gives_502() {
        let mut harness = start(|_| {});
        let mut client = harness.connect();
        client
            .write_all(b"GET http://plain.test/ HTTP/1.1\r\nHost: plain.test\r\n\r\n")
            .unwrap();
        let (id, _, _, _) = harness.daemon.open();
        harness.daemon.send(DaemonFrame::Relay { id });
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 502 "), "{response}");
        assert!(response.contains("relay_not_supported"), "{response}");
        assert!(matches!(
            harness.daemon.frame(),
            SidecarFrame::Close {
                ip: None,
                outcome: CloseOutcome::UpstreamFailed,
                ..
            }
        ));
    }

    #[test]
    fn a_stream_frame_for_a_connection_never_opened_stops_the_proxy() {
        let mut harness = start(|_| {});
        harness.daemon.send_data(99, b"who");
        assert!(matches!(
            harness.proxy.join().unwrap(),
            ProxyExit::Protocol(detail) if detail.contains("never opened")
        ));
    }

    fn identity() -> PeerIdentity {
        PeerIdentity {
            pid: Some(42),
            uid: Some(1000),
            gid: Some(1000),
            exe: Some("/usr/bin/curl".into()),
            exe_sha256: Some(protocol::credential_hash("curl")),
            ancestors: vec!["/bin/sh".into()],
            error: None,
        }
    }

    #[test]
    fn the_identity_line_is_required_on_the_identity_socket_and_refused_elsewhere() {
        let mut harness = start(|config| config.peer_timeout = Duration::from_millis(300));
        // With the line: the open carries the identity.
        let mut client = harness.identity.connect();
        let mut sent = identity().line().unwrap();
        sent.extend_from_slice(b"CONNECT id.test:443 HTTP/1.1\r\n\r\n");
        client.write_all(&sent).unwrap();
        match harness.daemon.frame() {
            SidecarFrame::Open { id, peer, host, .. } => {
                assert_eq!(host, "id.test");
                assert_eq!(peer, Some(identity()));
                harness.daemon.send(DaemonFrame::Deny {
                    id,
                    status: 403,
                    reason: "not_allowed".into(),
                    hint: "no".into(),
                });
            }
            other => panic!("{other:?}"),
        }
        assert!(read_response(&mut client).starts_with("HTTP/1.1 403"));

        // Without it, or with a bad one, nothing reaches the daemon.
        let oversized = format!(
            "AXO-PEER/1 {{\"exe\":\"/{}\"}}\r\n",
            "a".repeat(protocol::MAX_PEER_LINE_BYTES)
        );
        for (request, reason) in [
            (
                "CONNECT id.test:443 HTTP/1.1\r\n\r\n".to_string(),
                "identity_required",
            ),
            (String::new(), "identity_required"),
            (
                "AXO-PEER/1 {\"exe\":5}\r\nCONNECT id.test:443 HTTP/1.1\r\n\r\n".to_string(),
                "bad_identity",
            ),
            (
                "AXO-PEER/1 {\"exe\":\"/x\",\"extra\":1}\r\nCONNECT id.test:443 HTTP/1.1\r\n\r\n"
                    .to_string(),
                "bad_identity",
            ),
            (oversized, "bad_identity"),
            (
                format!(
                    "{}{}CONNECT id.test:443 HTTP/1.1\r\n\r\n",
                    String::from_utf8(identity().line().unwrap()).unwrap(),
                    String::from_utf8(identity().line().unwrap()).unwrap()
                ),
                "identity_not_accepted",
            ),
        ] {
            let mut client = harness.identity.connect();
            let _ = client.write_all(request.as_bytes());
            let response = read_response(&mut client);
            assert!(
                response.starts_with("HTTP/1.1 400 "),
                "{reason}: {response}"
            );
            assert!(response.contains(reason), "{reason}: {response}");
        }

        // A forged line on the ordinary socket is refused as such.
        let mut client = harness.connect();
        let mut forged = identity().line().unwrap();
        forged.extend_from_slice(b"CONNECT id.test:443 HTTP/1.1\r\n\r\n");
        client.write_all(&forged).unwrap();
        let response = read_response(&mut client);
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
        assert!(response.contains("identity_not_accepted"), "{response}");
        assert!(harness
            .daemon
            .frame_timeout(Duration::from_millis(300))
            .is_none());
    }
}
