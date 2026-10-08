//! Linux-only, single-threaded descendant supervision. No caller-visible exit
//! status or pipe EOF substitutes for the kernel's final `ECHILD` observation.

use crate::protocol::{
    bounded_message, CapturedOutput, Control, ExecRequest, HelperView, OutputCapture, PrimaryExit,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeOptions {
    /// `--harden`: the command and its descendants run in a Landlock domain
    /// (the write restriction's, a read-only helper's, or one that only
    /// refuses creating block devices), so they get no ptrace access to
    /// processes they did not start, and get `PR_SET_NO_NEW_PRIVS` and the
    /// seccomp denylist of [`crate::harden`] before `execve`. Without
    /// Landlock nothing launches.
    pub harden: bool,
    /// With `harden`: launch the command as a read-only helper through its
    /// view of the Workspace ([`HelperView`], see [`helper_view`]). The
    /// supervisor must start as root; the command never runs as root.
    pub helper: Option<HelperView>,
}

impl ServeOptions {
    /// A helper's launch is always hardened: without `harden` its command
    /// would get neither its view nor the helper's seccomp filter, so the
    /// supervisor refuses before reading a request.
    pub fn validate(&self) -> Result<(), String> {
        if self.helper.is_some() && !self.harden {
            return Err("a read-only helper's launch needs --harden".into());
        }
        Ok(())
    }
}

/// Run exactly one request. This must be called by the dedicated helper binary,
/// never inside a multithreaded host: the helper is the only child reaper.
pub fn serve() -> Result<(), String> {
    serve_with(ServeOptions::default())
}

/// [`serve`] with options.
pub fn serve_with(options: ServeOptions) -> Result<(), String> {
    options.validate()?;
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
            &options,
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
    options: &ServeOptions,
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
    // fork and exec, so the supervisor itself is never restricted. A hardened
    // command always gets one, because its Landlock domain is what keeps it
    // out of processes it did not start (ptrace access, which also guards
    // /proc/<pid>/mem, environ, maps and fd). A helper's also confines what
    // it reads (see [`helper_view`]).
    let prepared = match (
        options.helper.as_ref().filter(|_| options.harden),
        request.write_restriction.as_ref(),
        options.harden,
    ) {
        (Some(view), restriction, _) => helper_view::prepare(view, restriction)
            .map(|(ruleset, launch)| (Some(ruleset), Some(launch))),
        (None, Some(restriction), _) => landlock::prepare(restriction)
            .map(|ruleset| (Some(ruleset), None))
            .map_err(|message| format!("write restriction unavailable: {message}")),
        (None, None, true) => landlock::prepare_domain()
            .map(|ruleset| (Some(ruleset), None))
            .map_err(|message| format!("hardening unavailable: {message}")),
        (None, None, false) => Ok((None, None)),
    };
    let (restriction, helper) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            return Terminal {
                outcome: ProcessOutcome::LaunchFailed {
                    message: bounded_error(message),
                },
                primary_exit: None,
                launched: false,
                stdout: stdout_capture.finish(true),
                stderr: stderr_capture.finish(true),
                quiescent: true,
            };
        }
    };
    // Also built before fork: the child only installs it.
    let filter = options
        .harden
        .then(|| match (&helper, &request.write_restriction) {
            (Some(_), Some(_)) => crate::harden::Filter::helper(),
            (Some(_), None) => crate::harden::Filter::helper_file_tools(),
            (None, _) => crate::harden::Filter::native(),
        });
    let mut command = Command::new(&request.argv[0]);
    if let Some(launch) = &helper {
        match &launch.scratch {
            // The shell's own home and temporary directory: nothing it
            // writes there reaches another process, and nothing of
            // another's is in it.
            Some(scratch) => {
                command
                    .env("HOME", scratch.path())
                    .env("TMPDIR", scratch.path());
            }
            // The file tools write nothing.
            None => {
                command
                    .env("HOME", helper_view::NO_HOME)
                    .env_remove("TMPDIR");
            }
        }
    }
    if restriction.is_some() || filter.is_some() {
        let fd = restriction.as_ref().map(landlock::Ruleset::fd);
        let identity = helper.as_ref().map(|launch| launch.identity);
        // SAFETY: the closure only makes async-signal-safe system calls on an
        // already open descriptor (which outlives the spawn below), on plain
        // values and on the filter it owns, and allocates nothing.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                if let Some(identity) = identity {
                    identity.assume()?;
                }
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

