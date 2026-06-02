//! Interactive PTY-backed terminals running inside a session sandbox.
//!
//! `podman exec -i -t` only sets up a TTY for the inner command when the local
//! podman process's own stdio is a TTY. We give it one by allocating a host
//! pseudoterminal with [`portable_pty`] and spawning podman on the slave end.
//! The result is a bidirectional byte pipe: our reader sees vt100 output, the
//! writer accepts keystrokes — exactly what `xterm.js` expects on the other
//! side of the WebSocket.

use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::broadcast;

/// Live terminal we can drive over a WebSocket: reads stream out, keystrokes
/// stream in. Dropping it kills the child PTY.
pub struct PtyTerminal {
    pub id: String,
    pub command: String,
    /// Cumulative scrollback we tee from the reader, capped to ~64 KiB.
    /// New WS connections receive this so freshly-attached UIs catch up
    /// without re-running the command.
    pub scrollback: Arc<Mutex<Vec<u8>>>,
    /// Live output broadcast — every new chunk goes to all subscribers.
    pub output_tx: broadcast::Sender<Vec<u8>>,
    /// Keystrokes from any subscriber funnel into here and reach the PTY's
    /// stdin via the writer thread.
    pub input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// Status flag flipped to `false` once the child exits.
    pub alive: Arc<Mutex<bool>>,
    /// Hang onto the master so resize requests are possible. Wrapped in a
    /// `Mutex` because `MasterPty: ?Send` operations need exclusive access.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

impl PtyTerminal {
    /// Open a PTY and spawn `podman exec -i -t <container> sh -c <command>`
    /// on the slave end. Returns immediately; output streams to the
    /// broadcast and the scrollback buffer.
    pub fn spawn(
        id: String,
        container: &str,
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
        cmd.args(["exec", "-i", "-t", container, "sh", "-c", command]);
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

        let scrollback = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(64);
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(Mutex::new(true));

        // Reader: blocking std::io::Read, so run it on a blocking thread.
        {
            let scrollback = scrollback.clone();
            let output_tx = output_tx.clone();
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 4096];
                loop {
                    use std::io::Read;
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            if let Ok(mut sb) = scrollback.lock() {
                                sb.extend_from_slice(&chunk);
                                if sb.len() > 64 * 1024 {
                                    let cut = sb.len() - 64 * 1024;
                                    sb.drain(..cut);
                                }
                            }
                            // Best-effort broadcast — if there are no
                            // subscribers, we silently drop and keep going.
                            let _ = output_tx.send(chunk);
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

        Ok(Self {
            id,
            command: command.to_string(),
            scrollback,
            output_tx,
            input_tx,
            alive,
            master: Arc::new(Mutex::new(pair.master)),
        })
    }

    /// Resize the PTY — call this when the xterm.js container in the browser
    /// resizes so the inner program reflows.
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.lock().map(|a| *a).unwrap_or(false)
    }

    /// Snapshot of the scrollback so far — sent to new subscribers so they
    /// catch up before the live stream starts.
    pub fn snapshot(&self) -> Vec<u8> {
        self.scrollback
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}
