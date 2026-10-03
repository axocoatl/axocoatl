//! The daemon's side of one egress sidecar's control channel.
//!
//! [`start`] waits for the sidecar's `hello`, answers `hello_ack`, then serves
//! the channel: each `open` is decided concurrently by the
//! [`EgressAuthority`], each `close` is recorded, and a `ping` goes out every
//! 5 s. When the channel ends, every connection still open is recorded as
//! `interrupted`, and the authority hears `channel_lost` (or `stopped` after a
//! requested shutdown).
//!
//! A connection the authority answers with [`Decision::Relay`] gets a
//! [`RelayStream`] that [`EgressAuthority::relay`] serves in its own task;
//! its `data`, `eof` and `credit` frames are routed here. The writer sends
//! control frames (decisions, credit, revokes, pings) before queued `data`
//! and `eof` frames, so a busy relay never delays a decision.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axocoatl_exec::egress::protocol::{
    self, CloseOutcome, DaemonFrame, RequestKind, SidecarFrame, EGRESS_PROTOCOL_VERSION,
    HEARTBEAT_DEAD_MS, HEARTBEAT_MS, MAX_FRAME_BYTES, MAX_REVOKE_IDS,
};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;

use crate::egress::{
    CloseReport, Decision, EgressAuthority, OpenRequest, RelayEnd, RelayOpen, RelayShared,
    RelayStream, SidecarEvent,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Most decisions in flight at once; further `open` frames wait.
pub const MAX_CONCURRENT_DECISIONS: u32 = 512;

/// Timings, adjustable for tests.
#[derive(Debug, Clone, Copy)]
pub struct ControlTiming {
    pub hello: Duration,
    pub heartbeat: Duration,
    pub heartbeat_dead: Duration,
}

impl Default for ControlTiming {
    fn default() -> Self {
        Self {
            hello: Duration::from_secs(10),
            heartbeat: Duration::from_millis(HEARTBEAT_MS),
            heartbeat_dead: Duration::from_millis(HEARTBEAT_DEAD_MS),
        }
    }
}

/// Frames for the sidecar: control frames and relay stream frames wait in
/// separate queues, and the writer empties the control queue first.
#[derive(Clone, Debug)]
pub(crate) struct Outgoing {
    control: mpsc::UnboundedSender<DaemonFrame>,
    stream: mpsc::UnboundedSender<DaemonFrame>,
}

impl Outgoing {
    /// Queue a frame. False once the channel is gone.
    pub(crate) fn send(&self, frame: DaemonFrame) -> bool {
        if frame.is_stream() {
            self.stream.send(frame).is_ok()
        } else {
            self.control.send(frame).is_ok()
        }
    }

    fn is_closed(&self) -> bool {
        self.control.is_closed()
    }
}

/// Send side of a running control channel.
#[derive(Clone, Debug)]
pub struct ControlHandle {
    generation: u32,
    frames: Outgoing,
    shutdown: Arc<AtomicBool>,
}

impl ControlHandle {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Close these connections now. Returns false once the channel is gone.
    pub fn revoke(&self, ids: Vec<u64>) -> bool {
        ids.chunks(MAX_REVOKE_IDS).all(|chunk| {
            self.frames.send(DaemonFrame::Revoke {
                ids: chunk.to_vec(),
            })
        })
    }

    /// Ask the sidecar to exit.
    pub fn shutdown(&self) -> bool {
        self.shutdown.store(true, Ordering::Release);
        self.frames.send(DaemonFrame::Shutdown)
    }

    pub fn is_open(&self) -> bool {
        !self.frames.is_closed()
    }
}

/// Why a control channel ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEnd {
    /// After [`ControlHandle::shutdown`].
    Shutdown,
    /// End of input, a write failure, a bad frame or a lost heartbeat.
    ChannelLost(String),
}

async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|error| error.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_FRAME_BYTES {
            "egress control frame exceeds 64 KiB".into()
        } else {
            "egress control channel ended inside a frame".into()
        });
    }
    line.pop();
    Ok(Some(line))
}

fn to_frame(id: u64, decision: Decision) -> DaemonFrame {
    let frame = match decision {
        Decision::Allow { addrs } => DaemonFrame::Allow { id, addrs },
        Decision::Deny {
            status,
            reason,
            hint,
        } => DaemonFrame::Deny {
            id,
            status,
            reason,
            hint,
        },
        Decision::Relay => DaemonFrame::Relay { id },
    };
    if frame.validate().is_ok() {
        frame
    } else {
        DaemonFrame::Deny {
            id,
            status: 503,
            reason: "internal_error".into(),
            hint: "Axocoatl could not decide on this connection; retry shortly.".into(),
        }
    }
}

