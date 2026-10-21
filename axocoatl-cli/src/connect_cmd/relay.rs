//! Running `claude setup-token` (or `codex login`) in a pseudo-terminal as
//! large as the user's terminal (and resized with it on `SIGWINCH`): the
//! user's terminal in raw mode for the duration (always restored),
//! keystrokes relayed to the program until it exits, and its output relayed
//! back through the token filters ([`FilterChain`]). For `connect codex` in
//! the login container, what is shown is also watched for the sign-in
//! page's address, which is opened once on this computer
//! ([`super::browser`]), and a last line that may be Podman's own error is
//! held back for the caller ([`Program::hold_error_line`]). What reaches
//! the user's terminal can be followed ([`Lines`]), so that the message
//! after it is set off by exactly one blank line.

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

use super::browser::{OpenOnce, Opener, SignInPage};
use super::redact::{FilterChain, Found};
use super::screen::{Screen, Step};

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
/// What Podman starts each error it prints with.
const PODMAN_ERROR: &[u8] = b"Error: ";
/// How long a last line that may be Podman's error is held back while the
/// program runs on ([`Program::hold_error_line`]): Podman exits as soon as
/// it has printed one.
const HOLD_LIMIT: Duration = Duration::from_secs(2);
/// The longest line held back; a longer one is not Podman's error.
const MAX_HELD: usize = 4096;

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

/// How the program ended, and the tokens its output held.
pub(crate) struct SetupRun {
    pub(crate) found: Vec<Found>,
    /// Its exit code, when it exited on its own.
    pub(crate) exit_code: Option<u32>,
    pub(crate) interrupted: bool,
    /// Its last line of output, not shown, when it may be Podman's own
    /// error and the program ended with an error (not 0, not interrupted)
    /// ([`Program::hold_error_line`]); empty otherwise. The caller shows it,
    /// or not.
    pub(crate) held_line: Vec<u8>,
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
    let current = std::env::current_dir().ok();
    run_program(
        &Program {
            path: claude,
            args: &["setup-token"],
            cwd: current.as_deref(),
            set_env: &[],
            remove_env: &[],
            filters: FilterChain::claude,
            open_sign_in: None,
            hold_error_line: false,
        },
        input,
        output,
        stop,
        resized,
    )
}

/// A program to run in the pseudo-terminal.
pub(crate) struct Program<'a> {
    /// Its absolute path.
    pub(crate) path: &'a Path,
    pub(crate) args: &'a [&'a str],
    /// Where it runs (the parent's directory when `None`).
    pub(crate) cwd: Option<&'a Path>,
    /// Variables set for it, and variables removed from what it inherits.
    pub(crate) set_env: &'a [(&'a str, &'a std::ffi::OsStr)],
    pub(crate) remove_env: &'a [&'a str],
    /// The filters its output goes through, for a terminal of a size.
    pub(crate) filters: fn(u16, u16) -> FilterChain,
    /// The sign-in page to open once on this computer when its address is
    /// shown, and the opener to open it with (`None`: nothing is opened).
    pub(crate) open_sign_in: Option<(SignInPage, &'a Opener)>,
    /// Whether a last line that may be Podman's own error ([`held_from`])
    /// is held back: until more output follows it (then it is shown), for
    /// at most [`HOLD_LIMIT`] while the program runs, and when the program
    /// ends with an error, for the caller ([`SetupRun::held_line`]). For
    /// `podman exec`, whose own error when the container went away under it
    /// is not the program's output.
    pub(crate) hold_error_line: bool,
}

