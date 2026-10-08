//! Running `claude setup-token` in a pseudo-terminal: the user's terminal in
//! raw mode for the duration (always restored), keystrokes relayed to the
//! program, and its output relayed back through [`TokenFilter`].

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rustix::event::{PollFd, PollFlags};
use rustix::termios::{self, OptionalActions, Termios};
use zeroize::{Zeroize, Zeroizing};

use super::redact::{Found, TokenFilter};

/// How wide the program's terminal is: wide enough that the token is printed
/// on one line.
pub(crate) const WIDE_COLUMNS: u16 = 1000;
/// Held-back output is shown once the program has been quiet this long.
const IDLE_FLUSH: Duration = Duration::from_millis(250);
/// After the program exits, its remaining output is read for at most this
/// long.
const DRAIN: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

/// The terminal settings to put back if the process panics while a
/// terminal is raw (the release profile aborts on panic, so no destructor
/// runs then).
static RESTORE_ON_PANIC: Mutex<Vec<(RawFd, Termios)>> = Mutex::new(Vec::new());
static PANIC_HOOK: Once = Once::new();

fn saved_terminals() -> std::sync::MutexGuard<'static, Vec<(RawFd, Termios)>> {
    RESTORE_ON_PANIC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the panic hook does: put back every terminal still raw.
pub(crate) fn restore_saved_terminals() {
    for (fd, settings) in saved_terminals().drain(..) {
        // SAFETY: each descriptor is a terminal that a live `RawMode`
        // borrows; it is removed from the list when that `RawMode` drops.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = termios::tcsetattr(fd, OptionalActions::Now, &settings);
    }
}

/// A terminal in raw mode until this is dropped (or the process panics).
pub(crate) struct RawMode<'fd> {
    fd: BorrowedFd<'fd>,
    saved: Termios,
}

impl<'fd> RawMode<'fd> {
    pub(crate) fn enter(fd: BorrowedFd<'fd>) -> std::io::Result<Self> {
        let saved = termios::tcgetattr(fd)?;
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore_saved_terminals();
                previous(info);
            }));
        });
        let raw_fd = fd.as_raw_fd();
        saved_terminals().push((raw_fd, saved.clone()));
        let mut raw = saved.clone();
        raw.make_raw();
        if let Err(error) = termios::tcsetattr(fd, OptionalActions::Now, &raw) {
            forget_saved(raw_fd);
            return Err(error.into());
        }
        Ok(Self { fd, saved })
    }
}

fn forget_saved(fd: RawFd) {
    let mut saved = saved_terminals();
    if let Some(index) = saved.iter().rposition(|(saved_fd, _)| *saved_fd == fd) {
        saved.remove(index);
    }
}

impl Drop for RawMode<'_> {
    fn drop(&mut self) {
        forget_saved(self.fd.as_raw_fd());
        let _ = termios::tcsetattr(self.fd, OptionalActions::Now, &self.saved);
    }
}

/// How `claude setup-token` ended, and the tokens its output held.
pub(crate) struct SetupRun {
    pub(crate) found: Vec<Found>,
    /// Its exit code, when it exited on its own.
    pub(crate) exit_code: Option<u32>,
    pub(crate) interrupted: bool,
}

/// The rows of the user's terminal, else 24.
pub(crate) fn terminal_rows(fd: BorrowedFd<'_>) -> u16 {
    termios::tcgetwinsize(fd)
        .ok()
        .map(|size| size.ws_row)
        .filter(|rows| *rows > 0)
        .unwrap_or(24)
}

/// Run `claude setup-token` (`claude` is its absolute path) in a
/// pseudo-terminal `cols` wide and as tall as the user's terminal, with
/// `input` (the user's terminal) in raw mode and the filtered output written
/// to `output`. `stop` ends it early (a signal): the program is killed.
pub(crate) fn run_setup_token(
    claude: &Path,
    input: BorrowedFd<'_>,
    output: &mut dyn Write,
    cols: u16,
    stop: &AtomicBool,
) -> Result<SetupRun, String> {
    let rows = terminal_rows(input);
    let keystrokes = input
        .try_clone_to_owned()
        .map_err(|error| format!("could not read the terminal: {error}"))?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("could not open a pseudo-terminal: {error}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("could not read the pseudo-terminal: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("could not write the pseudo-terminal: {error}"))?;
    let mut command = CommandBuilder::new(claude);
    command.arg("setup-token");
    if let Ok(directory) = std::env::current_dir() {
        command.cwd(directory);
    }
    command.env("COLUMNS", cols.to_string());
    command.env("LINES", rows.to_string());
    // Raw before the program starts, so nothing it reads is cooked first;
    // every return below drops it, which restores the terminal.
    let raw = RawMode::enter(input)
        .map_err(|error| format!("could not put the terminal in raw mode: {error}"))?;
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("could not start {}: {error}", claude.display()))?;
    // Only the child holds the terminal's other end now, so its output ends
    // when it (and anything it started) exits.
    drop(pair.slave);

    let (sender, chunks) = mpsc::channel::<Zeroizing<Vec<u8>>>();
    // Not joined: a process the program left behind may keep the terminal
    // open, and this thread then ends with the process.
    std::thread::spawn(move || read_output(reader, &sender));
    let relay_stop = Arc::new(AtomicBool::new(false));
    {
        let relay_stop = relay_stop.clone();
        // Not joined either: it stops within one poll interval of
        // `relay_stop`, or, on a terminal `poll` cannot watch, at the next
        // keystroke.
        std::thread::spawn(move || relay_keystrokes(keystrokes, writer, &relay_stop));
    }
    let result = pump_output(&chunks, &mut *child, output, cols, stop);
    relay_stop.store(true, Ordering::SeqCst);
    if result.is_err() {
        let _ = child.kill();
    }
    drop(raw);
    result
}

