//! Linux-only, single-threaded descendant supervision. No caller-visible exit
//! status or pipe EOF substitutes for the kernel's final `ECHILD` observation.

use crate::protocol::{
    bounded_message, CapturedOutput, Control, ExecRequest, OutputCapture, PrimaryExit,
    ProcessOutcome, ServerMessage, CLEANUP_TIMEOUT_MS, MAX_CONTROL_BYTES, MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES,
};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CHILD_LIST_BYTES: u64 = 1024 * 1024;
const TERM_GRACE: Duration = Duration::from_millis(250);
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_MS: i32 = 10;
static CANCELLED: AtomicBool = AtomicBool::new(false);

extern "C" fn cancellation_signal(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

/// How `--serve` launches its command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeOptions {
    /// `--harden`: the command and its descendants get `PR_SET_NO_NEW_PRIVS`
    /// and the seccomp denylist of [`crate::harden`], after any Landlock
    /// restriction and before `execve`.
    pub harden: bool,
}

/// Run exactly one request. This must be called by the dedicated helper binary,
/// never inside a multithreaded host: the helper is the only child reaper.
pub fn serve() -> Result<(), String> {
    serve_with(ServeOptions::default())
}

/// [`serve`] with options.
pub fn serve_with(options: ServeOptions) -> Result<(), String> {
    establish_supervision()?;
    nonblocking(libc::STDIN_FILENO)?;
    nonblocking(libc::STDOUT_FILENO)?;
    let mut input = Input::new();
    let opened = Instant::now();
    let request: ExecRequest = loop {
        if opened.elapsed() >= INITIAL_REQUEST_TIMEOUT {
            return Err("initial request timed out".into());
        }
        if let Some(frame) = input.frame(MAX_REQUEST_BYTES)? {
            break serde_json::from_slice(&frame)
                .map_err(|error| format!("invalid request: {error}"))?;
        }
        if input.eof || CANCELLED.load(Ordering::Relaxed) {
            return Err("request stream ended before a complete request".into());
        }
        poll_fds(&[libc::STDIN_FILENO], POLL_MS)?;
    };
    request.validate()?;
    let payload = match &request.stdin {
        Some(descriptor) => Some(input.payload(descriptor.byte_len, opened)?),
        None => None,
    };
    request.validate_stdin(payload.as_deref())?;
    let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
    let request_sha256 = request.digest()?;
    let stdout_capture = OutputCapture::new(request.stdout_bytes)?;
    let stderr_capture = OutputCapture::new(request.stderr_bytes)?;
    send(&ServerMessage::Ready {
        protocol: request.protocol,
        invocation_id: request.invocation_id.clone(),
        request_sha256: request_sha256.clone(),
        supervisor_version: env!("CARGO_PKG_VERSION").into(),
    })?;

    let mut before_dispatch = ProcessOutcome::Cancelled;
    let dispatch = loop {
        if Instant::now() >= deadline {
            before_dispatch = ProcessOutcome::TimedOut;
            break false;
        }
        if CANCELLED.load(Ordering::Relaxed) {
            break false;
        }
        if let Some(frame) = input.frame(MAX_CONTROL_BYTES)? {
            match serde_json::from_slice::<Control>(&frame)
                .map_err(|error| format!("invalid control: {error}"))?
            {
                Control::Dispatch => break true,
                Control::Cancel => break false,
            }
        }
        if input.eof {
            break false;
        }
        poll_fds(&[libc::STDIN_FILENO], POLL_MS)?;
    };
    let terminal = if dispatch {
        execute(
            &request,
            options,
            payload.as_deref(),
            &mut input,
            deadline,
            stdout_capture,
            stderr_capture,
        )
    } else {
        Terminal {
            outcome: before_dispatch,
            primary_exit: None,
            launched: false,
            stdout: stdout_capture.finish(true),
            stderr: stderr_capture.finish(true),
            quiescent: true,
        }
    };
    send(&ServerMessage::Finished {
        protocol: request.protocol,
        invocation_id: request.invocation_id,
        request_sha256,
        outcome: terminal.outcome,
        primary_exit: terminal.primary_exit,
        launched: terminal.launched,
        stdout: terminal.stdout,
        stderr: terminal.stderr,
        quiescent: terminal.quiescent,
    })
}

