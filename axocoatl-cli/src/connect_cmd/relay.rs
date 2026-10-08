//! Running `claude setup-token` in a pseudo-terminal as large as the user's
//! terminal (and resized with it on `SIGWINCH`): the user's terminal in raw
//! mode for the duration (always restored), keystrokes relayed to the
//! program until it exits, and its output relayed back through
//! [`TokenFilter`].

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Once};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rustix::event::{
    fd_set_insert, fd_set_num_elements, FdSetElement, PollFd, PollFlags, Timespec,
};
use rustix::termios::{self, OptionalActions, SpecialCodeIndex, Termios};
use zeroize::{Zeroize, Zeroizing};

use super::redact::{Found, TokenFilter};

/// The size used when the user's terminal does not report one.
const FALLBACK_SIZE: (u16, u16) = (80, 24);
/// The smallest terminal `claude` runs in, in columns and rows. Claude Code
/// draws its sign-in screen for its terminal's size; when the screen is
/// taller than the terminal it redraws only the rows that fit, and in a
/// terminal much smaller than this the token's first rows can be among
/// those it never draws (while the rest is drawn, without the `sk-ant-`
/// that marks it). A smaller terminal is enlarged to this: the screen may
/// then look broken in it, but the token is whole, and hidden.
pub(crate) const MIN_SIZE: (u16, u16) = (40, 24);
/// How often the waiter looks for the program's exit (the keystroke relay
/// stops within this of it).
const EXIT_POLL: Duration = Duration::from_millis(5);
/// Held-back output is shown once the program has been quiet this long.
const IDLE_FLUSH: Duration = Duration::from_millis(250);
/// After the program exits, its remaining output is read for at most this
/// long.
const DRAIN: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);

/// A terminal in raw mode: its descriptor, its settings before, and the
/// thread that made it raw.
type SavedTerminal = (RawFd, Termios, std::thread::ThreadId);

/// The terminal settings to put back if the process panics while a
/// terminal is raw (the release profile aborts on panic, so no destructor
/// runs then).
static RESTORE_ON_PANIC: Mutex<Vec<SavedTerminal>> = Mutex::new(Vec::new());
static PANIC_HOOK: Once = Once::new();

fn saved_terminals() -> std::sync::MutexGuard<'static, Vec<SavedTerminal>> {
    RESTORE_ON_PANIC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the panic hook does: put back every terminal still raw (in tests,
/// where a panic fails one test and the others go on, only the terminals
/// the panicking thread made raw).
pub(crate) fn restore_saved_terminals() {
    let current = std::thread::current().id();
    saved_terminals().retain(|(fd, settings, thread)| {
        if cfg!(test) && *thread != current {
            return true;
        }
        // SAFETY: each descriptor is a terminal that a live `RawMode`
        // borrows; it is removed from the list when that `RawMode` drops.
        let fd = unsafe { BorrowedFd::borrow_raw(*fd) };
        let _ = termios::tcsetattr(fd, OptionalActions::Now, settings);
        false
    });
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
        saved_terminals().push((raw_fd, saved.clone(), std::thread::current().id()));
        let mut raw = saved.clone();
        raw.make_raw();
        // A read returns after a tenth of a second without input, so even
        // where the terminal cannot be watched (see `relay_keystrokes`) no
        // read waits for a keystroke past the program's exit.
        raw.special_codes[SpecialCodeIndex::VMIN] = 0;
        raw.special_codes[SpecialCodeIndex::VTIME] = 1;
        if let Err(error) = termios::tcsetattr(fd, OptionalActions::Now, &raw) {
            forget_saved(raw_fd);
            return Err(error.into());
        }
        Ok(Self { fd, saved })
    }
}