/// A read-only helper's launch (`--helper`): the supervisor starts as root
/// and launches the command as the helper user, with `CAP_DAC_READ_SEARCH`
/// as its only capability (ambient, so its descendants keep it; every other
/// capability leaves its bounding set), in a Landlock domain that handles
/// every write right (Landlock ABI 3), opening files to read or execute them
/// and listing directories.
///
/// The capability lets the helper read the Workspace whatever its file modes
/// (a `mkdtemp` repository is `0700`, its files `0600`), which the writer
/// owns. Landlock grants reading, listing and executing beneath the
/// Workspace, and elsewhere only what any user may read when the command
/// starts, in the system directories ([`SYSTEM_ROOTS`], walked at each
/// launch; see `Walk`) and a few single files (devices, the kernel's global
/// `/proc` files). So the helper reads nothing outside the Workspace that it
/// could not read before: not the writer's home, anything in `/tmp`,
/// `/var/tmp`, `/dev/shm`, `/home`, `/root` or `/run`, `/etc/shadow`, or any
/// process's `/proc/<pid>` (its own included).
///
/// It writes almost nowhere. The file tools (no write restriction) may only
/// open `/dev/null` for writing ([`TOOL_DEVICES`]) and get no scratch
/// directory. The shell (a write restriction, whose `writable` roots a
/// helper does not get) may write only beneath its own scratch directory
/// (its `HOME` and `TMPDIR`, made for the command and removed when it ends)
/// and open a few devices for writing ([`SHELL_DEVICES`]).
///
/// Landlock covers neither passing through a directory nor connecting to a
/// Unix socket, watching a path or changing its extended attributes, which
/// the capability would extend to directories the helper could not enter
/// before, nor System V IPC or POSIX message queues; the helper's seccomp
/// filter refuses Unix sockets, `inotify` and `fanotify`, extended
/// attribute changes and every IPC call ([`crate::harden`]), and the file
/// tools' also refuses changing a file's mode, owner or times and opening
/// any socket. The Workspace must be a directory named without a final
/// symbolic link. What
/// remains is the metadata (`stat`, `readlink`, reading extended attributes)
/// of a path the helper names in such a directory. Reading another
/// process's environment or memory still needs ptrace access, which neither
/// its user nor its domain has.
mod helper_view {
    use super::landlock::{
        self, Fd, Handled, Ruleset, EXECUTE, READ, READ_DIR, READ_FILE, WRITE_FILE,
    };
    use crate::protocol::{HelperView, WriteRestriction};
    use std::ffi::{CStr, CString, OsStr};
    use std::io;
    use std::os::fd::RawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    const CAP_DAC_READ_SEARCH: libc::c_ulong = 2;
    const CAP_SETGID: u32 = 6;
    const CAP_SETUID: u32 = 7;
    const CAP_SETPCAP: u32 = 8;
    const CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    /// `SECBIT_NOROOT`, `SECBIT_NO_SETUID_FIXUP` and both their locks.
    const SECURE_BITS: libc::c_ulong = 0b1111;

    /// System directories a helper may read where any user may.
    pub(super) const SYSTEM_ROOTS: &[&str] = &[
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/opt",
        "/etc",
        "/proc/sys",
        "/sys/devices/system/cpu",
    ];

    /// Single files a helper may read where any user may: devices, and the
    /// kernel's global `/proc` files (no process's own).
    pub(super) const SYSTEM_FILES: &[&str] = &[
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/stat",
        "/proc/loadavg",
        "/proc/uptime",
        "/proc/version",
        "/proc/filesystems",
    ];

    /// The devices a helper's shell may open for writing, besides its scratch
    /// directory: nothing written to them is kept or reaches another process.
    pub(super) const SHELL_DEVICES: &[&str] =
        &["/dev/null", "/dev/zero", "/dev/tty", "/dev/urandom"];

    /// The one device a helper's file tools may open for writing, which
    /// discards what they drain there.
    pub(super) const TOOL_DEVICES: &[&str] = &["/dev/null"];

    /// `HOME` for a helper's file tools, which get no scratch directory: a
    /// path that does not exist, as for a system user without a home.
    pub(super) const NO_HOME: &str = "/nonexistent";

    /// How many entries of the system directories are inspected at most.
    const WALK_LIMIT: usize = 1_000_000;
    /// How deep beneath a system directory the walk goes; anything deeper
    /// gets no rule.
    const DEPTH_LIMIT: usize = 48;

    /// What a helper's launch keeps until the command ends.
    pub(super) struct Launch {
        /// The shell's own home and temporary directory; the file tools get
        /// none.
        pub(super) scratch: Option<Scratch>,
        pub(super) identity: Identity,
    }

    fn unavailable(message: impl std::fmt::Display) -> String {
        format!("helper view unavailable: {message}")
    }

    /// Check that this supervisor can launch the helper, make its scratch
    /// directory and build its ruleset. A refusal names what is missing.
    /// The ruleset handles reading and every write right, for the file tools
    /// (no write restriction) as for the shell (one, which when it denies
    /// the network also handles TCP). The file tools may write nothing but
    /// [`TOOL_DEVICES`]; the shell only beneath its scratch directory and
    /// [`SHELL_DEVICES`], never the restriction's own `writable` roots.
    pub(super) fn prepare(
        view: &HelperView,
        restriction: Option<&WriteRestriction>,
    ) -> Result<(Ruleset, Launch), String> {
        view.validate().map_err(unavailable)?;
        let identity = Identity::check(view.helper)?;
        let abi = landlock::abi().map_err(|message| {
            unavailable(format!(
                "{message}; a read-only helper's view needs Landlock ABI 3 (Linux 6.2 or later)"
            ))
        })?;
        let handled = handled(abi, restriction)?;
        let scratch = match restriction {
            Some(_) => Some(Scratch::create(view.helper).map_err(|error| {
                unavailable(format!("creating its scratch directory in /tmp: {error}"))
            })?),
            None => None,
        };
        if let Some(scratch) = &scratch {
            if within(scratch.path_str(), &view.workspace) {
                return Err(unavailable(format!(
                    "its scratch directory {} would be inside the Workspace {}",
                    scratch.path_str(),
                    view.workspace
                )));
            }
        }
        let ruleset = landlock::create(handled).map_err(unavailable)?;
        allow_workspace(&ruleset, &view.workspace).map_err(unavailable)?;
        let devices = match &scratch {
            Some(scratch) => {
                landlock::allow_path(&ruleset, scratch.path_str(), handled.fs, 0)
                    .map_err(unavailable)?;
                SHELL_DEVICES
            }
            None => TOOL_DEVICES,
        };
        for device in devices {
            allow_device_writes(&ruleset, device).map_err(unavailable)?;
        }
        let mut walk = Walk {
            ruleset: &ruleset,
            writer: view.writer.0,
            seen: 0,
        };
        for file in SYSTEM_FILES {
            walk.root(file).map_err(unavailable)?;
        }
        for root in SYSTEM_ROOTS {
            walk.root(root).map_err(unavailable)?;
        }
        Ok((ruleset, Launch { scratch, identity }))
    }