fn establish_supervision() -> Result<(), String> {
    // SAFETY: each prctl uses the documented scalar arguments, and this
    // dedicated single-threaded process has not spawned children yet.
    unsafe {
        if libc::prctl(
            libc::PR_SET_CHILD_SUBREAPER,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
        {
            return Err(format!(
                "setting child subreaper: {}",
                io::Error::last_os_error()
            ));
        }
        if libc::prctl(
            libc::PR_SET_DUMPABLE,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
        {
            return Err(format!(
                "protecting supervisor descriptors: {}",
                io::Error::last_os_error()
            ));
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_sigaction = libc::SIG_DFL;
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(format!(
                "restoring child wait semantics: {}",
                io::Error::last_os_error()
            ));
        }
        action.sa_sigaction = libc::SIG_IGN;
        if libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut()) != 0 {
            return Err(format!(
                "protecting control output: {}",
                io::Error::last_os_error()
            ));
        }
        action.sa_sigaction = cancellation_signal as *const () as usize;
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(format!(
                    "installing cancellation handler: {}",
                    io::Error::last_os_error()
                ));
            }
        }
        let mut unblocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblocked);
        if libc::sigprocmask(libc::SIG_SETMASK, &unblocked, std::ptr::null_mut()) != 0 {
            return Err(format!(
                "unblocking supervision signals: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

struct Terminal {
    outcome: ProcessOutcome,
    primary_exit: Option<PrimaryExit>,
    launched: bool,
    stdout: CapturedOutput,
    stderr: CapturedOutput,
    quiescent: bool,
}

fn execute(
    request: &ExecRequest,
    options: ServeOptions,
    payload: Option<&[u8]>,
    input: &mut Input,
    deadline: Instant,
    mut stdout_capture: OutputCapture,
    mut stderr_capture: OutputCapture,
) -> Terminal {
    if Instant::now() >= deadline || CANCELLED.load(Ordering::Relaxed) {
        return Terminal {
            outcome: if Instant::now() >= deadline {
                ProcessOutcome::TimedOut
            } else {
                ProcessOutcome::Cancelled
            },
            primary_exit: None,
            launched: false,
            stdout: stdout_capture.finish(true),
            stderr: stderr_capture.finish(true),
            quiescent: true,
        };
    }
    // Built before fork: the child only applies the prepared ruleset between
    // fork and exec, so the supervisor itself is never restricted.
    let restriction = match request.write_restriction.as_ref().map(landlock::prepare) {
        Some(Ok(ruleset)) => Some(ruleset),
        Some(Err(message)) => {
            return Terminal {
                outcome: ProcessOutcome::LaunchFailed {
                    message: bounded_error(format!("write restriction unavailable: {message}")),
                },
                primary_exit: None,
                launched: false,
                stdout: stdout_capture.finish(true),
                stderr: stderr_capture.finish(true),
                quiescent: true,
            };
        }
        None => None,
    };
    // Also built before fork: the child only installs it.
    let filter = options.harden.then(crate::harden::Filter::native);
    let mut command = Command::new(&request.argv[0]);
    if restriction.is_some() || filter.is_some() {
        let fd = restriction.as_ref().map(landlock::Ruleset::fd);
        // SAFETY: the closure only makes async-signal-safe system calls on an
        // already open descriptor (which outlives the spawn below) and on the
        // filter it owns, and allocates nothing.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                if let Some(fd) = fd {
                    landlock::restrict_self(fd)?;
                }
                if let Some(filter) = &filter {
                    filter.install()?;
                }
                Ok(())
            });
        }
    }
    let spawned = command
        .args(&request.argv[1..])
        .stdin(if payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    drop(restriction);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            // std::process::Command settles its failed exec child. Confirm the
            // same no-children condition instead of deriving it from the error.
            let mut primary = None;
            let quiescent = reap(-1, &mut primary).unwrap_or(false);
            return Terminal {
                outcome: ProcessOutcome::LaunchFailed {
                    message: bounded_error(error.to_string()),
                },
                primary_exit: None,
                launched: false,
                stdout: stdout_capture.finish(true),
                stderr: stderr_capture.finish(true),
                quiescent,
            };
        }
    };
    let primary_pid = child.id() as libc::pid_t;
    // Drop never waits: only reap() below owns waitpid for this process and
    // subsequently adopted descendants. Pipes remain owned until collection.
    let mut child_input = child.stdin.take();
    let mut input_written = 0;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    drop(child);
    let mut primary_exit = None;
    let mut interruption = None;
    let mut cleanup_started = None;
    let mut quiescent_at = None;
    let mut quiescent = false;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut io_error = None;
    if let Some(bytes) = payload {
        if bytes.is_empty() {
            child_input.take(); // Explicit empty input is EOF, never a newline.
        } else if let Some(pipe) = child_input.as_ref() {
            if let Err(error) = nonblocking(pipe.as_raw_fd()) {
                io_error = Some(format!("preparing child stdin after launch: {error}"));
                child_input = None;
            }
        } else {
            io_error = Some("child stdin pipe is missing after launch".into());
        }
    }
    if let Some(pipe) = stdout.as_ref() {
        if let Err(error) = nonblocking(pipe.as_raw_fd()) {
            io_error = Some(error);
            stdout = None;
        }
    }
    if let Some(pipe) = stderr.as_ref() {
        if let Err(error) = nonblocking(pipe.as_raw_fd()) {
            io_error = Some(error);
            stderr = None;
        }
    }
    if stdout.is_none() || stderr.is_none() {
        io_error = Some("child capture pipe is missing".into());
    }

    loop {
        let was_quiescent = quiescent;
        if interruption.is_none() {
            if let Some(bytes) = payload {
                if let Err(error) = deliver_input(&mut child_input, bytes, &mut input_written) {
                    io_error.get_or_insert(error);
                }
            }
        } else {
            child_input.take();
        }
        if let Err(error) = drain_pipe(&mut stdout, &mut stdout_capture, &mut stdout_eof) {
            io_error.get_or_insert(error);
        }
        if let Err(error) = drain_pipe(&mut stderr, &mut stderr_capture, &mut stderr_eof) {
            io_error.get_or_insert(error);
        }
        match reap(primary_pid, &mut primary_exit) {
            Ok(true) => {
                quiescent = true;
                quiescent_at.get_or_insert_with(Instant::now);
            }
            Ok(false) => {}
            Err(error) => {
                io_error.get_or_insert(error);
            }
        }
        // A final poll may end after the deadline with an already waitable
        // child. ECHILD proves absence now, not that the child exited on time.
        // Preserve that exit separately and conservatively latch the deadline.
        // Output drain alone must not time out previously proved quiescence.
        if !was_quiescent && interruption.is_none() {
            if CANCELLED.load(Ordering::Relaxed) {
                interruption = Some(ProcessOutcome::Cancelled);
            } else if Instant::now() >= deadline {
                interruption = Some(ProcessOutcome::TimedOut);
            }
        }
        if quiescent && payload.is_some_and(|bytes| input_written != bytes.len()) {
            io_error.get_or_insert_with(|| {
                format!(
                    "child exited after accepting {input_written} of {} stdin bytes",
                    payload.expect("checked payload").len()
                )
            });
            child_input.take();
        }
        if quiescent && stdout_eof && stderr_eof {
            break;
        }
        if quiescent_at.is_some_and(|time| time.elapsed() >= DRAIN_TIMEOUT) {
            // EOF may be unavailable, but owned-child quiescence was already
            // proved. Retain incomplete stream observations rather than hang.
            break;
        }
        if !quiescent && interruption.is_none() {
            if let Some(error) = io_error.take() {
                interruption = Some(ProcessOutcome::Failed {
                    message: bounded_error(error),
                });
            } else if CANCELLED.load(Ordering::Relaxed) {
                interruption = Some(ProcessOutcome::Cancelled);
            } else if Instant::now() >= deadline {
                interruption = Some(ProcessOutcome::TimedOut);
            } else {
                match input.frame(MAX_CONTROL_BYTES) {
                    Ok(Some(frame)) => match serde_json::from_slice::<Control>(&frame) {
                        Ok(Control::Cancel) => interruption = Some(ProcessOutcome::Cancelled),
                        Ok(Control::Dispatch) => {
                            interruption = Some(ProcessOutcome::Failed {
                                message: "duplicate dispatch control".into(),
                            })
                        }
                        Err(error) => {
                            interruption = Some(ProcessOutcome::Failed {
                                message: bounded_error(format!("invalid control: {error}")),
                            })
                        }
                    },
                    Ok(None) if input.eof => interruption = Some(ProcessOutcome::Cancelled),
                    Ok(None) => {}
                    Err(error) => {
                        interruption = Some(ProcessOutcome::Failed {
                            message: bounded_error(error),
                        })
                    }
                }
            }
        }
        if !quiescent && interruption.is_some() {
            let cleanup = cleanup_started.get_or_insert_with(Instant::now);
            if cleanup.elapsed() >= Duration::from_millis(CLEANUP_TIMEOUT_MS) {
                break;
            }
            let signal = if cleanup.elapsed() >= TERM_GRACE {
                libc::SIGKILL
            } else {
                libc::SIGTERM
            };
            // No wait happens between discovery and signalling. Every listed
            // child remains ours, including a zombie, until this sole reaper
            // waits for it; its PID therefore cannot be recycled in this gap.
            if let Err(error) = signal_children(signal) {
                io_error.get_or_insert(error);
            }
        }
        let mut fds = Vec::with_capacity(3);
        if !input.eof && interruption.is_none() {
            fds.push(libc::STDIN_FILENO);
        }
        if let Some(pipe) = stdout.as_ref() {
            fds.push(pipe.as_raw_fd());
        }
        if let Some(pipe) = stderr.as_ref() {
            fds.push(pipe.as_raw_fd());
        }
        if let Err(error) =
            poll_execution_fds(&fds, child_input.as_ref().map(AsRawFd::as_raw_fd), POLL_MS)
        {
            io_error.get_or_insert(error);
        }
    }
    // A pipe error remains incomplete even if another descriptor reached EOF.
    let outcome = interruption
        .or_else(|| {
            io_error.map(|message| ProcessOutcome::Failed {
                message: bounded_error(message),
            })
        })
        .unwrap_or_else(|| match &primary_exit {
            Some(PrimaryExit::Exited { code }) => ProcessOutcome::Exited { code: *code },
            Some(PrimaryExit::Signalled { signal }) => {
                ProcessOutcome::Signalled { signal: *signal }
            }
            None => ProcessOutcome::Failed {
                message: "primary process status was not observed".into(),
            },
        });
    Terminal {
        outcome,
        primary_exit,
        launched: true,
        stdout: stdout_capture.finish(stdout_eof),
        stderr: stderr_capture.finish(stderr_eof),
        quiescent,
    }
}