/// Wait for `hello`, answer `hello_ack`, and serve the channel in a task.
pub async fn start<R, W>(
    generation: u32,
    reader: R,
    mut writer: W,
    authority: Arc<dyn EgressAuthority>,
    timing: ControlTiming,
) -> Result<(ControlHandle, JoinHandle<ControlEnd>), String>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let hello = tokio::time::timeout(timing.hello, read_line(&mut reader))
        .await
        .map_err(|_| "the egress proxy did not say hello within 10 seconds".to_string())??
        .ok_or("the egress proxy exited before saying hello")?;
    match protocol::decode_sidecar(&hello)? {
        SidecarFrame::Hello { protocol, .. } if protocol == EGRESS_PROTOCOL_VERSION => {}
        other => return Err(format!("the egress proxy sent {other:?} instead of hello")),
    }
    let ack = protocol::encode_daemon(&DaemonFrame::HelloAck {
        protocol: EGRESS_PROTOCOL_VERSION,
    })?;
    writer
        .write_all(&ack)
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())?;

    let (control, mut control_queue) = mpsc::unbounded_channel::<DaemonFrame>();
    let (stream, mut stream_queue) = mpsc::unbounded_channel::<DaemonFrame>();
    let frames = Outgoing { control, stream };
    let write_failed = Arc::new(AtomicBool::new(false));
    let writer_failed = write_failed.clone();
    tokio::spawn(async move {
        loop {
            // Control frames first; stream frames only when none waits.
            let frame = tokio::select! {
                biased;
                frame = control_queue.recv() => match frame {
                    Some(frame) => frame,
                    None => return,
                },
                Some(frame) = stream_queue.recv() => frame,
            };
            let Ok(line) = protocol::encode_daemon(&frame) else {
                continue;
            };
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                writer_failed.store(true, Ordering::Release);
                return;
            }
        }
    });
    let handle = ControlHandle {
        generation,
        frames: frames.clone(),
        shutdown: Arc::new(AtomicBool::new(false)),
    };
    let shutdown = handle.shutdown.clone();
    let task = tokio::spawn(serve(
        generation,
        reader,
        frames,
        authority,
        timing,
        shutdown,
        write_failed,
    ));
    Ok((handle, task))
}

/// Connections the channel knows about, shared with the decision tasks.
#[derive(Default)]
struct Connections {
    /// Allowed or relayed and not yet closed.
    open: HashSet<u64>,
    /// Being decided.
    deciding: HashSet<u64>,
    /// Closed by the sidecar while being decided (it gave up waiting).
    closed_early: HashSet<u64>,
    relays: HashMap<u64, Arc<RelayShared>>,
}