/// Send the program's output, chunk by chunk, until it ends.
fn read_output(mut reader: Box<dyn Read + Send>, sender: &mpsc::Sender<Zeroizing<Vec<u8>>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if sender
                    .send(Zeroizing::new(buffer[..read].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    buffer.zeroize();
}

/// Copy keystrokes from the user's terminal to the program until `stop`.
fn relay_keystrokes(input: OwnedFd, mut writer: Box<dyn Write + Send>, stop: &AtomicBool) {
    let mut buffer = [0u8; 1024];
    let timeout = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    // macOS cannot `poll` some terminal devices (`/dev/tty`); then reads
    // block instead.
    let mut pollable = true;
    while !stop.load(Ordering::SeqCst) {
        if pollable {
            let mut fds = [PollFd::new(&input, PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => pollable = false,
            }
            let revents = fds[0].revents();
            if revents.intersects(PollFlags::NVAL) {
                pollable = false;
            } else if !revents.intersects(PollFlags::IN) {
                if revents.intersects(PollFlags::HUP | PollFlags::ERR) {
                    break;
                }
                continue;
            }
        }
        match rustix::io::read(&input, &mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if writer.write_all(&buffer[..read]).is_err() || writer.flush().is_err() {
                    break;
                }
            }
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
            Err(_) => break,
        }
    }
    buffer.zeroize();
}

fn show(output: &mut dyn Write, shown: &mut Vec<u8>) {
    if !shown.is_empty() {
        let _ = output.write_all(shown);
        let _ = output.flush();
        shown.clear();
    }
}

/// Filter the program's output to `output` until it has exited and its
/// output ended (or [`DRAIN`] passed), or until `stop`.
fn pump_output(
    chunks: &mpsc::Receiver<Zeroizing<Vec<u8>>>,
    child: &mut dyn portable_pty::Child,
    output: &mut dyn Write,
    cols: u16,
    stop: &AtomicBool,
) -> Result<SetupRun, String> {
    let mut filter = TokenFilter::new(cols);
    let mut shown = Vec::with_capacity(8192);
    let mut last_output = Instant::now();
    let mut flushed_idle = true;
    let mut exit_code = None;
    let mut drain_until: Option<Instant> = None;
    let mut output_ended = false;
    let mut interrupted = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            interrupted = true;
            let _ = child.kill();
            break;
        }
        if !output_ended {
            match chunks.recv_timeout(POLL) {
                Ok(chunk) => {
                    filter.push(&chunk, &mut shown);
                    show(output, &mut shown);
                    last_output = Instant::now();
                    flushed_idle = false;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !flushed_idle && last_output.elapsed() >= IDLE_FLUSH {
                        filter.flush_idle(&mut shown);
                        show(output, &mut shown);
                        flushed_idle = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => output_ended = true,
            }
        } else {
            std::thread::sleep(POLL);
        }
        if exit_code.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_code = Some(status.exit_code());
                    drain_until = Some(Instant::now() + DRAIN);
                }
                Ok(None) => {}
                Err(error) => return Err(format!("could not wait for claude: {error}")),
            }
        }
        if let Some(deadline) = drain_until {
            if output_ended || Instant::now() >= deadline {
                break;
            }
        }
    }
    // Whatever arrived before the end is still filtered.
    while let Ok(chunk) = chunks.try_recv() {
        filter.push(&chunk, &mut shown);
    }
    filter.finish(&mut shown);
    show(output, &mut shown);
    if interrupted {
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    let found = filter.into_found();
    Ok(SetupRun {
        found,
        exit_code,
        interrupted,
    })
}

/// A duplicate of standard input that a blocking task can own.
pub(crate) fn owned_stdin() -> std::io::Result<OwnedFd> {
    std::io::stdin().as_fd().try_clone_to_owned()
}