/// Run `program` as [`run_setup_token`] runs `claude setup-token`.
pub(crate) fn run_program(
    program: &Program<'_>,
    input: BorrowedFd<'_>,
    output: &mut dyn Write,
    stop: &AtomicBool,
    resized: &AtomicBool,
) -> Result<SetupRun, String> {
    let path = program.path;
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
    let writer = keystroke_writer(&*pair.master)
        .map_err(|error| format!("could not write the pseudo-terminal: {error}"))?;
    let mut command = CommandBuilder::new(path);
    command.args(program.args);
    if let Some(directory) = program.cwd {
        command.cwd(directory);
    }
    // The program asks the terminal for its size, which follows resizes; a
    // `COLUMNS` or `LINES` inherited from a shell would not.
    command.env_remove("COLUMNS");
    command.env_remove("LINES");
    for name in program.remove_env {
        command.env_remove(name);
    }
    for (name, value) in program.set_env {
        command.env(name, value);
    }
    // Raw before the program starts, so nothing it reads is cooked first;
    // every return below drops it, which restores the terminal.
    let raw = RawMode::enter(input)
        .map_err(|error| format!("could not put the terminal in raw mode: {error}"))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("could not start {}: {error}", path.display()))?;
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
    let display = Display::new(
        output,
        program
            .open_sign_in
            .map(|(page, opener)| OpenOnce::new(page, opener)),
        program.hold_error_line,
    );
    let result = pump_output(
        &chunks,
        &exited,
        &kill,
        &terminal,
        display,
        (cols, rows),
        stop,
        program.filters,
    );
    relay_stop.store(true, Ordering::SeqCst);
    if result.is_err() {
        kill.store(true, Ordering::SeqCst);
    }
    drop(raw);
    result
}