    /// What a helper's ruleset handles on a kernel offering Landlock `abi`:
    /// reading and every write right, and for a shell whose restriction
    /// denies the network also TCP. A kernel that cannot refuse all of it
    /// gets no launch: the file tools need ABI 3 (Linux 6.2), the shell
    /// ABI 4 (Linux 6.7). The shell's refusal starts as the write
    /// restriction's does, so the daemon tells it apart.
    pub(super) fn handled(
        abi: i64,
        restriction: Option<&WriteRestriction>,
    ) -> Result<Handled, String> {
        let writes = match restriction {
            Some(restriction) => restriction
                .validate()
                .and_then(|()| landlock::handled_access(abi, restriction.deny_network))
                .map_err(|message| format!("write restriction unavailable: {message}"))?,
            None => landlock::handled_access(abi, false).map_err(|message| {
                unavailable(format!(
                    "{message}; a read-only helper's view refuses every write with it"
                ))
            })?,
        };
        Ok(Handled {
            fs: writes.fs | READ,
            net: writes.net,
        })
    }

    /// Whether `inner` is `outer` or beneath it.
    pub(super) fn within(inner: &str, outer: &str) -> bool {
        let outer = outer.trim_end_matches('/');
        outer.is_empty() || inner == outer || inner.starts_with(&format!("{outer}/"))
    }