/// Bounded progress per pass keeps output draining, cancellation and child
/// reaping live even if the command never reads or writes before reading stdin.
fn deliver_input<T: Write + AsRawFd>(
    pipe: &mut Option<T>,
    bytes: &[u8],
    written: &mut usize,
) -> Result<(), String> {
    let Some(writer) = pipe.as_mut() else {
        return Ok(());
    };
    for _ in 0..8 {
        if *written == bytes.len() {
            *pipe = None;
            return Ok(());
        }
        let end = written.saturating_add(8192).min(bytes.len());
        match writer.write(&bytes[*written..end]) {
            Ok(0) => {
                *pipe = None;
                return Err(format!(
                    "child stdin closed after accepting {written} of {} bytes",
                    bytes.len()
                ));
            }
            Ok(amount) => *written += amount,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                *pipe = None;
                return Err(format!(
                    "delivering child stdin failed after accepting {written} of {} bytes: {error}",
                    bytes.len()
                ));
            }
        }
    }
    if *written == bytes.len() {
        *pipe = None;
    }
    Ok(())
}

/// true only for ECHILD after reaping every currently waitable child, including
/// clone children. A 0 return means that a live child still belongs to us.
fn reap(primary: libc::pid_t, primary_exit: &mut Option<PrimaryExit>) -> Result<bool, String> {
    // A command continuously spawning short-lived children must not monopolize
    // the waiter and prevent the outer loop from enforcing its deadline.
    for _ in 0..256 {
        let mut status = 0;
        // SAFETY: status points to a valid int and this helper alone reaps its
        // children; __WALL includes non-SIGCHLD clone children on Linux.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
        if pid > 0 {
            if pid == primary {
                if libc::WIFEXITED(status) {
                    *primary_exit = Some(PrimaryExit::Exited {
                        code: libc::WEXITSTATUS(status),
                    });
                } else if libc::WIFSIGNALED(status) {
                    *primary_exit = Some(PrimaryExit::Signalled {
                        signal: libc::WTERMSIG(status),
                    });
                }
            }
            continue;
        }
        if pid == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ECHILD) => return Ok(true),
            Some(libc::EINTR) => continue,
            _ => return Err(format!("waiting for descendants: {error}")),
        }
    }
    Ok(false)
}

