//! The egress proxy (`--egress-proxy`): listens on a Unix socket, reports each
//! request to the daemon over the control channel, and connects only to the
//! addresses the daemon allows.
//!
//! It fails closed: it stops on end of the control input, a malformed daemon
//! frame, `shutdown`, or no daemon frame for `HEARTBEAT_DEAD_MS`, and a
//! request with no decision within `DECISION_TIMEOUT_MS` is answered 503.
//! The process exits when [`run`] returns, which closes every connection.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::http::{self, HeadError, ProxyRequest};
use super::never;
use super::protocol::{
    self, CloseOutcome, DaemonFrame, RequestKind, SidecarFrame, CONNECT_TIMEOUT_MS,
    DECISION_TIMEOUT_MS, EGRESS_PROTOCOL_VERSION, HEAD_TIMEOUT_MS, HEARTBEAT_DEAD_MS,
    IDLE_TIMEOUT_MS, MAX_HEAD_BYTES,
};
use super::pump::{self, pump};

/// How long to wait for `hello_ack` after `hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Most revoke ids remembered for connections not yet registered.
const MAX_EARLY_REVOKES: usize = 4096;

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

enum Verdict {
    Allow(Vec<IpAddr>),
    Deny {
        status: u16,
        reason: String,
        hint: String,
    },
}

struct Connection {
    fds: Vec<RawFd>,
    revoked: Arc<AtomicBool>,
}

struct Shared {
    config: ProxyConfig,
    out: Mutex<Box<dyn Write + Send>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Verdict>>>,
    connections: Mutex<HashMap<u64, Connection>>,
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
        let mut current = self
            .exit
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if current.is_none() {
            *current = Some(exit);
        }
        drop(current);
        self.stop.store(true, Ordering::Release);
        self.slot_freed.notify_all();
        // Fail closed: every open tunnel ends now, and waiting requests see
        // their decision channel close.
        let connections = self
            .connections
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for connection in connections.values() {
            connection.revoked.store(true, Ordering::Release);
            for fd in &connection.fds {
                pump::shutdown(*fd, libc::SHUT_RDWR);
            }
        }
        drop(connections);
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn send(&self, frame: &SidecarFrame) -> bool {
        let line = match protocol::encode_sidecar(frame) {
            Ok(line) => line,
            Err(error) => {
                self.stop(ProxyExit::Protocol(error));
                return false;
            }
        };
        let mut out = self.out.lock().unwrap_or_else(|poison| poison.into_inner());
        if out.write_all(&line).and_then(|()| out.flush()).is_err() {
            drop(out);
            self.stop(ProxyExit::ControlClosed);
            return false;
        }
        true
    }