    /// Let the command read, list and execute beneath the Workspace. It must
    /// be a directory, named without a final symbolic link: a link would
    /// open whatever it points to, whose owner's modes the capability then
    /// passes over.
    fn allow_workspace(ruleset: &Ruleset, path: &str) -> Result<(), String> {
        let name = CString::new(path).map_err(|_| format!("invalid path {path}"))?;
        // SAFETY: name is NUL-terminated; O_PATH opens without access, and
        // O_NOFOLLOW opens a final link itself rather than its target.
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Err(format!("the Workspace {path} does not exist"));
            }
            return Err(format!("opening the Workspace {path}: {error}"));
        }
        let fd = Fd(fd);
        let stat = landlock::fstat(fd.0).map_err(|error| format!("{path}: {error}"))?;
        if landlock::kind(&stat) != libc::S_IFDIR {
            return Err(format!(
                "the Workspace {path} is not a directory (a symbolic link is refused)"
            ));
        }
        landlock::add_rule(ruleset, fd.0, READ)
            .map_err(|error| format!("allowing reads beneath the Workspace {path}: {error}"))
    }

    /// Let the command open the character device at `path` for writing (no
    /// other write right: a device is neither created nor truncated). A
    /// path that is missing or not a character device gets no rule.
    fn allow_device_writes(ruleset: &Ruleset, path: &str) -> Result<(), String> {
        let name = CString::new(path).map_err(|_| format!("invalid path {path}"))?;
        // SAFETY: name is NUL-terminated; O_PATH opens without access.
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Ok(());
        }
        let fd = Fd(fd);
        let stat = landlock::fstat(fd.0).map_err(|error| format!("{path}: {error}"))?;
        if landlock::kind(&stat) != libc::S_IFCHR {
            return Ok(());
        }
        landlock::add_rule(ruleset, fd.0, WRITE_FILE)
            .map_err(|error| format!("allowing writes to {path}: {error}"))
    }

    /// Whether any user may read this file now.
    pub(super) fn readable_file(stat: &libc::stat) -> bool {
        stat.st_mode & libc::S_IROTH != 0
    }

    /// Whether any user may pass through this directory now.
    pub(super) fn passable(stat: &libc::stat) -> bool {
        landlock::kind(stat) == libc::S_IFDIR && stat.st_mode & libc::S_IXOTH != 0
    }

    /// Whether any user may list this directory now.
    pub(super) fn listable(stat: &libc::stat) -> bool {
        passable(stat) && stat.st_mode & libc::S_IROTH != 0
    }

    /// Whether nobody but its owner, which is not `writer`, may add, remove
    /// or rename its entries: what appears in it later is its owner's
    /// (root's, as a rule), never the writer's.
    pub(super) fn sealed(stat: &libc::stat, writer: u32) -> bool {
        stat.st_uid != writer && stat.st_mode & (libc::S_IWGRP | libc::S_IWOTH) == 0
    }

    /// The rights a rule on an entry of this kind grants.
    fn rights(stat: &libc::stat) -> u64 {
        match landlock::kind(stat) {
            libc::S_IFLNK => 0,
            libc::S_IFDIR => READ,
            _ => EXECUTE | READ_FILE,
        }
    }

    /// How much of an entry the helper may read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Reach {
        /// All of it, now and later: one rule on it covers it.
        Whole,
        /// Parts of it, whose rules are already added.
        Part,
        /// None of it.
        Nothing,
    }

    /// What the walk found beneath an entry.
    #[derive(Debug, Clone, Copy)]
    struct Seen {
        reach: Reach,
        /// Some directory in it (itself included) is one other users may not
        /// list or pass through.
        private: bool,
    }

    impl Seen {
        fn leaf(reach: Reach) -> Self {
            Self {
                reach,
                private: false,
            }
        }

        /// `stat` when the entry may be read whole.
        fn then_whole(self, stat: libc::stat) -> Option<libc::stat> {
            (self.reach == Reach::Whole).then_some(stat)
        }
    }

    /// Adds the rules for what any user may read in the system directories.
    ///
    /// A directory any user may list, in which nobody but root (or another
    /// owner that is not the writer) can add entries, and everything beneath
    /// which any user may read, gets one rule for all of it. Otherwise each
    /// such entry in it does, every file any user may read gets one of its
    /// own, and the directory itself, when no directory beneath it is private
    /// to its owner, gets one that only lists it: what the writer (or anyone)
    /// adds there later is listed but never opened. What gets a rule is
    /// judged when the command starts; a file whose owner later makes it
    /// private keeps its rule while the command runs.
    struct Walk<'a> {
        ruleset: &'a Ruleset,
        writer: u32,
        seen: usize,
    }

    impl Walk<'_> {
        /// Add the rules for `path`, when it exists and every directory
        /// above it is one any user may pass through.
        fn root(&mut self, path: &str) -> Result<(), String> {
            let mut above = PathBuf::from("/");
            let parent = Path::new(path).parent().unwrap_or(Path::new("/"));
            for part in parent.components().skip(1) {
                above.push(part);
                match stat_path(&above) {
                    Ok(stat) if passable(&stat) => {}
                    _ => return Ok(()),
                }
            }
            let name = CString::new(path).map_err(|_| format!("invalid path {path}"))?;
            // SAFETY: name is NUL-terminated; O_PATH opens without access.
            let fd = unsafe {
                libc::open(
                    name.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Ok(());
            }
            let fd = Fd(fd);
            let stat = landlock::fstat(fd.0).map_err(|error| format!("{path}: {error}"))?;
            if self.entry(&fd, &stat, 0)?.reach == Reach::Whole {
                landlock::add_rule(self.ruleset, fd.0, rights(&stat))
                    .map_err(|error| format!("allowing reads beneath {path}: {error}"))?;
            }
            Ok(())
        }

        fn entry(&mut self, fd: &Fd, stat: &libc::stat, depth: usize) -> Result<Seen, String> {
            if landlock::kind(stat) == libc::S_IFDIR {
                self.count()?;
                return self.directory(fd, stat, depth);
            }
            self.leaf(stat)
        }

        /// Anything but a directory.
        fn leaf(&mut self, stat: &libc::stat) -> Result<Seen, String> {
            self.count()?;
            Ok(Seen::leaf(match landlock::kind(stat) {
                // A link opens nothing itself; its target is checked when
                // opened.
                libc::S_IFLNK => Reach::Whole,
                _ if readable_file(stat) => Reach::Whole,
                _ => Reach::Nothing,
            }))
        }

        fn count(&mut self) -> Result<(), String> {
            self.seen += 1;
            if self.seen > WALK_LIMIT {
                return Err(format!(
                    "the system directories hold more than {WALK_LIMIT} entries"
                ));
            }
            Ok(())
        }

        fn directory(&mut self, fd: &Fd, stat: &libc::stat, depth: usize) -> Result<Seen, String> {
            let private = Seen {
                reach: Reach::Nothing,
                private: true,
            };
            if !passable(stat) || depth >= DEPTH_LIMIT {
                return Ok(private);
            }
            // SAFETY: "." is NUL-terminated and fd an O_PATH directory.
            let listed = unsafe {
                libc::openat(
                    fd.0,
                    c".".as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if listed < 0 {
                return Ok(private);
            }
            let listed = Fd(listed);
            let names = entry_names(listed.0)?;
            let mut whole = Vec::new();
            let mut all = true;
            let mut private_below = !listable(stat);
            for name in names {
                // Only a directory is opened to be walked; anything else is
                // judged by its status, and opened again (and checked to be
                // the same) only if it gets a rule of its own.
                let seen = match stat_at(listed.0, &name) {
                    Ok(child_stat) if landlock::kind(&child_stat) == libc::S_IFDIR => {
                        match landlock::open_entry(listed.0, &name) {
                            Ok((child, child_stat)) => {
                                let seen = self.entry(&child, &child_stat, depth + 1)?;
                                Some((seen, child_stat))
                            }
                            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
                            Err(_) => None,
                        }
                    }
                    Ok(child_stat) => Some((self.leaf(&child_stat)?, child_stat)),
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
                    Err(_) => None,
                };
                let Some((seen, child_stat)) = seen else {
                    all = false;
                    private_below = true;
                    continue;
                };
                private_below |= seen.private;
                match seen.then_whole(child_stat) {
                    Some(child_stat) => whole.push((name, child_stat.st_dev, child_stat.st_ino)),
                    None => all = false,
                }
            }
            if all && listable(stat) && sealed(stat, self.writer) {
                return Ok(Seen {
                    reach: Reach::Whole,
                    private: false,
                });
            }
            // Only parts: a rule on each entry that may be read whole, if
            // it is still the entry that was inspected.
            for (name, device, inode) in whole {
                let Ok((child, child_stat)) = landlock::open_entry(listed.0, &name) else {
                    continue;
                };
                if child_stat.st_dev != device
                    || child_stat.st_ino != inode
                    || (landlock::kind(&child_stat) != libc::S_IFDIR
                        && landlock::kind(&child_stat) != libc::S_IFLNK
                        && !readable_file(&child_stat))
                {
                    continue;
                }
                landlock::add_rule(self.ruleset, child.0, rights(&child_stat)).map_err(
                    |error| {
                        format!(
                            "allowing reads of {}: {error}",
                            String::from_utf8_lossy(name.to_bytes())
                        )
                    },
                )?;
            }
            // Listing it, and the directories beneath, which any user may
            // list now, opens nothing in it.
            if !private_below {
                landlock::add_rule(self.ruleset, fd.0, READ_DIR).map_err(|error| {
                    format!("allowing a system directory to be listed: {error}")
                })?;
            }
            Ok(Seen {
                reach: Reach::Part,
                private: private_below,
            })
        }
    }

    /// The status of `name` in the directory open at `dir`, without
    /// following a final link.
    fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
        // SAFETY: stat is fully initialised by a successful fstatat; name is
        // NUL-terminated and dir an open directory.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    fn stat_path(path: &Path) -> io::Result<libc::stat> {
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: stat is fully initialised by a successful lstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::lstat(name.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    /// The names in the directory open at `fd`, without `.` and `..`.
    fn entry_names(fd: RawFd) -> Result<Vec<CString>, String> {
        let mut names = Vec::new();
        let mut buffer = vec![0u8; 32 * 1024];
        loop {
            // SAFETY: buffer is writable for its length; fd is a directory.
            let read = unsafe {
                libc::syscall(libc::SYS_getdents64, fd, buffer.as_mut_ptr(), buffer.len())
            };
            if read < 0 {
                return Err(format!(
                    "listing a system directory: {}",
                    io::Error::last_os_error()
                ));
            }
            if read == 0 {
                return Ok(names);
            }
            let mut offset = 0usize;
            while offset < read as usize {
                // struct linux_dirent64: d_ino u64, d_off i64, d_reclen u16,
                // d_type u8, d_name (NUL-terminated).
                let record = &buffer[offset..read as usize];
                if record.len() < 19 {
                    return Err("malformed directory entry".into());
                }
                let length = u16::from_ne_bytes([record[16], record[17]]) as usize;
                if length < 19 || length > record.len() {
                    return Err("malformed directory entry".into());
                }
                let name = CStr::from_bytes_until_nul(&record[19..length])
                    .map_err(|_| "malformed directory entry name".to_string())?;
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    names.push(name.to_owned());
                }
                offset += length;
            }
        }
    }

    /// The helper's own home and temporary directory, `0700` and the
    /// helper's, removed (without following links) when it is dropped.
    pub(super) struct Scratch {
        path: PathBuf,
        text: String,
    }

    impl Scratch {
        fn create(owner: (u32, u32)) -> io::Result<Self> {
            let mut template = *b"/tmp/axocoatl-helper.XXXXXX\0";
            // SAFETY: template is a writable NUL-terminated mkdtemp pattern.
            if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
                return Err(io::Error::last_os_error());
            }
            let bytes = &template[..template.len() - 1];
            let scratch = Self {
                path: PathBuf::from(OsStr::from_bytes(bytes)),
                text: String::from_utf8_lossy(bytes).into_owned(),
            };
            let name = CString::new(bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
            // SAFETY: name is NUL-terminated; the directory is opened, not
            // followed through a link, and changed by descriptor.
            let fd = unsafe {
                libc::open(
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = Fd(fd);
            // SAFETY: fd is the directory just made.
            if unsafe { libc::fchown(fd.0, owner.0, owner.1) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(scratch)
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        fn path_str(&self) -> &str {
            &self.text
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The user and group the helper's command assumes.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct Identity {
        uid: u32,
        gid: u32,
    }

    impl Identity {
        /// Whether this supervisor (root) can launch the command as
        /// `helper` with only `CAP_DAC_READ_SEARCH`.
        fn check(helper: (u32, u32)) -> Result<Self, String> {
            // SAFETY: geteuid has no arguments.
            if unsafe { libc::geteuid() } != 0 {
                return Err(unavailable(
                    "the supervisor must start as root to launch a read-only helper",
                ));
            }
            let permitted = permitted_capabilities()
                .map_err(|error| unavailable(format!("reading its capabilities: {error}")))?;
            for (cap, name) in [
                (CAP_SETUID, "CAP_SETUID"),
                (CAP_SETGID, "CAP_SETGID"),
                (CAP_SETPCAP, "CAP_SETPCAP"),
                (CAP_DAC_READ_SEARCH as u32, "CAP_DAC_READ_SEARCH"),
            ] {
                if permitted & (1 << cap) == 0 {
                    return Err(unavailable(format!(
                        "the container gives root no {name}; a hardened Session container \
                         made before Axocoatl 1.3.0 lacks CAP_DAC_READ_SEARCH: restart the \
                         Session"
                    )));
                }
            }
            Ok(Self {
                uid: helper.0,
                gid: helper.1,
            })
        }

        /// Become the helper, between fork and exec: no supplementary
        /// groups, its group and user, and `CAP_DAC_READ_SEARCH` as the only
        /// capability in every set, ambient included, so the command and its
        /// descendants keep it and can never gain another. Its locked
        /// secure bits keep a set-user-ID-root program from granting any
        /// (`SECBIT_NOROOT`, besides no-new-privileges), and keep the kernel
        /// from setting aside the capability when the command asks whether
        /// it may read a path (`access`, as Git does for a repository's
        /// directories; `SECBIT_NO_SETUID_FIXUP`), so the answer is what an
        /// `open` would get. First the command's own standard pipes, which
        /// this supervisor (root) made for it, become the helper's, so that
        /// it may open them again by path (`/dev/stdout`, `/dev/stderr`).
        /// Raw system calls only: they act on this one thread, allocate
        /// nothing and are async-signal-safe.
        pub(super) fn assume(self) -> io::Result<()> {
            let fail = || Err(io::Error::last_os_error());
            // SAFETY: plain system calls with scalar arguments, or pointers
            // to complete local structures, in the single-threaded child.
            unsafe {
                for fd in 0..3 {
                    let mut stat: libc::stat = std::mem::zeroed();
                    if libc::fstat(fd, &mut stat) == 0
                        && stat.st_mode & libc::S_IFMT == libc::S_IFIFO
                        && stat.st_uid == 0
                        && libc::fchown(fd, self.uid, self.gid) != 0
                    {
                        return fail();
                    }
                }
                for cap in 0..64 as libc::c_ulong {
                    if cap != CAP_DAC_READ_SEARCH
                        && libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0
                        && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL)
                    {
                        return fail();
                    }
                }
                // With no set-user-ID fixup the change of user below keeps
                // the capabilities, which the capset after it reduces.
                if libc::prctl(libc::PR_SET_SECUREBITS, SECURE_BITS, 0, 0, 0) != 0
                    || libc::syscall(libc::SYS_setgroups, 0, std::ptr::null::<libc::gid_t>()) != 0
                    || libc::syscall(libc::SYS_setresgid, self.gid, self.gid, self.gid) != 0
                    || libc::syscall(libc::SYS_setresuid, self.uid, self.uid, self.uid) != 0
                {
                    return fail();
                }
                let header = CapHeader {
                    version: CAPABILITY_VERSION_3,
                    pid: 0,
                };
                let only = 1u32 << CAP_DAC_READ_SEARCH;
                let data = [
                    CapData {
                        effective: only,
                        permitted: only,
                        inheritable: only,
                    },
                    CapData {
                        effective: 0,
                        permitted: 0,
                        inheritable: 0,
                    },
                ];
                if libc::syscall(libc::SYS_capset, &header, data.as_ptr()) != 0
                    || libc::prctl(
                        libc::PR_CAP_AMBIENT,
                        libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong,
                        CAP_DAC_READ_SEARCH,
                        0,
                        0,
                    ) != 0
                {
                    return fail();
                }
            }
            Ok(())
        }
    }

    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    /// This process's permitted capabilities.
    fn permitted_capabilities() -> io::Result<u64> {
        let mut header = CapHeader {
            version: CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [
            CapData {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
            CapData {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        ];
        // SAFETY: header and data are complete version 3 structures.
        if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(data[0].permitted) | (u64::from(data[1].permitted) << 32))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn stat(kind: libc::mode_t, mode: libc::mode_t, uid: u32) -> libc::stat {
            // SAFETY: an all-zero stat is a valid value.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            stat.st_mode = kind | mode;
            stat.st_uid = uid;
            stat
        }

        /// A rule outside the Workspace covers what any user may read when
        /// the command starts; one rule covers a directory only when nobody
        /// but root (or another user that is not the writer) can add to it.
        #[test]
        fn only_what_any_user_may_read_is_opened_and_only_sealed_directories_whole() {
            let writer = 1000;
            assert!(readable_file(&stat(libc::S_IFREG, 0o644, 0)));
            assert!(readable_file(&stat(libc::S_IFREG, 0o604, writer)));
            assert!(!readable_file(&stat(libc::S_IFREG, 0o640, 0)));
            assert!(!readable_file(&stat(libc::S_IFREG, 0o600, 0)));
            for mode in [0o755, 0o555, 0o751] {
                assert!(passable(&stat(libc::S_IFDIR, mode, 0)), "{mode:o}");
            }
            assert!(passable(&stat(libc::S_IFDIR, 0o711, 0)));
            assert!(!listable(&stat(libc::S_IFDIR, 0o711, 0)));
            assert!(!passable(&stat(libc::S_IFDIR, 0o700, 0)));
            assert!(!passable(&stat(libc::S_IFDIR, 0o754, 0)));
            assert!(!passable(&stat(libc::S_IFREG, 0o755, 0)));
            assert!(listable(&stat(libc::S_IFDIR, 0o755, writer)));
            assert!(listable(&stat(libc::S_IFDIR, 0o1777, 0)));
            assert!(sealed(&stat(libc::S_IFDIR, 0o755, 0), writer));
            assert!(sealed(&stat(libc::S_IFDIR, 0o755, 503), writer));
            for mode in [0o775, 0o757, 0o777, 0o1777, 0o2775] {
                assert!(!sealed(&stat(libc::S_IFDIR, mode, 0), writer), "{mode:o}");
            }
            assert!(!sealed(&stat(libc::S_IFDIR, 0o755, writer), writer));
            assert_eq!(rights(&stat(libc::S_IFDIR, 0o755, 0)), READ);
            assert_eq!(rights(&stat(libc::S_IFREG, 0o755, 0)), EXECUTE | READ_FILE);
            assert_eq!(rights(&stat(libc::S_IFLNK, 0o777, 0)), 0);
        }

        /// The shell writes no directory but its scratch directory, and only
        /// these devices; the file tools only `/dev/null`.
        #[test]
        fn a_helper_may_write_only_discarding_devices() {
            assert_eq!(
                SHELL_DEVICES,
                ["/dev/null", "/dev/zero", "/dev/tty", "/dev/urandom"]
            );
            assert_eq!(TOOL_DEVICES, ["/dev/null"]);
            for device in SHELL_DEVICES.iter().chain(TOOL_DEVICES) {
                assert!(device.starts_with("/dev/") && !device[5..].contains('/'));
                assert!(!device.starts_with("/dev/shm"), "{device}");
            }
            assert!(within("/tmp/axocoatl-helper.x", "/tmp"));
            assert!(within("/tmp/axocoatl-helper.x", "/tmp/"));
            assert!(within("/tmp", "/tmp"));
            assert!(within("/tmp/x", "/"));
            assert!(!within("/tmp/axocoatl-helper.x", "/tmp/axo"));
            assert!(!within("/tmp", "/tmp/axocoatl-helper.x"));
        }

        /// A kernel whose Landlock cannot refuse every write gets no helper
        /// at all, and one that cannot refuse TCP no helper shell; the rest
        /// handle reading and every write right, and the shell TCP too.
        #[test]
        fn a_kernel_without_the_needed_landlock_abi_launches_no_helper() {
            let shell = WriteRestriction {
                writable: vec!["/tmp".into()],
                protected: vec!["/work/repo".into()],
                deny_network: true,
            };
            for abi in [1, 2] {
                let refused = handled(abi, None).unwrap_err();
                assert!(
                    refused.starts_with("helper view unavailable") && refused.contains("Linux 6.2"),
                    "{refused}"
                );
                let refused = handled(abi, Some(&shell)).unwrap_err();
                assert!(
                    refused.starts_with("write restriction unavailable"),
                    "{refused}"
                );
            }
            let refused = handled(3, Some(&shell)).unwrap_err();
            assert!(
                refused.starts_with("write restriction unavailable")
                    && refused.contains("Linux 6.7"),
                "{refused}"
            );
            for abi in [3, 4, 5, 6, 7] {
                let tools = handled(abi, None).unwrap();
                let writes = landlock::handled_access(abi, false).unwrap();
                assert_eq!(tools.fs, writes.fs | READ);
                assert_eq!(tools.fs & WRITE_FILE, WRITE_FILE);
                assert_eq!(tools.net, 0);
            }
            for abi in [4, 5, 6, 7] {
                let restricted = handled(abi, Some(&shell)).unwrap();
                assert_eq!(restricted.fs, handled(abi, None).unwrap().fs);
                assert_eq!(
                    restricted.net,
                    landlock::handled_access(abi, true).unwrap().net
                );
                assert_ne!(restricted.net, 0);
            }
        }

        /// A helper's launch without `--harden` is refused before any
        /// request is read, never run without its view.
        #[test]
        fn a_helper_launch_is_always_hardened() {
            let view = HelperView {
                helper: (1001, 1001),
                writer: (1000, 1000),
                workspace: "/work/repo".into(),
            };
            let unhardened = crate::supervisor::ServeOptions {
                harden: false,
                helper: Some(view.clone()),
            };
            assert!(unhardened.validate().unwrap_err().contains("--harden"));
            for options in [
                crate::supervisor::ServeOptions {
                    harden: true,
                    helper: Some(view),
                },
                crate::supervisor::ServeOptions {
                    harden: true,
                    helper: None,
                },
                crate::supervisor::ServeOptions::default(),
            ] {
                assert!(options.validate().is_ok(), "{options:?}");
            }
        }

        /// No process's own `/proc/<pid>`, `/proc/self` included, is among
        /// what a helper may read outside the Workspace, and neither is any
        /// home, `/tmp`, `/run` or `/root`.
        #[test]
        fn the_system_paths_hold_no_process_home_or_shared_directory() {
            for path in SYSTEM_ROOTS.iter().chain(SYSTEM_FILES) {
                assert!(path.starts_with('/'), "{path}");
                for refused in [
                    "/proc/self",
                    "/home",
                    "/root",
                    "/tmp",
                    "/var",
                    "/run",
                    "/dev/shm",
                ] {
                    assert!(!path.starts_with(refused), "{path}");
                }
                if let Some(rest) = path.strip_prefix("/proc/") {
                    assert!(!rest.starts_with(|c: char| c.is_ascii_digit()), "{path}");
                    assert!(!rest.contains('/') || rest.starts_with("sys"), "{path}");
                }
            }
        }
    }
}

/// Kernel write restriction for a launched command tree (Landlock). Writes
/// are refused everywhere except beneath the allowed roots and, when the
/// restriction denies the network, so is every TCP bind and connect; reading
/// and execution stay unrestricted, except for a read-only helper's command
/// ([`super::helper_view`]). Inherited by every descendant.
mod landlock {
    use crate::protocol::WriteRestriction;
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::fd::RawFd;

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;
    pub(super) const EXECUTE: u64 = 1 << 0;
    pub(super) const WRITE_FILE: u64 = 1 << 1;
    pub(super) const READ_FILE: u64 = 1 << 2;
    pub(super) const READ_DIR: u64 = 1 << 3;
    /// Opening a file to read it, listing a directory, executing a file.
    pub(super) const READ: u64 = EXECUTE | READ_FILE | READ_DIR;
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
        pub(super) fs: u64,
        pub(super) net: u64,
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

    /// What a hardened command's own domain handles when it has no write
    /// restriction: creating block devices, which it then cannot do anywhere,
    /// and nothing else. The domain exists for Landlock's ptrace rule: a
    /// process in a domain gets no ptrace access to a process outside it
    /// (tracing, or reading `/proc/<pid>/mem`, `environ`, `maps` or `fd`),
    /// while the processes it starts share its domain and stay reachable.
    /// That rule holds from ABI 1 (Linux 5.13).
    pub(super) const DOMAIN_ONLY: Handled = Handled {
        fs: MAKE_BLOCK,
        net: 0,
    };

    /// The kernel's Landlock ABI. Fails when the kernel or the container's
    /// seccomp policy does not offer Landlock.
    pub(super) fn abi() -> Result<i64, String> {
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
        Ok(abi)
    }

    /// A ruleset that handles `handled` and allows nothing yet.
    pub(super) fn create(handled: Handled) -> Result<Ruleset, String> {
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
        Ok(Ruleset(fd as RawFd))
    }

    /// The ruleset of a hardened command without a write restriction
    /// ([`DOMAIN_ONLY`]). Fails without Landlock, so such a command is
    /// never launched outside a domain.
    pub(super) fn prepare_domain() -> Result<Ruleset, String> {
        abi()?;
        create(DOMAIN_ONLY)
    }

    /// Create the ruleset in the supervisor. Fails when the kernel or the
    /// container's seccomp policy does not offer Landlock, or offers only a
    /// version that cannot refuse every write or, when the restriction
    /// denies the network, every TCP bind and connect.
    pub(super) fn prepare(restriction: &WriteRestriction) -> Result<Ruleset, String> {
        restriction.validate()?;
        let handled = handled_access(abi()?, restriction.deny_network)?;
        let ruleset = create(handled)?;
        let home = std::env::var("HOME").ok();
        allow_writes(&ruleset, restriction, home.as_deref(), handled.fs)?;
        Ok(ruleset)
    }

    /// Allow `rights` (the handled write rights) beneath each of the
    /// restriction's writable roots that exists, `$HOME` being `home`.
    fn allow_writes(
        ruleset: &Ruleset,
        restriction: &WriteRestriction,
        home: Option<&str>,
        rights: u64,
    ) -> Result<(), String> {
        for root in restriction.effective_writable(home) {
            // A file accepts only file rights; a directory accepts all.
            allow_path(ruleset, &root, rights, rights & (WRITE_FILE | TRUNCATE))?;
        }
        Ok(())
    }

    /// Allow `dir_rights` beneath `path` when it is a directory, or
    /// `file_rights` on it otherwise. `false` when it does not exist.
    pub(super) fn allow_path(
        ruleset: &Ruleset,
        path: &str,
        dir_rights: u64,
        file_rights: u64,
    ) -> Result<bool, String> {
        let name = CString::new(path).map_err(|_| "invalid path".to_owned())?;
        // SAFETY: name is NUL-terminated; O_PATH opens without access.
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(last_error(&format!("opening {path}")));
        }
        let fd = Fd(fd);
        let is_dir = fstat(fd.0).is_ok_and(|stat| kind(&stat) == libc::S_IFDIR);
        add_rule(ruleset, fd.0, if is_dir { dir_rights } else { file_rights })
            .map_err(|error| format!("allowing access beneath {path}: {error}"))?;
        Ok(true)
    }

    /// Add one path-beneath rule on the open descriptor `fd`. Nothing to do
    /// for no rights.
    pub(super) fn add_rule(ruleset: &Ruleset, fd: RawFd, rights: u64) -> io::Result<()> {
        if rights == 0 {
            return Ok(());
        }
        let rule = PathBeneathAttr {
            allowed_access: rights,
            parent_fd: fd,
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
        if added != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// An owned descriptor, closed on drop.
    pub(super) struct Fd(pub(super) RawFd);

    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: the descriptor is owned here and closed once.
            unsafe {
                libc::close(self.0);
            }
        }
    }

    pub(super) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        // SAFETY: stat is fully initialised by a successful fstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    pub(super) fn kind(stat: &libc::stat) -> libc::mode_t {
        stat.st_mode & libc::S_IFMT
    }

    /// Open `name` beneath the directory `dir` without following a final
    /// symbolic link and without any access (`O_PATH`), and inspect it.
    pub(super) fn open_entry(dir: RawFd, name: &CStr) -> io::Result<(Fd, libc::stat)> {
        // SAFETY: name is NUL-terminated and dir is an open directory.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = Fd(fd);
        let stat = fstat(fd.0)?;
        Ok((fd, stat))
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

        /// A hardened command without a write restriction is put in a domain
        /// that refuses nothing it needs: only creating block devices, and
        /// no network right.
        #[test]
        fn a_hardened_domain_handles_only_block_devices() {
            assert_eq!(DOMAIN_ONLY.fs, MAKE_BLOCK);
            assert_eq!(DOMAIN_ONLY.net, 0);
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