fn signal_children(signal: libc::c_int) -> Result<(), String> {
    // SAFETY: gettid has no pointer arguments and returns this calling thread.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/task/{tid}/children"))
        .map_err(|error| format!("opening owned child list: {error}"))?
        .take(CHILD_LIST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading owned child list: {error}"))?;
    if bytes.len() as u64 > CHILD_LIST_BYTES {
        return Err("owned child list exceeds bound".into());
    }
    let list = std::str::from_utf8(&bytes).map_err(|_| "invalid owned child list")?;
    for token in list.split_ascii_whitespace() {
        let pid: libc::pid_t = token.parse().map_err(|_| "invalid owned child pid")?;
        if pid <= 1 || i64::from(pid) == tid {
            return Err("invalid owned child identity".into());
        }
        // SAFETY: positive PIDs from this thread's own child list only. No
        // negative process-group or all-process selector can reach this call.
        if unsafe { libc::kill(pid, signal) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("signalling owned child: {error}"));
            }
        }
    }
    Ok(())
}

fn drain_pipe<T: Read + AsRawFd>(
    pipe: &mut Option<T>,
    capture: &mut OutputCapture,
    eof: &mut bool,
) -> Result<(), String> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    // Bound each pass so continuously writing children cannot starve deadline,
    // cancellation, reaping, or the other output stream.
    let mut bytes = [0_u8; 8192];
    for _ in 0..8 {
        match reader.read(&mut bytes) {
            Ok(0) => {
                *eof = true;
                *pipe = None;
                break;
            }
            Ok(count) => {
                if let Err(error) = capture.observe(&bytes[..count]) {
                    *pipe = None;
                    return Err(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                *pipe = None;
                return Err(format!("reading child output: {error}"));
            }
        }
    }
    Ok(())
}

struct Input {
    buffer: Vec<u8>,
    eof: bool,
}
impl Input {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            eof: false,
        }
    }
    fn payload(&mut self, length: usize, opened: Instant) -> Result<Vec<u8>, String> {
        if length > crate::protocol::MAX_STDIN_BYTES {
            return Err("stdin payload exceeds bound".into());
        }
        let mut payload = Vec::with_capacity(length);
        let buffered = self.buffer.len().min(length);
        payload.extend(self.buffer.drain(..buffered));
        while payload.len() < length {
            if opened.elapsed() >= INITIAL_REQUEST_TIMEOUT || CANCELLED.load(Ordering::Relaxed) {
                return Err(
                    "stdin payload preparation timed out or was cancelled before dispatch".into(),
                );
            }
            if self.eof {
                return Err("stdin payload ended before its declared length".into());
            }
            let mut bytes = [0u8; 8192];
            let amount = bytes.len().min(length - payload.len());
            // SAFETY: bytes owns amount writable bytes; stdin is our control fd.
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), amount) };
            if count > 0 {
                payload.extend_from_slice(&bytes[..count as usize]);
                continue;
            }
            if count == 0 {
                self.eof = true;
                continue;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => poll_fds(&[libc::STDIN_FILENO], POLL_MS)?,
                io::ErrorKind::Interrupted => {}
                _ => return Err(format!("reading stdin payload before dispatch: {error}")),
            }
        }
        Ok(payload)
    }

    fn frame(&mut self, limit: usize) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                if end >= limit {
                    return Err("control input exceeds bound".into());
                }
                let mut rest = self.buffer.split_off(end + 1);
                std::mem::swap(&mut rest, &mut self.buffer);
                rest.truncate(end);
                return Ok(Some(rest));
            }
            if self.buffer.len() >= limit {
                return Err("control input exceeds bound".into());
            }
            if self.eof {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err("incomplete control frame".into())
                };
            }
            let mut bytes = [0_u8; 4096];
            let amount = bytes.len().min(limit.saturating_sub(self.buffer.len()));
            // SAFETY: bytes contains amount writable bytes and stdin is open.
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), amount) };
            if count > 0 {
                self.buffer.extend_from_slice(&bytes[..count as usize]);
                continue;
            }
            if count == 0 {
                self.eof = true;
                continue;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => return Ok(None),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(format!("reading control: {error}")),
            }
        }
    }
}

