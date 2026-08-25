//! Interactive PTY-backed terminals for a session sandbox.
//!
//! A terminal is backend-neutral: a bidirectional byte pipe (vt100 output out,
//! keystrokes in) plus resize and a liveness flag — exactly what `xterm.js`
//! expects on the other side of the WebSocket. How that pipe is produced is
//! backend-specific and lives in the constructors:
//!
//! - [`PtyTerminal::spawn_podman`] allocates a host pseudoterminal with
//!   [`portable_pty`] and runs `podman exec -i -t` on the slave end (the local
//!   podman process needs a TTY on its own stdio for the inner command to get
//!   one).
//! - a remote backend (E2B) feeds the same channels from its own PTY stream.
//!
//! Everything after construction — scrollback, the output broadcast, the input
//! channel, `resize`, `is_alive` — is identical regardless of backend.

use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::broadcast;

const PTY_SCROLLBACK_MAX_BYTES: usize = 64 * 1024;

struct PtyOutputInner {
    /// Serializes one append+publish commit with subscribe+snapshot cuts.
    gate: Mutex<()>,
    scrollback: Mutex<Vec<u8>>,
    sender: broadcast::Sender<Vec<u8>>,
}

/// Shared output owner used by both local and remote PTY pumps.
///
/// Keeping construction crate-private prevents a backend from accidentally
/// updating scrollback and live subscribers as two unrelated operations.
#[derive(Clone)]
pub(crate) struct PtyOutput {
    inner: Arc<PtyOutputInner>,
}