    fn revoke(&self, ids: &[u64]) {
        let connections = self
            .connections
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut early = self
            .early_revokes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for id in ids {
            match connections.get(id) {
                Some(connection) => {
                    connection.revoked.store(true, Ordering::Release);
                    for fd in &connection.fds {
                        pump::shutdown(*fd, libc::SHUT_RDWR);
                    }
                }
                None if early.len() < MAX_EARLY_REVOKES => {
                    early.insert(*id);
                }
                None => {}
            }
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
        *shared
            .last_daemon_frame
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Instant::now();
        match frame {
            DaemonFrame::HelloAck { .. } => shared.acked.store(true, Ordering::Release),
            DaemonFrame::Allow { id, addrs } => {
                if let Some(waiter) = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id)
                {
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
                shared
                    .early_revokes
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id);
                if let Some(waiter) = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id)
                {
                    let _ = waiter.send(Verdict::Deny {
                        status,
                        reason,
                        hint,
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
        let last = *shared
            .last_daemon_frame
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
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

fn read_head(stream: &mut UnixStream, timeout: Duration) -> HeadRead {
    let deadline = Instant::now() + timeout;
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return HeadRead::Refused(
                "head_timeout",
                "The request head did not arrive within 10 seconds.",
            );
        }
        if stream.set_read_timeout(Some(remaining)).is_err() {
            return HeadRead::Gone;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return HeadRead::Gone,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return HeadRead::Refused(
                    "head_timeout",
                    "The request head did not arrive within 10 seconds.",
                )
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return HeadRead::Gone,
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

fn handle(shared: Arc<Shared>, mut client: UnixStream) {
    let started = Instant::now();
    let (buffer, end) = match read_head(&mut client, shared.config.head_timeout) {
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
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (waiter, verdict) = mpsc::channel();
    shared
        .pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(id, waiter);
    let sent = shared.send(&SidecarFrame::Open {
        id,
        kind: request.kind,
        host: request.host.clone(),
        port: request.port,
        auth: request.auth.clone(),
        method: request.method.clone(),
        path: request.path.clone(),
    });
    let unavailable = |client: &mut UnixStream, reason: &str, hint: &str| {
        respond(
            client,
            &http::refusal_response(503, reason, &request.host, request.port, hint),
        )
    };
    if !sent {
        return unavailable(
            &mut client,
            "egress_stopped",
            "Axocoatl's egress proxy is stopping; retry shortly.",
        );
    }
    let verdict = match verdict.recv_timeout(shared.config.decision_timeout) {
        Ok(verdict) => verdict,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            shared
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&id);
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
    let revoked_early = shared
        .early_revokes
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&id);
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
    let revoked_while_connecting;
    {
        let mut connections = shared
            .connections
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let early = shared
            .early_revokes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        revoked_while_connecting = early;
        if early || shared.stopped() {
            revoked.store(true, Ordering::Release);
        }
        connections.insert(
            id,
            Connection {
                fds: vec![client.as_raw_fd(), upstream.as_raw_fd()],
                revoked: revoked.clone(),
            },
        );
    }
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
    shared
        .connections
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&id);
    drop(upstream);
    drop(client);
    let outcome = if shared.stopped() && report.outcome != CloseOutcome::Closed {
        pump::PumpReport {
            outcome: CloseOutcome::Interrupted,
            ..report
        }
    } else {
        report
    };
    shared.send(&close_frame(id, Some(ip), started, outcome));
}

/// Run the proxy until it must stop. Sends `hello`, waits for `hello_ack`,
/// then serves `listener`. Never returns before the control channel or the
/// daemon tells it to stop.
pub fn run(
    config: ProxyConfig,
    listener: UnixListener,
    control_in: Box<dyn Read + Send>,
    control_out: Box<dyn Write + Send>,
) -> ProxyExit {
    let max_connections = config.max_connections.max(1);
    let shared = Arc::new(Shared {
        config,
        out: Mutex::new(control_out),
        pending: Mutex::new(HashMap::new()),
        connections: Mutex::new(HashMap::new()),
        early_revokes: Mutex::new(HashSet::new()),
        stop: AtomicBool::new(false),
        exit: Mutex::new(None),
        active: Mutex::new(0),
        slot_freed: Condvar::new(),
        next_id: AtomicU64::new(1),
        last_daemon_frame: Mutex::new(Instant::now()),
        acked: AtomicBool::new(false),
    });
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
    if let Err(error) = listener.set_nonblocking(true) {
        shared.stop(ProxyExit::Protocol(format!("listener: {error}")));
    }
    while !shared.stopped() {
        {
            let mut active = shared
                .active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
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
        let mut entry = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd.
        unsafe { libc::poll(&mut entry, 1, 100) };
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                *shared
                    .active
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) += 1;
                let connection_shared = shared.clone();
                std::thread::spawn(move || {
                    handle(connection_shared.clone(), stream);
                    let mut active = connection_shared
                        .active
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    *active -= 1;
                    connection_shared.slot_freed.notify_one();
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => shared.stop(ProxyExit::Protocol(format!("accept: {error}"))),
        }
    }
    let exit = shared
        .exit
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap_or(ProxyExit::ControlClosed);
    if let ProxyExit::Protocol(detail) = &exit {
        let mut out = shared
            .out
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Ok(line) = protocol::encode_sidecar(&SidecarFrame::Fatal {
            detail: detail.chars().take(protocol::MAX_DETAIL_CHARS).collect(),
        }) {
            let _ = out.write_all(&line).and_then(|()| out.flush());
        }
    }
    exit
}

/// `--egress-proxy --socket <path> [--max-connections N]` with the process's
/// stdin and stdout as the control channel. Returns the exit status.
pub fn main(socket: PathBuf, max_connections: usize) -> i32 {
    connectable_sockets_by_default();
    let listener = match bind_unix_listener(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "egress proxy: cannot listen on {}: {error}",
                socket.display()
            );
            return 1;
        }
    };
    let exit = run(
        ProxyConfig::new(max_connections),
        listener,
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

        fn send(&mut self, frame: DaemonFrame) {
            self.writer
                .write_all(&protocol::encode_daemon(&frame).unwrap())
                .unwrap();
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
    }

    struct Harness {
        daemon: FakeDaemon,
        socket: PathBuf,
        proxy: std::thread::JoinHandle<ProxyExit>,
        _dir: tempfile::TempDir,
    }

    fn allow_loopback(_: IpAddr) -> bool {
        false
    }

    fn start(configure: impl FnOnce(&mut ProxyConfig)) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("proxy.sock");
        // The socket's mode comes from the umask; the binary sets it (see
        // tests/egress_binary.rs), the test harness does not.
        let listener = bind_unix_listener(&socket).unwrap();
        let (daemon_end, proxy_end) = UnixStream::pair().unwrap();
        let mut config = ProxyConfig::new(8);
        config.never = allow_loopback;
        configure(&mut config);
        let proxy_in = proxy_end.try_clone().unwrap();
        let proxy = std::thread::spawn(move || {
            run(config, listener, Box::new(proxy_in), Box::new(proxy_end))
        });
        let mut daemon = FakeDaemon {
            reader: BufReader::new(daemon_end.try_clone().unwrap()),
            writer: daemon_end,
        };
        match daemon.frame() {
            SidecarFrame::Hello {
                protocol,
                max_connections,
                ..
            } => assert_eq!((protocol, max_connections), (1, 8)),
            other => panic!("expected hello, got {other:?}"),
        }
        daemon.send(DaemonFrame::HelloAck { protocol: 1 });
        Harness {
            daemon,
            socket,
            proxy,
            _dir: dir,
        }
    }

    fn basic(token: &str) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = format!("axo:{token}").into_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let value = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..4 {
                out.push(if index <= chunk.len() {
                    TABLE[((value >> (18 - index * 6)) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
            ..
        } = open
        else {
            panic!("{open:?}")
        };
        assert_eq!(kind, RequestKind::Http);
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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

        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        // A late decision is ignored.
        harness.daemon.send(DaemonFrame::Allow {
            id,
            addrs: vec!["127.0.0.1".parse().unwrap()],
        });
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
            let mut client = UnixStream::connect(&harness.socket).unwrap();
            client.write_all(request).unwrap();
            let response = read_response(&mut client);
            assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
            assert!(response.contains(reason), "{reason}: {response}");
        }
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        let mut refused = UnixStream::connect(&harness.socket).unwrap();
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
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
    }

    #[test]
    fn end_of_control_input_stops_the_proxy_and_its_tunnels() {
        let mut harness = start(|_| {});
        let (port, upstream) = echo_server();
        let mut client = UnixStream::connect(&harness.socket).unwrap();
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
        if let Ok(mut late) = UnixStream::connect(&harness.socket) {
            let _ = late.write_all(b"CONNECT a.test:443 HTTP/1.1\r\n\r\n");
            late.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut buffer = [0u8; 16];
            assert!(!matches!(late.read(&mut buffer), Ok(n) if n > 0));
        }
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
}