fn nonblocking(fd: RawFd) -> Result<(), String> {
    // SAFETY: fcntl only inspects/modifies flags on a descriptor owned here.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(format!(
                "setting nonblocking descriptor: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn poll_fds(fds: &[RawFd], timeout: i32) -> Result<(), String> {
    poll_execution_fds(fds, None, timeout)
}

fn poll_execution_fds(fds: &[RawFd], writable: Option<RawFd>, timeout: i32) -> Result<(), String> {
    let mut entries = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect::<Vec<_>>();
    if let Some(fd) = writable {
        entries.push(libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        });
    }
    // SAFETY: entries owns its complete pollfd allocation for this call.
    let result =
        unsafe { libc::poll(entries.as_mut_ptr(), entries.len() as libc::nfds_t, timeout) };
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        return Err(format!(
            "polling process descriptors: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn send(message: &ServerMessage) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|error| format!("encoding control output: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("control output exceeds bound".into());
    }
    let started = Instant::now();
    let mut position = 0;
    while position < bytes.len() {
        if started.elapsed() >= WRITE_TIMEOUT {
            return Err("control output acknowledgement timed out".into());
        }
        // SAFETY: the slice remains alive and readable until write returns.
        let count = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                bytes[position..].as_ptr().cast(),
                bytes.len() - position,
            )
        };
        if count > 0 {
            position += count as usize;
            continue;
        }
        if count == 0 {
            return Err("control output made no progress".into());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("writing control output: {error}"));
        }
        let mut entry = libc::pollfd {
            fd: libc::STDOUT_FILENO,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: entry is valid for the one-element poll array.
        unsafe {
            libc::poll(&mut entry, 1, POLL_MS);
        }
    }
    Ok(())
}