fn forget_saved(fd: RawFd) {
    let mut saved = saved_terminals();
    if let Some(index) = saved.iter().rposition(|(saved_fd, ..)| *saved_fd == fd) {
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

/// The columns and rows of the user's terminal (80 by 24 when it reports
/// none).
pub(crate) fn terminal_size(fd: BorrowedFd<'_>) -> (u16, u16) {
    termios::tcgetwinsize(fd)
        .ok()
        .filter(|size| size.ws_col > 0 && size.ws_row > 0)
        .map(|size| (size.ws_col, size.ws_row))
        .unwrap_or(FALLBACK_SIZE)
}

/// The size of `claude`'s terminal for a user's terminal of `size`: the
/// same, enlarged to at least [`MIN_SIZE`].
pub(crate) fn program_size((cols, rows): (u16, u16)) -> (u16, u16) {
    (cols.max(MIN_SIZE.0), rows.max(MIN_SIZE.1))
}

/// Run `claude setup-token` (`claude` is its absolute path) in a
/// pseudo-terminal the size of the user's terminal (at least [`MIN_SIZE`]),
/// with `input` (the user's terminal) in raw mode and the filtered output
/// written to `output`.
/// `resized` (set on `SIGWINCH`) resizes the pseudo-terminal to the user's
/// terminal again; `stop` ends it early (a signal): the program is killed.
pub(crate) fn run_setup_token(
    claude: &Path,
    input: BorrowedFd<'_>,
    output: &mut dyn Write,
    stop: &AtomicBool,
    resized: &AtomicBool,
) -> Result<SetupRun, String> {
    // A resize before the start is read with the size below.
    resized.store(false, Ordering::SeqCst);
    let (cols, rows) = program_size(terminal_size(input));
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
    // The program asks the terminal for its size, which follows resizes; a
    // `COLUMNS` or `LINES` inherited from a shell would not.
    command.env_remove("COLUMNS");
    command.env_remove("LINES");
    // Raw before the program starts, so nothing it reads is cooked first;
    // every return below drops it, which restores the terminal.
    let raw = RawMode::enter(input)
        .map_err(|error| format!("could not put the terminal in raw mode: {error}"))?;
    let child = pair
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
        // `relay_stop`, without reading anything after it.
        std::thread::spawn(move || relay_keystrokes(keystrokes, writer, &relay_stop));
    }
    let kill = Arc::new(AtomicBool::new(false));
    let (exited_sender, exited) = mpsc::channel::<Option<u32>>();
    {
        let relay_stop = relay_stop.clone();
        let kill = kill.clone();
        std::thread::spawn(move || wait_for_exit(child, &relay_stop, &kill, &exited_sender));
    }
    let terminal = Terminal {
        input,
        master: &*pair.master,
        resized,
    };
    let result = pump_output(
        &chunks,
        &exited,
        &kill,
        &terminal,
        output,
        (cols, rows),
        stop,
    );
    relay_stop.store(true, Ordering::SeqCst);
    if result.is_err() {
        kill.store(true, Ordering::SeqCst);
    }
    drop(raw);
    result
}

/// The user's terminal and the program's, for resizes.
struct Terminal<'a, 'fd> {
    input: BorrowedFd<'fd>,
    master: &'a dyn portable_pty::MasterPty,
    resized: &'a AtomicBool,
}

/// Own the program until it exits: kill it when `kill` is set, and the
/// moment it exits stop the keystroke relay (so nothing typed after it,
/// during the token check for example, is read away from the shell) and
/// send its exit code.
fn wait_for_exit(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    relay_stop: &AtomicBool,
    kill: &AtomicBool,
    exited: &mpsc::Sender<Option<u32>>,
) {
    let mut killed = false;
    loop {
        if kill.load(Ordering::SeqCst) && !killed {
            killed = true;
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                relay_stop.store(true, Ordering::SeqCst);
                let _ = exited.send(Some(status.exit_code()));
                return;
            }
            Ok(None) => std::thread::sleep(EXIT_POLL),
            Err(_) => {
                relay_stop.store(true, Ordering::SeqCst);
                let _ = exited.send(None);
                return;
            }
        }
    }
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

/// Wait up to `timeout` for `input` to be readable: `Some(true)` when it is,
/// `Some(false)` on a timeout, `None` when neither `poll` nor `select` can
/// watch it.
fn wait_readable(input: &OwnedFd, timeout: &Timespec, use_select: &mut bool) -> Option<bool> {
    if !*use_select {
        let mut fds = [PollFd::new(input, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(timeout)) {
            Ok(0) => return Some(false),
            Ok(_) => {
                let revents = fds[0].revents();
                if revents.intersects(PollFlags::IN) {
                    return Some(true);
                }
                if !revents.intersects(PollFlags::NVAL) {
                    // Hung up or failed: a read says which.
                    return Some(revents.intersects(PollFlags::HUP | PollFlags::ERR));
                }
            }
            Err(rustix::io::Errno::INTR) => return Some(false),
            Err(_) => {}
        }
        // macOS cannot `poll` some terminal devices; `select` can.
        *use_select = true;
    }
    let fd = input.as_raw_fd();
    let mut set = vec![FdSetElement::default(); fd_set_num_elements(1, fd + 1)];
    fd_set_insert(&mut set, fd);
    // SAFETY: `fd` is `input`'s, open for the whole call.
    match unsafe { rustix::event::select(fd + 1, Some(&mut set), None, None, Some(timeout)) } {
        Ok(0) => Some(false),
        Ok(_) => Some(true),
        Err(rustix::io::Errno::INTR) => Some(false),
        Err(_) => None,
    }
}

/// Copy keystrokes from the user's terminal to the program until `stop`
/// (set the moment the program exits). It waits for input with a timeout
/// and checks `stop` again before each read, so input typed after the
/// program exits stays in the terminal for the shell. `select` watches every
/// terminal on macOS and `poll` every one on Linux; were neither able to,
/// reads would still return within a tenth of a second (the raw mode's
/// `VTIME`), so `stop` is seen within it.
fn relay_keystrokes(input: OwnedFd, mut writer: Box<dyn Write + Send>, stop: &AtomicBool) {
    let mut buffer = [0u8; 1024];
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let mut use_select = false;
    let mut watchable = true;
    while !stop.load(Ordering::SeqCst) {
        if watchable {
            match wait_readable(&input, &timeout, &mut use_select) {
                Some(true) => {}
                Some(false) => continue,
                None => watchable = false,
            }
            if stop.load(Ordering::SeqCst) {
                break;
            }
        }
        match rustix::io::read(&input, &mut buffer) {
            // Unwatched, nothing was typed within `VTIME`; watched, the
            // terminal hung up.
            Ok(0) if !watchable => {}
            Ok(0) => break,
            Ok(read) => {
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
    exited: &mpsc::Receiver<Option<u32>>,
    kill: &AtomicBool,
    terminal: &Terminal<'_, '_>,
    output: &mut dyn Write,
    size: (u16, u16),
    stop: &AtomicBool,
) -> Result<SetupRun, String> {
    let mut filter = TokenFilter::new(size.0, size.1);
    let mut size = size;
    let mut shown = Vec::with_capacity(8192);
    let mut last_output = Instant::now();
    let mut flushed_idle = true;
    let mut exit_code = None;
    let mut has_exited = false;
    let mut drain_until: Option<Instant> = None;
    let mut output_ended = false;
    let mut interrupted = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            interrupted = true;
            kill.store(true, Ordering::SeqCst);
            break;
        }
        if terminal.resized.swap(false, Ordering::SeqCst) {
            let new_size = program_size(terminal_size(terminal.input));
            if new_size != size {
                // What was already read was drawn for the old size.
                while let Ok(chunk) = chunks.try_recv() {
                    filter.push(&chunk, &mut shown);
                }
                show(output, &mut shown);
                let _ = terminal.master.resize(PtySize {
                    rows: new_size.1,
                    cols: new_size.0,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                filter.resize(new_size.0, new_size.1);
                size = new_size;
            }
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
        if !has_exited {
            match exited.try_recv() {
                Ok(code) => {
                    has_exited = true;
                    exit_code = code;
                    drain_until = Some(Instant::now() + DRAIN);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("could not wait for claude".into());
                }
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
    // Modes the program left set (it was stopped, or did not clean up).
    shown.extend_from_slice(&filter.reset_sequence());
    show(output, &mut shown);
    if interrupted {
        for _ in 0..20 {
            if exited.try_recv().is_ok() {
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