/// Where keystrokes are written to the program: its terminal's master side,
/// through a descriptor of its own. Not portable-pty's `take_writer`, whose
/// writer, when dropped, types a line break and an end-of-file into the
/// program's terminal. The relay drops its writer once the program has
/// exited (`run_program` waits for that after a stop too). On Linux the
/// program's terminal lasts until the relay closes its own side, so it
/// echoed that line break into the output still being read: an extra blank
/// line after the program's last line (or after the error line held for the
/// caller), always while a process the program left holds the terminal and
/// now and then otherwise; such a process could also read it as a keystroke
/// nobody typed. On macOS the terminal is revoked when the program exits,
/// so the line break reached nothing. Tested by
/// `the_keystroke_writer_types_nothing_when_it_is_dropped` (both platforms)
/// and `the_relay_types_nothing_into_a_terminal_the_program_left_open`
/// (Linux).
pub(crate) fn keystroke_writer(
    master: &dyn portable_pty::MasterPty,
) -> std::io::Result<std::fs::File> {
    let fd = portable_pty::MasterPty::as_raw_fd(master).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the pseudo-terminal has no descriptor",
        )
    })?;
    // SAFETY: `fd` is `master`'s, open for as long as `master` is borrowed
    // here; it is duplicated before this returns.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    Ok(std::fs::File::from(fd.try_clone_to_owned()?))
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
fn relay_keystrokes(input: OwnedFd, mut writer: std::fs::File, stop: &AtomicBool) {
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

/// Where the last line of `bytes` starts when it may be Podman's own error,
/// to be held back: a line (after a CR or LF, or at the start of `bytes`
/// when `line_start`) of at most [`MAX_HELD`] bytes of plain text (no
/// control character, so no color) that starts with [`PODMAN_ERROR`] and
/// is followed by nothing but line breaks; or, while no line break ends it,
/// the start of such a line. `None` when there is none (pure).
pub(crate) fn held_from(bytes: &[u8], line_start: bool) -> Option<usize> {
    let is_break = |byte: &u8| matches!(byte, b'\r' | b'\n');
    let end = bytes
        .iter()
        .rposition(|byte| !is_break(byte))
        .map_or(0, |last| last + 1);
    let start = bytes[..end]
        .iter()
        .rposition(is_break)
        .map_or(0, |at| at + 1);
    let text = &bytes[start..end];
    if (start == 0 && !line_start)
        || text.is_empty()
        || bytes.len() - start > MAX_HELD
        || text.iter().any(u8::is_ascii_control)
    {
        return None;
    }
    let ended = end < bytes.len();
    (text.starts_with(PODMAN_ERROR) || (!ended && PODMAN_ERROR.starts_with(text))).then_some(start)
}

/// The user's terminal as the relay writes to it: what the filters let
/// through, watched for the sign-in page's address when one is to be
/// opened, except a last line held back ([`Program::hold_error_line`]).
struct Display<'w, 'o> {
    output: &'w mut dyn Write,
    open_once: Option<OpenOnce<'o>>,
    hold: bool,
    /// Output not shown yet: a last line that may be Podman's error.
    held: Vec<u8>,
    /// Since when the line in `held` has been held.
    held_since: Instant,
    /// Whether what was shown ends at the start of a line.
    line_start: bool,
}

impl<'w, 'o> Display<'w, 'o> {
    fn new(output: &'w mut dyn Write, open_once: Option<OpenOnce<'o>>, hold: bool) -> Self {
        Self {
            output,
            open_once,
            hold,
            held: Vec::new(),
            held_since: Instant::now(),
            line_start: true,
        }
    }

    /// Show `shown` after what is held, holding back a last line that may
    /// be Podman's error.
    fn show(&mut self, shown: &mut Vec<u8>) {
        if !self.hold {
            self.write(shown);
            shown.clear();
            return;
        }
        let held_before = !self.held.is_empty();
        self.held.append(shown);
        match held_from(&self.held, self.line_start) {
            Some(0) if held_before => {}
            Some(start) => {
                let line = self.held.split_off(start);
                let now = std::mem::replace(&mut self.held, line);
                self.write(&now);
                self.held_since = Instant::now();
            }
            None => {
                let now = std::mem::take(&mut self.held);
                self.write(&now);
            }
        }
    }

    /// Show the held line once it has been held for [`HOLD_LIMIT`]: the
    /// program runs on after it.
    fn show_overdue(&mut self) {
        if !self.held.is_empty() && self.held_since.elapsed() >= HOLD_LIMIT {
            let held = std::mem::take(&mut self.held);
            self.write(&held);
        }
    }

    /// The opener's note, once it has opened the page: written straight to
    /// the user's terminal, never through the filters, whose model of the
    /// terminal follows the program's output alone. It is a line of its
    /// own, before a held one.
    fn note(&mut self) {
        if let Some(note) = self.open_once.as_mut().and_then(OpenOnce::note) {
            let _ = self.output.write_all(&note);
            let _ = self.output.flush();
            self.line_start = true;
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(open_once) = &mut self.open_once {
            open_once.watch(bytes);
        }
        let _ = self.output.write_all(bytes);
        let _ = self.output.flush();
        self.line_start = matches!(bytes.last(), Some(b'\r' | b'\n'));
    }

    /// Show the last of the output (`shown`) and then `reset`, keeping back
    /// the held line when `keep_held` (returned) and showing it otherwise.
    /// The program has ended: nothing is opened for it any more.
    fn finish(mut self, shown: &mut Vec<u8>, reset: &[u8], keep_held: bool) -> Vec<u8> {
        self.open_once = None;
        self.show(shown);
        let held = std::mem::take(&mut self.held);
        if !keep_held {
            self.write(&held);
        }
        let _ = self.output.write_all(reset);
        let _ = self.output.flush();
        if keep_held {
            held
        } else {
            Vec::new()
        }
    }
}

impl Drop for Display<'_, '_> {
    /// A line still held when the relay fails is shown.
    fn drop(&mut self) {
        let held = std::mem::take(&mut self.held);
        self.write(&held);
    }
}

/// Filter the program's output to `output` until it has exited and its
/// output ended (or [`DRAIN`] passed), or until `stop`.
#[allow(clippy::too_many_arguments)]
fn pump_output(
    chunks: &mpsc::Receiver<Zeroizing<Vec<u8>>>,
    exited: &mpsc::Receiver<Option<u32>>,
    kill: &AtomicBool,
    terminal: &Terminal<'_, '_>,
    mut display: Display<'_, '_>,
    size: (u16, u16),
    stop: &AtomicBool,
    filters: fn(u16, u16) -> FilterChain,
) -> Result<SetupRun, String> {
    let mut filter = filters(size.0, size.1);
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
                display.show(&mut shown);
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
        display.note();
        if !output_ended {
            match chunks.recv_timeout(POLL) {
                Ok(chunk) => {
                    filter.push(&chunk, &mut shown);
                    display.show(&mut shown);
                    last_output = Instant::now();
                    flushed_idle = false;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !flushed_idle && last_output.elapsed() >= IDLE_FLUSH {
                        filter.flush_idle(&mut shown);
                        display.show(&mut shown);
                        flushed_idle = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => output_ended = true,
            }
        } else {
            std::thread::sleep(POLL);
        }
        if !has_exited {
            display.show_overdue();
            match exited.try_recv() {
                Ok(code) => {
                    has_exited = true;
                    exit_code = code;
                    drain_until = Some(Instant::now() + DRAIN);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("could not wait for the program".into());
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
    // Then the modes the program left set (it was stopped, or did not clean
    // up) are reset; a held line is kept for the caller when the program
    // ended with an error.
    let held_line = display.finish(
        &mut shown,
        &filter.reset_sequence(),
        !interrupted && exit_code != Some(0),
    );
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
        held_line,
    })
}

/// What the user's terminal shows at the end of what was written to it
/// through [`Lines::follow`]: a [`Screen`] of its size, fed every byte that
/// reached it (the program's output as shown, a held line shown after it or
/// not, the modes reset), so that the message after it is set off by
/// exactly one blank line ([`Lines::before_message`]) however the output
/// ended. A blank line is one that shows nothing (spaces at most, such as
/// a hidden token's padding wrapped onto it). Shared: the writer goes to
/// the relay's thread.
#[derive(Clone)]
pub(crate) struct Lines(Arc<Mutex<Shown>>);

struct Shown {
    screen: Screen,
    /// Whether the line above the first one written to is blank.
    blank_above: bool,
}

impl Lines {
    /// For a terminal of `size` (columns, rows) whose cursor is at the start
    /// of a line, below a blank one when `blank_above`.
    pub(crate) fn new((cols, rows): (u16, u16), blank_above: bool) -> Self {
        Self(Arc::new(Mutex::new(Shown {
            screen: Screen::new(cols, rows),
            blank_above,
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shown> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `output`, with what is written to it fed to these lines.
    pub(crate) fn follow<W: Write>(&self, output: W) -> FollowedOutput<W> {
        FollowedOutput {
            output,
            lines: self.clone(),
        }
    }

    /// `bytes` reached the terminal.
    pub(crate) fn feed(&self, bytes: &[u8]) {
        let screen = &mut self.lock().screen;
        for byte in bytes {
            if let Step::Repeat(count) = screen.feed(*byte).step {
                for _ in 0..count {
                    if screen.repeat_last().is_none() {
                        break;
                    }
                }
            }
        }
    }

    /// The line breaks to write before a message (the terminal is no longer
    /// raw, so `\n` also returns the cursor) for exactly one blank line
    /// between it and the last line that is not: two on a line that is not
    /// blank, one on a blank line below one that is not, none (or `\r`,
    /// back to the line's start) on a blank line below a blank one (more
    /// blank lines the output ended with stay).
    pub(crate) fn before_message(&self) -> &'static str {
        let shown = self.lock();
        let screen = &shown.screen;
        let (x, row) = screen.cursor();
        let blank =
            |row: Option<u64>| row.map_or(shown.blank_above, |row| screen.row_is_blank(row));
        if !blank(Some(row)) {
            "\n\n"
        } else if !blank(row.checked_sub(1)) {
            "\n"
        } else if x > 0 {
            "\r"
        } else {
            ""
        }
    }
}

/// A writer whose output is followed by [`Lines`].
pub(crate) struct FollowedOutput<W> {
    output: W,
    lines: Lines,
}

impl<W: Write> Write for FollowedOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.lines.feed(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// A duplicate of standard input that a blocking task can own.
pub(crate) fn owned_stdin() -> std::io::Result<OwnedFd> {
    std::io::stdin().as_fd().try_clone_to_owned()
}