async fn serve<R: AsyncRead + Unpin + Send + 'static>(
    generation: u32,
    mut reader: BufReader<R>,
    frames: Outgoing,
    authority: Arc<dyn EgressAuthority>,
    timing: ControlTiming,
    shutdown: Arc<AtomicBool>,
    write_failed: Arc<AtomicBool>,
) -> ControlEnd {
    let connections: Arc<Mutex<Connections>> = Arc::new(Mutex::new(Connections::default()));
    // Highest connection id the sidecar has opened; stream frames for a
    // higher one are a protocol error.
    let highest = Arc::new(AtomicU64::new(0));
    let decisions = Arc::new(Semaphore::new(MAX_CONCURRENT_DECISIONS as usize));
    let mut ping = tokio::time::interval(timing.heartbeat);
    ping.tick().await;
    let mut last_frame = Instant::now();
    let relay_for = |id: u64| -> Result<Option<Arc<RelayShared>>, ControlEnd> {
        if id == 0 || id > highest.load(Ordering::Acquire) {
            return Err(ControlEnd::ChannelLost(format!(
                "the egress proxy sent relay bytes for connection {id}, which it never opened"
            )));
        }
        Ok(lock(&connections).relays.get(&id).cloned())
    };
    let end = loop {
        tokio::select! {
            line = read_line(&mut reader) => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break if shutdown.load(Ordering::Acquire) {
                        ControlEnd::Shutdown
                    } else {
                        ControlEnd::ChannelLost("the egress proxy's control channel closed".into())
                    },
                    Err(error) => break ControlEnd::ChannelLost(error),
                };
                last_frame = Instant::now();
                let frame = match protocol::decode_sidecar(&line) {
                    Ok(frame) => frame,
                    Err(error) => break ControlEnd::ChannelLost(format!("malformed frame from the egress proxy: {error}")),
                };
                match frame {
                    SidecarFrame::Open { id, kind, host, port, auth, method, path, peer } => {
                        highest.fetch_max(id, Ordering::AcqRel);
                        let Ok(permit) = decisions.clone().acquire_owned().await else {
                            break ControlEnd::ChannelLost("decision capacity closed".into());
                        };
                        lock(&connections).deciding.insert(id);
                        let authority = authority.clone();
                        let frames = frames.clone();
                        let connections = connections.clone();
                        tokio::spawn(async move {
                            let open = OpenRequest { generation, id, kind, host, port, auth, method, path, peer };
                            let decision = authority.decide(open.clone()).await;
                            let decision = match decision {
                                Decision::Relay if kind != RequestKind::Connect => Decision::deny(
                                    502,
                                    "relay_not_supported",
                                    "Axocoatl can only relay CONNECT requests.",
                                ),
                                other => other,
                            };
                            // Decided under the lock, acted on after it.
                            let relay = {
                                let mut known = lock(&connections);
                                known.deciding.remove(&id);
                                let gave_up = known.closed_early.remove(&id);
                                match decision {
                                    Decision::Relay if !gave_up => {
                                        let shared = RelayShared::new();
                                        known.relays.insert(id, shared.clone());
                                        known.open.insert(id);
                                        // Queued before the stream exists, so
                                        // the sidecar has the relay before any
                                        // data.
                                        frames.send(DaemonFrame::Relay { id });
                                        Some(shared)
                                    }
                                    Decision::Relay => None,
                                    decision => {
                                        if matches!(decision, Decision::Allow { .. }) && !gave_up {
                                            known.open.insert(id);
                                        }
                                        frames.send(to_frame(id, decision));
                                        None
                                    }
                                }
                            };
                            drop(permit);
                            if let Some(shared) = relay {
                                let stream = RelayStream::new(id, shared, frames);
                                authority.relay(RelayOpen { generation, id, open }, stream).await;
                            }
                        });
                    }
                    SidecarFrame::Close { id, ip, up, down, ms, outcome, error } => {
                        let relay = {
                            let mut known = lock(&connections);
                            known.open.remove(&id);
                            if known.deciding.contains(&id) {
                                known.closed_early.insert(id);
                            }
                            known.relays.remove(&id)
                        };
                        if let Some(relay) = relay {
                            relay.end(RelayEnd::Closed);
                        }
                        authority
                            .closed(CloseReport { generation, id, ip, up, down, ms, outcome, error })
                            .await;
                    }
                    SidecarFrame::Data { id, b } => {
                        let relay = match relay_for(id) {
                            Ok(relay) => relay,
                            Err(end) => break end,
                        };
                        let Some(relay) = relay else { continue };
                        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&b) else {
                            break ControlEnd::ChannelLost("relay data from the egress proxy is not base64".into());
                        };
                        match relay.received(bytes) {
                            Ok(Some(discarded)) => {
                                frames.send(DaemonFrame::Credit { id, bytes: discarded });
                            }
                            Ok(None) => {}
                            Err(error) => break ControlEnd::ChannelLost(error),
                        }
                    }
                    SidecarFrame::Eof { id } => {
                        match relay_for(id) {
                            Ok(Some(relay)) => relay.received_eof(),
                            Ok(None) => {}
                            Err(end) => break end,
                        }
                    }
                    SidecarFrame::Credit { id, bytes } => {
                        match relay_for(id) {
                            Ok(Some(relay)) => {
                                if let Err(error) = relay.credited(bytes) {
                                    break ControlEnd::ChannelLost(error);
                                }
                            }
                            Ok(None) => {}
                            Err(end) => break end,
                        }
                    }
                    SidecarFrame::Pong => {}
                    SidecarFrame::Fatal { detail } => {
                        break ControlEnd::ChannelLost(format!("the egress proxy failed: {detail}"));
                    }
                    SidecarFrame::Hello { .. } => {
                        break ControlEnd::ChannelLost("the egress proxy said hello twice".into());
                    }
                }
            }
            _ = ping.tick() => {
                if write_failed.load(Ordering::Acquire) {
                    break ControlEnd::ChannelLost("writing to the egress proxy failed".into());
                }
                if last_frame.elapsed() > timing.heartbeat_dead {
                    break ControlEnd::ChannelLost(format!(
                        "no frame from the egress proxy for {} s",
                        timing.heartbeat_dead.as_secs()
                    ));
                }
                let _ = frames.send(DaemonFrame::Ping);
            }
        }
    };
    // Every relay ends with the channel.
    let relays: Vec<Arc<RelayShared>> = lock(&connections)
        .relays
        .drain()
        .map(|(_, relay)| relay)
        .collect();
    for relay in relays {
        relay.end(RelayEnd::ChannelLost);
    }
    // Let in-flight decisions finish so every allowed connection is known.
    let _all = decisions.acquire_many(MAX_CONCURRENT_DECISIONS).await.ok();
    let mut remaining: Vec<u64> = {
        let mut known = lock(&connections);
        for relay in known.relays.drain().map(|(_, relay)| relay) {
            relay.end(RelayEnd::ChannelLost);
        }
        known.open.drain().collect()
    };
    remaining.sort_unstable();
    for id in remaining {
        authority
            .closed(CloseReport {
                generation,
                id,
                ip: None,
                up: 0,
                down: 0,
                ms: 0,
                outcome: CloseOutcome::Interrupted,
                error: Some("the egress control channel ended".into()),
            })
            .await;
    }
    authority
        .sidecar_event(match &end {
            ControlEnd::Shutdown => SidecarEvent::Stopped { generation },
            ControlEnd::ChannelLost(detail) => SidecarEvent::ChannelLost {
                generation,
                detail: detail.clone(),
            },
        })
        .await;
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{EgressGrant, GrantSpec};
    use axocoatl_exec::egress::protocol::RequestKind;
    use tokio::io::{duplex, AsyncBufReadExt, DuplexStream};

    #[derive(Debug, Default)]
    struct Recorder {
        decided: Mutex<Vec<OpenRequest>>,
        closed: Mutex<Vec<CloseReport>>,
        events: Mutex<Vec<SidecarEvent>>,
        delay: Option<Duration>,
    }

    #[async_trait::async_trait]
    impl EgressAuthority for Recorder {
        async fn grant(&self, _: GrantSpec) -> Result<EgressGrant, String> {
            Err("not used".into())
        }

        async fn decide(&self, open: OpenRequest) -> Decision {
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            self.decided.lock().unwrap().push(open.clone());
            if open.host == "allowed.test" {
                Decision::Allow {
                    addrs: vec!["203.0.113.5".parse().unwrap()],
                }
            } else if open.host == "relay.test" {
                Decision::Relay
            } else if open.host == "broken.test" {
                Decision::Deny {
                    status: 200,
                    reason: "Bad Reason".into(),
                    hint: String::new(),
                }
            } else {
                Decision::deny(403, "not_allowed", format!("{} is not allowed", open.host))
            }
        }

        async fn closed(&self, report: CloseReport) {
            self.closed.lock().unwrap().push(report);
        }

        async fn sidecar_event(&self, event: SidecarEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    struct Sidecar {
        to_daemon: DuplexStream,
        from_daemon: tokio::io::BufReader<DuplexStream>,
    }

    impl Sidecar {
        async fn send(&mut self, frame: SidecarFrame) {
            self.to_daemon
                .write_all(&protocol::encode_sidecar(&frame).unwrap())
                .await
                .unwrap();
        }

        async fn frame(&mut self) -> DaemonFrame {
            let mut line = Vec::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                self.from_daemon.read_until(b'\n', &mut line),
            )
            .await
            .expect("daemon frame")
            .unwrap();
            protocol::decode_daemon(&line).unwrap()
        }

        async fn frame_skipping_pings(&mut self) -> DaemonFrame {
            loop {
                let frame = self.frame().await;
                if frame != DaemonFrame::Ping {
                    return frame;
                }
            }
        }
    }

    fn timing() -> ControlTiming {
        ControlTiming {
            hello: Duration::from_millis(500),
            heartbeat: Duration::from_millis(50),
            heartbeat_dead: Duration::from_secs(5),
        }
    }

    async fn connect(
        authority: Arc<Recorder>,
        timing: ControlTiming,
    ) -> (Sidecar, ControlHandle, JoinHandle<ControlEnd>) {
        connect_with(authority, timing, 1 << 20).await
    }

    /// `buffer` bounds the daemon-to-sidecar pipe, so a test can hold the
    /// daemon's writer back by not reading.
    async fn connect_with(
        authority: Arc<dyn EgressAuthority>,
        timing: ControlTiming,
        buffer: usize,
    ) -> (Sidecar, ControlHandle, JoinHandle<ControlEnd>) {
        let (daemon_read, sidecar_write) = duplex(1 << 20);
        let (sidecar_read, daemon_write) = duplex(buffer);
        let mut sidecar = Sidecar {
            to_daemon: sidecar_write,
            from_daemon: tokio::io::BufReader::new(sidecar_read),
        };
        sidecar
            .send(SidecarFrame::Hello {
                protocol: 2,
                version: "test".into(),
                max_connections: 8,
            })
            .await;
        let (handle, task) = start(7, daemon_read, daemon_write, authority, timing)
            .await
            .unwrap();
        assert_eq!(sidecar.frame().await, DaemonFrame::HelloAck { protocol: 2 });
        (sidecar, handle, task)
    }

    fn open(id: u64, host: &str) -> SidecarFrame {
        SidecarFrame::Open {
            id,
            kind: RequestKind::Connect,
            host: host.into(),
            port: 443,
            auth: Some(protocol::credential_hash("axe_x")),
            method: None,
            path: None,
            peer: None,
        }
    }

    #[tokio::test]
    async fn decisions_closes_and_interrupted_connections() {
        let authority = Arc::new(Recorder::default());
        let (mut sidecar, handle, task) = connect(authority.clone(), timing()).await;
        assert_eq!(handle.generation(), 7);
        sidecar.send(open(1, "allowed.test")).await;
        assert_eq!(
            sidecar.frame_skipping_pings().await,
            DaemonFrame::Allow {
                id: 1,
                addrs: vec!["203.0.113.5".parse().unwrap()]
            }
        );
        sidecar.send(open(2, "denied.test")).await;
        match sidecar.frame_skipping_pings().await {
            DaemonFrame::Deny {
                id, status, reason, ..
            } => {
                assert_eq!((id, status, reason.as_str()), (2, 403, "not_allowed"))
            }
            other => panic!("{other:?}"),
        }
        // An invalid decision becomes a 503, never an invalid frame.
        sidecar.send(open(3, "broken.test")).await;
        match sidecar.frame_skipping_pings().await {
            DaemonFrame::Deny { status, reason, .. } => {
                assert_eq!((status, reason.as_str()), (503, "internal_error"))
            }
            other => panic!("{other:?}"),
        }
        sidecar.send(open(4, "allowed.test")).await;
        assert!(matches!(
            sidecar.frame_skipping_pings().await,
            DaemonFrame::Allow { id: 4, .. }
        ));
        sidecar
            .send(SidecarFrame::Close {
                id: 1,
                ip: Some("203.0.113.5".parse().unwrap()),
                up: 10,
                down: 20,
                ms: 5,
                outcome: CloseOutcome::Closed,
                error: None,
            })
            .await;
        assert!(handle.revoke(vec![4]));
        assert_eq!(
            sidecar.frame_skipping_pings().await,
            DaemonFrame::Revoke { ids: vec![4] }
        );
        // The sidecar disappears with connection 4 still open.
        drop(sidecar);
        let end = task.await.unwrap();
        assert!(matches!(end, ControlEnd::ChannelLost(_)), "{end:?}");
        let closed = authority.closed.lock().unwrap().clone();
        assert_eq!(closed.len(), 2);
        assert_eq!(
            (closed[0].id, closed[0].up, closed[0].outcome),
            (1, 10, CloseOutcome::Closed)
        );
        assert_eq!(
            (closed[1].id, closed[1].outcome),
            (4, CloseOutcome::Interrupted)
        );
        assert_eq!(closed[1].generation, 7);
        assert!(matches!(
            authority.events.lock().unwrap().last(),
            Some(SidecarEvent::ChannelLost { generation: 7, .. })
        ));
        let decided = authority.decided.lock().unwrap();
        assert_eq!(decided.len(), 4);
        assert_eq!(decided[0].auth, Some(protocol::credential_hash("axe_x")));
    }

    #[tokio::test]
    async fn pings_flow_and_a_silent_sidecar_is_lost() {
        let authority = Arc::new(Recorder::default());
        let (mut sidecar, _handle, task) = connect(
            authority.clone(),
            ControlTiming {
                heartbeat_dead: Duration::from_millis(300),
                ..timing()
            },
        )
        .await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Ping);
        let end = task.await.unwrap();
        assert!(
            matches!(&end, ControlEnd::ChannelLost(detail) if detail.contains("no frame")),
            "{end:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_ends_as_stopped() {
        let authority = Arc::new(Recorder::default());
        let (mut sidecar, handle, task) = connect(authority.clone(), timing()).await;
        assert!(handle.shutdown());
        assert_eq!(sidecar.frame_skipping_pings().await, DaemonFrame::Shutdown);
        drop(sidecar);
        assert_eq!(task.await.unwrap(), ControlEnd::Shutdown);
        assert_eq!(
            authority.events.lock().unwrap().last(),
            Some(&SidecarEvent::Stopped { generation: 7 })
        );
    }

    #[tokio::test]
    async fn bad_frames_end_the_channel() {
        for bad in [
            &b"{\"t\":\"exec\",\"argv\":[\"sh\"]}\n"[..],
            b"{\"t\":\"hello\",\"protocol\":2,\"version\":\"x\",\"max_connections\":8}\n",
            b"{\"t\":\"fatal\",\"detail\":\"listener failed\"}\n",
            b"{\"t\":\"open\",\"id\":1,\"kind\":\"connect\",\"host\":\"a.test\",\"port\":443,\"auth\":\"axe_rawtoken\"}\n",
        ] {
            let authority = Arc::new(Recorder::default());
            let (mut sidecar, _handle, task) = connect(authority.clone(), timing()).await;
            sidecar.to_daemon.write_all(bad).await.unwrap();
            let end = task.await.unwrap();
            assert!(matches!(end, ControlEnd::ChannelLost(_)), "{end:?}");
            assert!(authority.decided.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_missing_or_wrong_hello_is_refused() {
        let (daemon_read, _sidecar_write) = duplex(1024);
        let (_sidecar_read, daemon_write) = duplex(1024);
        let error = start(
            1,
            daemon_read,
            daemon_write,
            Arc::new(Recorder::default()),
            ControlTiming {
                hello: Duration::from_millis(100),
                ..timing()
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("did not say hello"), "{error}");

        let (daemon_read, mut sidecar_write) = duplex(1024);
        let (_sidecar_read, daemon_write) = duplex(1024);
        sidecar_write
            .write_all(&protocol::encode_sidecar(&SidecarFrame::Pong).unwrap())
            .await
            .unwrap();
        let error = start(
            1,
            daemon_read,
            daemon_write,
            Arc::new(Recorder::default()),
            timing(),
        )
        .await
        .unwrap_err();
        assert!(error.contains("instead of hello"), "{error}");
    }

    #[tokio::test]
    async fn decisions_in_flight_finish_before_interrupted_closes() {
        let authority = Arc::new(Recorder {
            delay: Some(Duration::from_millis(200)),
            ..Recorder::default()
        });
        let (mut sidecar, _handle, task) = connect(authority.clone(), timing()).await;
        sidecar.send(open(9, "allowed.test")).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(sidecar);
        task.await.unwrap();
        let closed = authority.closed.lock().unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(
            (closed[0].id, closed[0].outcome),
            (9, CloseOutcome::Interrupted)
        );
    }

    use crate::egress::{PeerIdentity, RelayOpen, RelayStream};
    use axocoatl_exec::egress::protocol::{MAX_DATA_BYTES, RELAY_WINDOW_BYTES};

    /// What [`RelayAuthority`] does with a relayed connection.
    #[derive(Debug, Clone, Copy)]
    enum Serve {
        /// Echo the client's bytes, then end.
        Echo,
        /// Send this many bytes, then end.
        Flood(usize),
        /// Read until the stream ends or fails.
        Hold,
    }

    /// Relays every `connect` and serves it as told, recording how it ended.
    #[derive(Debug)]
    struct RelayAuthority {
        serve: Serve,
        opened: Mutex<Vec<RelayOpen>>,
        closed: Mutex<Vec<CloseReport>>,
        ended: Mutex<Vec<Result<u64, std::io::ErrorKind>>>,
    }

    impl RelayAuthority {
        fn new(serve: Serve) -> Arc<Self> {
            Arc::new(Self {
                serve,
                opened: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
                ended: Mutex::new(Vec::new()),
            })
        }

        async fn ended(&self) -> Result<u64, std::io::ErrorKind> {
            for _ in 0..500 {
                if let Some(result) = self.ended.lock().unwrap().first() {
                    return *result;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the relay never ended");
        }
    }

    #[async_trait::async_trait]
    impl EgressAuthority for RelayAuthority {
        async fn grant(&self, _: GrantSpec) -> Result<EgressGrant, String> {
            Err("not used".into())
        }

        async fn decide(&self, _: OpenRequest) -> Decision {
            Decision::Relay
        }

        async fn closed(&self, report: CloseReport) {
            self.closed.lock().unwrap().push(report);
        }

        async fn sidecar_event(&self, _: SidecarEvent) {}

        async fn relay(&self, open: RelayOpen, stream: RelayStream) {
            assert_eq!(stream.id(), open.id);
            self.opened.lock().unwrap().push(open);
            let result = match self.serve {
                Serve::Echo => {
                    let (mut reader, mut writer) = tokio::io::split(stream);
                    let copied = tokio::io::copy(&mut reader, &mut writer).await;
                    let _ = writer.shutdown().await;
                    copied.map_err(|error| error.kind())
                }
                Serve::Flood(count) => {
                    let mut stream = stream;
                    let bytes = pattern(count);
                    match stream.write_all(&bytes).await {
                        Ok(()) => stream
                            .shutdown()
                            .await
                            .map(|()| count as u64)
                            .map_err(|error| error.kind()),
                        Err(error) => Err(error.kind()),
                    }
                }
                Serve::Hold => {
                    let mut stream = stream;
                    let mut sink = Vec::new();
                    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut sink)
                        .await
                        .map(|count| count as u64)
                        .map_err(|error| error.kind())
                }
            };
            self.ended.lock().unwrap().push(result);
        }
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|index| (index % 249) as u8).collect()
    }

    fn data(id: u64, bytes: &[u8]) -> SidecarFrame {
        SidecarFrame::Data {
            id,
            b: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    fn decoded(b: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(b).unwrap()
    }

    fn quiet() -> ControlTiming {
        ControlTiming {
            heartbeat: Duration::from_secs(3600),
            ..timing()
        }
    }

    #[tokio::test]
    async fn a_relay_echoes_a_mebibyte_through_the_authority_within_both_windows() {
        let authority = RelayAuthority::new(Serve::Echo);
        let (mut sidecar, _handle, _task) = connect_with(authority.clone(), quiet(), 1 << 20).await;
        let peer = PeerIdentity {
            pid: Some(9),
            uid: Some(1000),
            exe: Some("/usr/bin/curl".into()),
            ..PeerIdentity::default()
        };
        sidecar
            .send(SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "relay.test".into(),
                port: 443,
                auth: None,
                method: None,
                path: None,
                peer: Some(peer.clone()),
            })
            .await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Relay { id: 1 });
        let upload = pattern(1024 * 1024);
        let window = u64::from(RELAY_WINDOW_BYTES);
        let (mut sent, mut credit) = (0usize, window);
        let mut received = Vec::new();
        let mut eof_sent = false;
        loop {
            while sent < upload.len() && credit > 0 {
                let take = (credit as usize)
                    .min(MAX_DATA_BYTES)
                    .min(upload.len() - sent);
                sidecar.send(data(1, &upload[sent..sent + take])).await;
                sent += take;
                credit -= take as u64;
            }
            if sent == upload.len() && !eof_sent {
                sidecar.send(SidecarFrame::Eof { id: 1 }).await;
                eof_sent = true;
            }
            match sidecar.frame().await {
                DaemonFrame::Credit { id: 1, bytes } => {
                    credit += u64::from(bytes);
                    assert!(credit <= window);
                }
                DaemonFrame::Data { id: 1, b } => {
                    let bytes = decoded(&b);
                    assert!(bytes.len() <= MAX_DATA_BYTES);
                    received.extend_from_slice(&bytes);
                    // Written to the client at once: credit it back.
                    sidecar
                        .send(SidecarFrame::Credit {
                            id: 1,
                            bytes: bytes.len() as u32,
                        })
                        .await;
                }
                DaemonFrame::Eof { id: 1 } => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(received, upload);
        assert_eq!(authority.ended().await, Ok(upload.len() as u64));
        let opened = authority.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1);
        assert_eq!((opened[0].generation, opened[0].id), (7, 1));
        assert_eq!(opened[0].open.peer, Some(peer));
        sidecar
            .send(SidecarFrame::Close {
                id: 1,
                ip: None,
                up: upload.len() as u64,
                down: upload.len() as u64,
                ms: 5,
                outcome: CloseOutcome::Closed,
                error: None,
            })
            .await;
        // Bytes for the closed relay are dropped; the channel goes on.
        sidecar.send(data(1, b"late")).await;
        sidecar.send(open(2, "relay.test")).await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Relay { id: 2 });
        let closed = authority.closed.lock().unwrap().clone();
        assert_eq!(closed.len(), 1);
        assert_eq!((closed[0].id, closed[0].ip), (1, None));
    }

    #[tokio::test]
    async fn the_daemon_writes_no_more_than_the_sidecar_credits() {
        let authority = RelayAuthority::new(Serve::Flood(1024 * 1024));
        let (mut sidecar, _handle, _task) = connect_with(authority.clone(), quiet(), 1 << 20).await;
        sidecar.send(open(1, "relay.test")).await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Relay { id: 1 });
        let mut received = 0usize;
        while received < RELAY_WINDOW_BYTES as usize {
            match sidecar.frame().await {
                DaemonFrame::Data { id: 1, b } => received += decoded(&b).len(),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(received, RELAY_WINDOW_BYTES as usize);
        // Nothing more until the sidecar credits it.
        let mut line = Vec::new();
        assert!(tokio::time::timeout(
            Duration::from_millis(300),
            sidecar.from_daemon.read_until(b'\n', &mut line)
        )
        .await
        .is_err());
        sidecar
            .send(SidecarFrame::Credit {
                id: 1,
                bytes: 65_536,
            })
            .await;
        let mut more = 0usize;
        while more < 65_536 {
            match sidecar.frame().await {
                DaemonFrame::Data { id: 1, b } => more += decoded(&b).len(),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(more, 65_536);
        drop(sidecar);
        assert_eq!(authority.ended().await, Err(std::io::ErrorKind::BrokenPipe));
    }

    #[tokio::test]
    async fn control_frames_overtake_queued_relay_data() {
        let authority = RelayAuthority::new(Serve::Flood(RELAY_WINDOW_BYTES as usize));
        // A pipe too small for one data frame holds the writer back.
        let (mut sidecar, handle, _task) = connect_with(authority.clone(), quiet(), 4096).await;
        sidecar.send(open(1, "relay.test")).await;
        // Let the authority queue its whole window.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(handle.revoke(vec![1]));
        let mut seen = Vec::new();
        loop {
            match sidecar.frame().await {
                DaemonFrame::Relay { id: 1 } => seen.push("relay"),
                DaemonFrame::Data { id: 1, .. } => seen.push("data"),
                DaemonFrame::Revoke { ids } => {
                    assert_eq!(ids, vec![1]);
                    seen.push("revoke");
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        // At most the one data frame the writer was already sending.
        assert_eq!(seen[0], "relay");
        assert!(seen.len() <= 3, "{seen:?}");
    }

    #[tokio::test]
    async fn relays_end_when_the_sidecar_closes_them_or_the_channel_ends() {
        let authority = RelayAuthority::new(Serve::Hold);
        let (mut sidecar, _handle, task) = connect_with(authority.clone(), quiet(), 1 << 20).await;
        sidecar.send(open(1, "relay.test")).await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Relay { id: 1 });
        sidecar.send(data(1, b"partial")).await;
        sidecar
            .send(SidecarFrame::Close {
                id: 1,
                ip: None,
                up: 7,
                down: 0,
                ms: 1,
                outcome: CloseOutcome::Revoked,
                error: None,
            })
            .await;
        // Closed before the client finished: the read fails, never a clean end.
        assert_eq!(
            authority.ended().await,
            Err(std::io::ErrorKind::ConnectionReset)
        );

        let authority = RelayAuthority::new(Serve::Hold);
        let (mut sidecar, _handle, task_two) =
            connect_with(authority.clone(), quiet(), 1 << 20).await;
        sidecar.send(open(4, "relay.test")).await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Relay { id: 4 });
        drop(sidecar);
        assert!(matches!(
            task_two.await.unwrap(),
            ControlEnd::ChannelLost(_)
        ));
        assert_eq!(
            authority.ended().await,
            Err(std::io::ErrorKind::ConnectionReset)
        );
        let closed = authority.closed.lock().unwrap().clone();
        assert_eq!(
            closed
                .iter()
                .map(|close| (close.id, close.outcome))
                .collect::<Vec<_>>(),
            vec![(4, CloseOutcome::Interrupted)]
        );
        task.abort();
    }

    #[tokio::test]
    async fn the_default_relay_ends_the_connection_and_http_is_never_relayed() {
        let authority = Arc::new(Recorder::default());
        let (mut sidecar, _handle, _task) = connect(authority.clone(), timing()).await;
        sidecar.send(open(1, "relay.test")).await;
        assert_eq!(
            sidecar.frame_skipping_pings().await,
            DaemonFrame::Relay { id: 1 }
        );
        // The default relay drops the stream: the client reads its end.
        assert_eq!(
            sidecar.frame_skipping_pings().await,
            DaemonFrame::Eof { id: 1 }
        );
        // Client bytes after that are credited back, never buffered.
        sidecar.send(data(1, &[1u8; 1000])).await;
        assert_eq!(
            sidecar.frame_skipping_pings().await,
            DaemonFrame::Credit { id: 1, bytes: 1000 }
        );
        sidecar
            .send(SidecarFrame::Open {
                id: 2,
                kind: RequestKind::Http,
                host: "relay.test".into(),
                port: 80,
                auth: Some(protocol::credential_hash("axe_x")),
                method: Some("GET".into()),
                path: Some("/".into()),
                peer: None,
            })
            .await;
        match sidecar.frame_skipping_pings().await {
            DaemonFrame::Deny {
                id, status, reason, ..
            } => assert_eq!(
                (id, status, reason.as_str()),
                (2, 502, "relay_not_supported")
            ),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn relay_frames_for_connections_never_opened_end_the_channel() {
        for frame in [
            data(5, b"x"),
            SidecarFrame::Eof { id: 5 },
            SidecarFrame::Credit { id: 5, bytes: 1 },
        ] {
            let authority = Arc::new(Recorder::default());
            let (mut sidecar, _handle, task) = connect(authority.clone(), timing()).await;
            sidecar.send(open(1, "denied.test")).await;
            sidecar.send(frame.clone()).await;
            let end = task.await.unwrap();
            assert!(
                matches!(&end, ControlEnd::ChannelLost(detail) if detail.contains("never opened")),
                "{frame:?}: {end:?}"
            );
        }
        // A sidecar that grants past the window ends the channel too.
        let authority = RelayAuthority::new(Serve::Hold);
        let (mut sidecar, _handle, task) = connect_with(authority.clone(), quiet(), 1 << 20).await;
        sidecar.send(open(1, "relay.test")).await;
        assert_eq!(sidecar.frame().await, DaemonFrame::Relay { id: 1 });
        sidecar.send(SidecarFrame::Credit { id: 1, bytes: 1 }).await;
        let end = task.await.unwrap();
        assert!(
            matches!(&end, ControlEnd::ChannelLost(detail) if detail.contains("window")),
            "{end:?}"
        );
        assert_eq!(
            authority.ended().await,
            Err(std::io::ErrorKind::ConnectionReset)
        );
    }

    /// The daemon never buffers more than one window of a client's bytes:
    /// past it, or after the client's end, the sidecar broke the protocol.
    #[test]
    fn a_relay_buffers_at_most_one_window() {
        let shared = RelayShared::new();
        assert_eq!(
            shared.received(vec![0u8; RELAY_WINDOW_BYTES as usize]),
            Ok(None)
        );
        assert!(shared.received(vec![0u8]).unwrap_err().contains("window"));
        let shared = RelayShared::new();
        assert_eq!(shared.received(vec![1u8; 10]), Ok(None));
        shared.received_eof();
        assert!(shared
            .received(vec![1u8])
            .unwrap_err()
            .contains("after its end"));
        assert!(shared.credited(1).unwrap_err().contains("window"));
    }
}