fn bounded_error(message: String) -> String {
    bounded_message(message)
}

/// Kernel write restriction for a launched command tree (Landlock). Writes
/// are refused everywhere except beneath the allowed roots and, when the
/// restriction denies the network, so is every TCP bind and connect; reading
/// and execution stay unrestricted. Inherited by every descendant.
mod landlock {
    use crate::protocol::WriteRestriction;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::RawFd;

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;
    const WRITE_FILE: u64 = 1 << 1;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    const NET_BIND_TCP: u64 = 1 << 0;
    const NET_CONNECT_TCP: u64 = 1 << 1;
    const SYS_CREATE_RULESET: libc::c_long = 444;
    const SYS_ADD_RULE: libc::c_long = 445;
    const SYS_RESTRICT_SELF: libc::c_long = 446;

    /// The ABI 4 layout. An older kernel accepts it while the network
    /// field is zero, and a nonzero one is only sent from ABI 4 on.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    pub(super) struct Ruleset(RawFd);

    impl Ruleset {
        pub(super) fn fd(&self) -> RawFd {
            self.0
        }
    }

    impl Drop for Ruleset {
        fn drop(&mut self) {
            // SAFETY: this descriptor is owned by the ruleset and closed once.
            unsafe {
                libc::close(self.0);
            }
        }
    }

    fn last_error(what: &str) -> String {
        format!("{what}: {}", io::Error::last_os_error())
    }