impl PtyOutput {
    pub(crate) fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(PtyOutputInner {
                gate: Mutex::new(()),
                scrollback: Mutex::new(Vec::new()),
                sender,
            }),
        }
    }

    /// Append to the retained tail and publish the same bytes as one commit.
    pub(crate) fn append(&self, chunk: Vec<u8>) {
        let _commit = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut scrollback = self
            .inner
            .scrollback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scrollback.extend_from_slice(&chunk);
        if scrollback.len() > PTY_SCROLLBACK_MAX_BYTES {
            let cut = scrollback.len() - PTY_SCROLLBACK_MAX_BYTES;
            scrollback.drain(..cut);
        }
        // With no subscriber the retained scrollback is still authoritative.
        // With subscribers this send occurs before the append commit unlocks.
        let _ = self.inner.sender.send(chunk);
    }

    /// Atomically subscribe to future output and copy the retained tail.
    ///
    /// Output committed before this cut appears only in `snapshot`; output
    /// committed after it appears only through `receiver`.
    fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        let _commit = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receiver = self.inner.sender.subscribe();
        let snapshot = self
            .inner
            .scrollback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (snapshot, receiver)
    }

    fn snapshot(&self) -> Vec<u8> {
        let _commit = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner
            .scrollback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Live terminal we can drive over a WebSocket: reads stream out, keystrokes
/// stream in. Dropping it tears the backing child/stream down.
pub struct PtyTerminal {
    pub id: String,
    pub command: String,
    output: PtyOutput,
    /// Keystrokes from any subscriber funnel into here and reach the terminal's
    /// stdin via the writer/pump.
    pub input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// Status flag flipped to `false` once the child exits.
    pub alive: Arc<Mutex<bool>>,
    /// Backend-specific resize hook. Podman captures the pty master; a remote
    /// backend captures its resize RPC. Owning the backend handle here also
    /// means dropping the terminal tears that handle (and the child) down.
    resize_hook: Box<dyn Fn(u16, u16) + Send + Sync>,
}

impl PtyTerminal {
    /// Open a host PTY and spawn `podman exec -i -t <container> sh -c <command>`
    /// on the slave end. Returns immediately; output streams to the broadcast
    /// and the scrollback buffer.
    pub fn spawn_podman(
        id: String,
        container: &str,
        workdir: &std::path::Path,
        command: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Self, String> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| {
                let msg = format!("openpty failed: {e}");
                tracing::error!("{msg}");
                msg
            })?;

        let mut cmd = CommandBuilder::new("podman");
        // `-w` so a terminal opened in a variant lane starts in that lane's
        // worktree rather than the container's default (the session root).
        cmd.args(["exec", "-i", "-t", "-w"]);
        cmd.arg(workdir);
        cmd.args([container, "sh", "-c", command]);
        // No TERM in the parent could otherwise leave vt100 features off.
        cmd.env("TERM", "xterm-256color");

        let mut child = pair.slave.spawn_command(cmd).map_err(|e| {
            let msg = format!("spawning podman exec -t in {container}: {e}");
            tracing::error!("{msg}");
            msg
        })?;
        // Drop the slave handle so the PTY closes when the child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone reader: {e}"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take writer: {e}"))?;

        let output = PtyOutput::new(64);
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(Mutex::new(true));

        // Reader: blocking std::io::Read, so run it on a blocking thread.
        {
            let output = output.clone();
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 4096];
                loop {
                    use std::io::Read;
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            output.append(chunk);
                        }
                    }
                }
            });
        }

        // Writer: pump every incoming chunk into the PTY master's writer.
        std::thread::spawn(move || {
            use std::io::Write;
            while let Ok(bytes) = input_rx.recv() {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
        });

        // Reaper: flip `alive` to false once the child exits.
        {
            let alive = alive.clone();
            std::thread::spawn(move || {
                let _ = child.wait();
                if let Ok(mut a) = alive.lock() {
                    *a = false;
                }
            });
        }

        // The resize hook owns the pty master. Wrapped in a `Mutex` because
        // `MasterPty: ?Send` operations need exclusive access; owning it here
        // means the master (and thus the child PTY) drops with the terminal.
        let master = Arc::new(Mutex::new(pair.master));
        let resize_hook: Box<dyn Fn(u16, u16) + Send + Sync> = Box::new(move |rows, cols| {
            if let Ok(m) = master.lock() {
                let _ = m.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        });

        Ok(Self {
            id,
            command: command.to_string(),
            output,
            input_tx,
            alive,
            resize_hook,
        })
    }

    /// Build a terminal from already-wired channels and a backend resize hook.
    /// Used by remote backends whose output/input pumps are set up by the caller
    /// (the Podman path uses [`PtyTerminal::spawn_podman`] instead).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        id: String,
        command: String,
        output: PtyOutput,
        input_tx: std::sync::mpsc::Sender<Vec<u8>>,
        alive: Arc<Mutex<bool>>,
        resize_hook: Box<dyn Fn(u16, u16) + Send + Sync>,
    ) -> Self {
        Self {
            id,
            command,
            output,
            input_tx,
            alive,
            resize_hook,
        }
    }

    /// Resize the terminal — call this when the xterm.js container in the browser
    /// resizes so the inner program reflows.
    pub fn resize(&self, rows: u16, cols: u16) {
        (self.resize_hook)(rows, cols);
    }

    pub fn is_alive(&self) -> bool {
        self.alive.lock().map(|a| *a).unwrap_or(false)
    }

    /// Return one exact attach cut: retained output followed by a receiver that
    /// contains only output committed after that retained snapshot.
    pub fn attach_output(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        self.output.attach()
    }

    /// Snapshot of the scrollback so far — sent to new subscribers so they
    /// catch up before the live stream starts.
    pub fn snapshot(&self) -> Vec<u8> {
        self.output.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_cut_delivers_output_committed_during_the_cut_exactly_once() {
        let output = PtyOutput::new(8);
        output.append(b"before".to_vec());
        let (input_tx, _input_rx) = std::sync::mpsc::channel();
        let terminal = Arc::new(PtyTerminal::from_parts(
            "term-exact-cut".to_string(),
            "sh".to_string(),
            output.clone(),
            input_tx,
            Arc::new(Mutex::new(true)),
            Box::new(|_, _| {}),
        ));

        // Hold scrollback after attach has taken the outer gate. The append is
        // forced to queue behind that gate, so it must land in the returned
        // receiver rather than being duplicated into or lost from snapshot.
        let scrollback = output
            .inner
            .scrollback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let attach_terminal = terminal.clone();
        let attach = std::thread::spawn(move || attach_terminal.attach_output());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if output.inner.gate.try_lock().is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "attach did not acquire the output gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let append_output = output.clone();
        let append = std::thread::spawn(move || append_output.append(b"during".to_vec()));
        drop(scrollback);

        let (snapshot, mut receiver) = attach.join().unwrap();
        append.join().unwrap();
        assert_eq!(snapshot, b"before");
        assert_eq!(receiver.try_recv().unwrap(), b"during");
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}
