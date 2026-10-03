//! The daemon's side of one egress sidecar's control channel.
//!
//! [`start`] waits for the sidecar's `hello`, answers `hello_ack`, then serves
//! the channel: each `open` is decided concurrently by the
//! [`EgressAuthority`], each `close` is recorded, and a `ping` goes out every
//! 5 s. When the channel ends, every connection still open is recorded as
//! `interrupted`, and the authority hears `channel_lost` (or `stopped` after a
//! requested shutdown).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axocoatl_exec::egress::protocol::{
    self, CloseOutcome, DaemonFrame, SidecarFrame, EGRESS_PROTOCOL_VERSION, HEARTBEAT_DEAD_MS,
    HEARTBEAT_MS, MAX_FRAME_BYTES, MAX_REVOKE_IDS,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;

use crate::egress::{CloseReport, Decision, EgressAuthority, OpenRequest, SidecarEvent};

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

/// Send side of a running control channel.
#[derive(Clone, Debug)]
pub struct ControlHandle {
    generation: u32,
    frames: mpsc::UnboundedSender<DaemonFrame>,
    shutdown: Arc<AtomicBool>,
}

impl ControlHandle {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Close these connections now. Returns false once the channel is gone.
    pub fn revoke(&self, ids: Vec<u64>) -> bool {
        ids.chunks(MAX_REVOKE_IDS).all(|chunk| {
            self.frames
                .send(DaemonFrame::Revoke {
                    ids: chunk.to_vec(),
                })
                .is_ok()
        })
    }

    /// Ask the sidecar to exit.
    pub fn shutdown(&self) -> bool {
        self.shutdown.store(true, Ordering::Release);
        self.frames.send(DaemonFrame::Shutdown).is_ok()
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

    let (frames, mut outgoing) = mpsc::unbounded_channel::<DaemonFrame>();
    let write_failed = Arc::new(AtomicBool::new(false));
    let writer_failed = write_failed.clone();
    tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
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

async fn serve<R: AsyncRead + Unpin + Send + 'static>(
    generation: u32,
    mut reader: BufReader<R>,
    frames: mpsc::UnboundedSender<DaemonFrame>,
    authority: Arc<dyn EgressAuthority>,
    timing: ControlTiming,
    shutdown: Arc<AtomicBool>,
    write_failed: Arc<AtomicBool>,
) -> ControlEnd {
    let open: Arc<Mutex<HashSet<u64>>> = Arc::new(Mutex::new(HashSet::new()));
    let decisions = Arc::new(Semaphore::new(MAX_CONCURRENT_DECISIONS as usize));
    let mut ping = tokio::time::interval(timing.heartbeat);
    ping.tick().await;
    let mut last_frame = Instant::now();
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
                    SidecarFrame::Open { id, kind, host, port, auth, method, path } => {
                        let Ok(permit) = decisions.clone().acquire_owned().await else {
                            break ControlEnd::ChannelLost("decision capacity closed".into());
                        };
                        let authority = authority.clone();
                        let frames = frames.clone();
                        let open = open.clone();
                        tokio::spawn(async move {
                            let decision = authority
                                .decide(OpenRequest { generation, id, kind, host, port, auth, method, path })
                                .await;
                            if matches!(decision, Decision::Allow { .. }) {
                                open.lock().unwrap_or_else(|poison| poison.into_inner()).insert(id);
                            }
                            let _ = frames.send(to_frame(id, decision));
                            drop(permit);
                        });
                    }
                    SidecarFrame::Close { id, ip, up, down, ms, outcome, error } => {
                        open.lock().unwrap_or_else(|poison| poison.into_inner()).remove(&id);
                        authority
                            .closed(CloseReport { generation, id, ip, up, down, ms, outcome, error })
                            .await;
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
    // Let in-flight decisions finish so every allowed connection is known.
    let _all = decisions.acquire_many(MAX_CONCURRENT_DECISIONS).await.ok();
    let remaining: Vec<u64> = open
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .drain()
        .collect();
    let mut remaining = remaining;
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
        let (daemon_read, sidecar_write) = duplex(1 << 20);
        let (sidecar_read, daemon_write) = duplex(1 << 20);
        let mut sidecar = Sidecar {
            to_daemon: sidecar_write,
            from_daemon: tokio::io::BufReader::new(sidecar_read),
        };
        sidecar
            .send(SidecarFrame::Hello {
                protocol: 1,
                version: "test".into(),
                max_connections: 8,
            })
            .await;
        let (handle, task) = start(7, daemon_read, daemon_write, authority, timing)
            .await
            .unwrap();
        assert_eq!(sidecar.frame().await, DaemonFrame::HelloAck { protocol: 1 });
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
            b"{\"t\":\"hello\",\"protocol\":1,\"version\":\"x\",\"max_connections\":8}\n",
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
}