    /// The rights a ruleset handles, each refused unless a rule allows it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Handled {
        fs: u64,
        net: u64,
    }

    /// The rights a ruleset handles on a kernel offering Landlock `abi`.
    /// Before ABI 3 Landlock cannot refuse truncating an existing file,
    /// which would let a read-only command empty any repository file, so
    /// such a kernel offers no write restriction at all. Before ABI 4
    /// (Linux 6.7) it cannot refuse TCP, so a restriction that also denies
    /// the network is unavailable there. TCP is handled with no allowed
    /// port, so every bind and connect is refused.
    pub(super) fn handled_access(abi: i64, deny_network: bool) -> Result<Handled, String> {
        if abi < 3 {
            return Err(format!(
                "Landlock ABI {abi} cannot refuse truncating files; version 3 (Linux 6.2) or \
                 later is required"
            ));
        }
        if deny_network && abi < 4 {
            return Err(format!(
                "Landlock ABI {abi} cannot refuse TCP connections; version 4 (Linux 6.7) or \
                 later is required"
            ));
        }
        Ok(Handled {
            fs: WRITE_FILE
                | REMOVE_DIR
                | REMOVE_FILE
                | MAKE_CHAR
                | MAKE_DIR
                | MAKE_REG
                | MAKE_SOCK
                | MAKE_FIFO
                | MAKE_BLOCK
                | MAKE_SYM
                | REFER
                | TRUNCATE,
            net: if deny_network {
                NET_BIND_TCP | NET_CONNECT_TCP
            } else {
                0
            },
        })
    }

    /// Create the ruleset in the supervisor. Fails when the kernel or the
    /// container's seccomp policy does not offer Landlock, or offers only a
    /// version that cannot refuse every write or, when the restriction
    /// denies the network, every TCP bind and connect.
    pub(super) fn prepare(restriction: &WriteRestriction) -> Result<Ruleset, String> {
        restriction.validate()?;
        // SAFETY: querying the ABI takes no attribute pointer.
        let abi = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            return Err(last_error("Landlock is not available"));
        }
        let handled = handled_access(abi, restriction.deny_network)?;
        let attr = RulesetAttr {
            handled_access_fs: handled.fs,
            handled_access_net: handled.net,
        };
        // SAFETY: attr is a valid, correctly sized ruleset attribute.
        let fd = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(last_error("creating the Landlock ruleset"));
        }
        let ruleset = Ruleset(fd as RawFd);
        let home = std::env::var("HOME").ok();
        for root in restriction.effective_writable(home.as_deref()) {
            let path =
                CString::new(root.as_str()).map_err(|_| "invalid writable path".to_owned())?;
            // SAFETY: path is NUL-terminated; O_PATH opens without access.
            let parent = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if parent < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                    continue;
                }
                return Err(last_error(&format!("opening {root}")));
            }
            // SAFETY: parent is an open descriptor; stat is fully initialised by fstat.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let is_dir = unsafe { libc::fstat(parent, &mut stat) } == 0
                && (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR;
            // A file accepts only file rights; a directory accepts all.
            let allowed = if is_dir {
                handled.fs
            } else {
                handled.fs & (WRITE_FILE | TRUNCATE)
            };
            let rule = PathBeneathAttr {
                allowed_access: allowed,
                parent_fd: parent,
            };
            // SAFETY: rule is a valid path-beneath attribute for this ruleset.
            let added = unsafe {
                libc::syscall(
                    SYS_ADD_RULE,
                    ruleset.fd(),
                    RULE_PATH_BENEATH,
                    &rule as *const PathBeneathAttr,
                    0u32,
                )
            };
            // SAFETY: parent was opened above and is no longer needed.
            unsafe {
                libc::close(parent);
            }
            if added != 0 {
                return Err(last_error(&format!("allowing writes beneath {root}")));
            }
        }
        Ok(ruleset)
    }

    /// Enforce the prepared ruleset on the calling (child) process.
    pub(super) fn restrict_self(fd: RawFd) -> io::Result<()> {
        // SAFETY: both are plain system calls with scalar arguments.
        unsafe {
            if libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(SYS_RESTRICT_SELF, fd, 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_kernel_that_cannot_refuse_truncation_offers_no_restriction() {
            for abi in [1, 2] {
                for deny_network in [false, true] {
                    let refused = handled_access(abi, deny_network).unwrap_err();
                    assert!(refused.contains("truncating"), "{refused}");
                }
            }
            for abi in [3, 4, 6] {
                let handled = handled_access(abi, false).unwrap();
                assert_eq!(handled.fs & TRUNCATE, TRUNCATE);
                assert_eq!(handled.fs & REFER, REFER);
                assert_eq!(handled.fs & WRITE_FILE, WRITE_FILE);
                assert_eq!(handled.net, 0);
            }
        }

        /// A read-only shell's restriction denies the network. A kernel that
        /// cannot refuse TCP offers no such restriction, so the command is
        /// not launched; it never runs with only its writes restricted.
        #[test]
        fn a_kernel_that_cannot_refuse_tcp_offers_no_network_restriction() {
            let refused = handled_access(3, true).unwrap_err();
            assert!(refused.contains("TCP"), "{refused}");
            assert!(refused.contains("Linux 6.7"), "{refused}");
            for abi in [4, 5, 6, 7] {
                let denied = handled_access(abi, true).unwrap();
                assert_eq!(denied.net, NET_BIND_TCP | NET_CONNECT_TCP);
                let open = handled_access(abi, false).unwrap();
                assert_eq!(open.net, 0);
                assert_eq!(denied.fs, open.fs);
            }
        }
    }
}
