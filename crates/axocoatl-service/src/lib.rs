//! OS background-service management for the Axocoatl daemon.
//!
//! Powers `axocoatl service install/start/restart/stop/status/uninstall`: registering
//! the daemon as a real OS service — a **systemd user unit** on Linux, a
//! **launchd user agent** on macOS — so it runs continuously and survives
//! reboots. This is the "Always-On Service" mode: it is about keeping the
//! daemon *process* alive, distinct from "Proactive Agents" (agents that act
//! on their own while the daemon runs).

use std::path::Path;
use std::time::{Duration, Instant};

mod environment;
pub use environment::{ServiceEnvironment, CARRIED_VARIABLES, SYSTEM_PATH};

// Each backend's manager runs only on its own platform, but both definitions
// are rendered and tested on every platform.
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod systemd;

#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod launchd;

/// Logical service name — the systemd unit basename.
pub const SERVICE_NAME: &str = "axocoatl";

/// Errors from service management.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("background-service management is not supported on this platform")]
    Unsupported,
    #[error("service control command failed: {0}")]
    Control(String),
    #[error("could not locate the home directory")]
    NoHome,
}

/// What [`ServiceManager::start`] or [`ServiceManager::restart`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStart {
    /// The service was not running and now runs its installed definition.
    Started,
    /// `start`: the service already ran, and was left as it was (it keeps
    /// the definition it was started with).
    AlreadyRunning,
    /// `restart`: the service ran, was stopped, and now runs its installed
    /// definition.
    Restarted,
}

/// A snapshot of the installed service's state.
#[derive(Debug, Clone)]
pub struct ServiceStatus {
    /// The service unit/plist is written and registered.
    pub installed: bool,
    /// The daemon process is currently running under the service.
    pub running: bool,
    /// The service is set to start automatically at login / boot.
    pub enabled: bool,
    /// Human-readable detail (PID, backend, last error).
    pub detail: String,
}

/// Manages the Axocoatl daemon as an OS background service.
pub trait ServiceManager {
    /// The platform service backend — `"systemd"` or `"launchd"`.
    fn backend(&self) -> &'static str;

    /// Write and register the service so it runs
    /// `<exe> serve --config <config>` with `environment`. Both paths must be
    /// absolute.
    fn install(
        &self,
        exe: &Path,
        config: &Path,
        environment: &ServiceEnvironment,
    ) -> Result<(), ServiceError>;

    /// Stop, deregister, and remove the service definition.
    fn uninstall(&self) -> Result<(), ServiceError>;

    /// Start the service now and enable it for login/boot. A service that
    /// already runs is left running ([`ServiceStart::AlreadyRunning`]).
    fn start(&self) -> Result<ServiceStart, ServiceError>;

    /// Stop the service when it runs and start it again with the installed
    /// definition (what `install` wrote), enabled for login/boot; start it
    /// when it does not run.
    fn restart(&self) -> Result<ServiceStart, ServiceError>;

    /// Stop the service and return once its process has exited. A service
    /// that does not run is left as it is.
    fn stop(&self) -> Result<(), ServiceError>;

    /// Report the current service state.
    fn status(&self) -> Result<ServiceStatus, ServiceError>;

    /// Where the daemon's output goes: a log file, or the command that shows
    /// the service's journal.
    fn logs(&self) -> String;

    /// A one-line hint to show the user after `install` (e.g. enabling
    /// systemd linger), or `None` when nothing extra is needed.
    fn post_install_hint(&self) -> Option<String> {
        None
    }

    /// The daemon's last start failure in its log, when its last start
    /// failed (see [`last_start_failure`]). Best effort: `None` when the log
    /// cannot be read.
    fn last_start_failure(&self) -> Option<String> {
        None
    }

    /// Where the daemon's log ends now. Taken before a start, what
    /// [`Self::log_since`] then returns is the started daemon's.
    fn log_mark(&self) -> LogMark {
        LogMark::default()
    }

    /// What the daemon logged after `mark` (its last [`LOG_TAIL_BYTES`]),
    /// or `None` when the log cannot be read.
    fn log_since(&self, _mark: &LogMark) -> Option<String> {
        None
    }

    /// Whether the daemon's process has exited since the service was last
    /// started (or restarted, or loaded): a start that failed, which the
    /// service manager starts again.
    fn exited_since_start(&self) -> bool {
        false
    }
}

/// Where a daemon's log ended before a start ([`ServiceManager::log_mark`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogMark {
    /// launchd: the log file's length.
    pub(crate) offset: u64,
    /// systemd: the cursor of the unit's last journal entry; `None` when it
    /// had none.
    pub(crate) cursor: Option<String>,
}

/// What the daemon a service start or restart started did
/// ([`wait_for_daemon`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStart {
    /// It said it started: it serves.
    Started,
    /// It exited before it started. What it said: the line it printed, or
    /// its last line when it printed none of them.
    Failed(String),
    /// It said neither within the wait.
    StillStarting,
}

/// Wait up to `timeout` for the daemon a service start began after `mark`
/// to say it started, or to fail: it said it started, or it exited, with
/// the line it logged after the mark. launchd and systemd start a failing
/// daemon again, so a start that fails never ends on its own. The last
/// such line counts, and a failure only once this start's daemon exited:
/// the daemon of the definition the start replaced may have been started
/// again, and failed, just after the mark.
pub fn wait_for_daemon(
    manager: &dyn ServiceManager,
    mark: &LogMark,
    timeout: Duration,
    poll: Duration,
) -> DaemonStart {
    let deadline = Instant::now() + timeout;
    let mut exited_at: Option<Instant> = None;
    loop {
        let log = manager.log_since(mark).unwrap_or_default();
        let last = start_lines(&log).last();
        if matches!(last, Some(StartLine::Started)) {
            return DaemonStart::Started;
        }
        let now = Instant::now();
        if exited_at.is_none() && manager.exited_since_start() {
            exited_at = Some(now);
        }
        match (exited_at, last) {
            (Some(_), Some(StartLine::Failed(line))) => return DaemonStart::Failed(line),
            // The journal may get its last lines after systemd saw it exit.
            (Some(exited), _) if now >= exited + EXITED_LOG_GRACE => {
                let last = log
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(shorten);
                return DaemonStart::Failed(
                    last.unwrap_or_else(|| "the daemon exited before it said anything".to_string()),
                );
            }
            // launchd or systemd did not say it exited.
            (None, Some(StartLine::Failed(line))) if now >= deadline => {
                return DaemonStart::Failed(line)
            }
            (None, _) if now >= deadline => return DaemonStart::StillStarting,
            _ => {}
        }
        std::thread::sleep(poll);
    }
}

/// How long [`wait_for_daemon`] still reads the log after the daemon exited.
const EXITED_LOG_GRACE: Duration = Duration::from_millis(if cfg!(test) { 200 } else { 2000 });

/// What `serve` prints, at the start of a line, when it exits before it
/// starts: bootstrap failed, the configuration is invalid, another daemon
/// holds the IPC socket, or the port cannot be listened on.
const START_FAILURES: [&str; 5] = [
    "Failed to bootstrap daemon: ",
    "Configuration error:",
    "Failed to reserve the CLI IPC socket: ",
    "✗ Port ",
    "✗ Axocoatl cannot listen on ",
];
/// What `serve` prints once bootstrap succeeded and the port is held.
const STARTED: &str = "Axocoatl server starting on ";
/// How much of the end of a log is read, and how long a reported line may be.
pub(crate) const LOG_TAIL_BYTES: u64 = 64 * 1024;
const FAILURE_LINE_CHARS: usize = 1000;

/// A line in which `serve` says how its start ended.
enum StartLine {
    Started,
    /// The failure line (with the next line when it ends with `:`, as the
    /// configuration error does).
    Failed(String),
}

/// The lines of `log` that say how a start ended, in order.
fn start_lines(log: &str) -> impl Iterator<Item = StartLine> + '_ {
    let mut lines = log.lines().map(str::trim).peekable();
    std::iter::from_fn(move || {
        while let Some(line) = lines.next() {
            if line.starts_with(STARTED) {
                return Some(StartLine::Started);
            }
            if START_FAILURES.iter().any(|prefix| line.starts_with(prefix)) {
                let mut text = line.to_string();
                if line.ends_with(':') {
                    if let Some(next) = lines.peek().filter(|next| !next.is_empty()) {
                        text.push(' ');
                        text.push_str(next);
                    }
                }
                return Some(StartLine::Failed(shorten(&text)));
            }
        }
        None
    })
}

/// `line`, cut at [`FAILURE_LINE_CHARS`].
fn shorten(line: &str) -> String {
    let mut shown: String = line.chars().take(FAILURE_LINE_CHARS).collect();
    if shown.len() < line.len() {
        shown.push('…');
    }
    shown
}

/// The last line in `log` that says a start failed (`Failed to bootstrap
/// daemon: …`, a configuration error, a port in use), when no successful
/// start was logged after it: what a service that keeps restarting and
/// failing (a launchd `KeepAlive` loop) last said.
pub fn last_start_failure(log: &str) -> Option<String> {
    match start_lines(log).last()? {
        StartLine::Failed(line) => Some(line),
        StartLine::Started => None,
    }
}

/// The last [`LOG_TAIL_BYTES`] of the file at `path`, lossily decoded.
/// Used by the launchd backend, whose log is a file.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn read_log_tail(path: &Path) -> std::io::Result<String> {
    read_log_from(path, 0)
}

/// The last [`LOG_TAIL_BYTES`] of what the file at `path` holds after
/// `offset` (of all of it when it is shorter, as after it was emptied),
/// lossily decoded: where a started daemon said last how its start ended.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn read_log_from(path: &Path, offset: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let offset = if offset > length { 0 } else { offset };
    file.seek(SeekFrom::Start(
        offset.max(length.saturating_sub(LOG_TAIL_BYTES)),
    ))?;
    let mut bytes = Vec::new();
    file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The background-service manager for the current platform.
///
/// Returns [`ServiceError::Unsupported`] on platforms without a supported
/// service system.
pub fn manager() -> Result<Box<dyn ServiceManager>, ServiceError> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(systemd::SystemdManager::resolve()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(launchd::LaunchdManager::resolve()?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ServiceError::Unsupported)
    }
}

#[cfg(all(test, unix))]
mod test_support {
    use std::path::Path;

    /// Write the program at `path` (mode 755) with `text`, in a child
    /// process, so this multi-threaded test process never holds a
    /// descriptor open for writing it: on Linux a test on another thread
    /// that forks meanwhile would pass it to its child, and running the
    /// program before that child execs fails with ETXTBSY.
    pub(crate) fn write_program(path: &Path, text: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("cat > \"$1.new\" && chmod 755 \"$1.new\" && mv -f \"$1.new\" \"$1\"")
            .arg("sh")
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "{}", path.display());
    }
}

#[cfg(test)]
mod start_failure_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const START_FAILED: &str = START_FAILURES[0];

    #[test]
    fn the_last_failure_is_reported_only_when_no_start_followed_it() {
        let failed = "Failed to bootstrap daemon: Session error: Axocoatl data directory '/d' failed data-directory format ownership admission: invalid or ambiguous format ownership: unsupported, incomplete, or differently owned manifest";
        let log = format!(
            "Axocoatl server starting on 127.0.0.1:8080\n2026-10-19T01:00:00Z  INFO stopped\n{failed}\n{failed}\n"
        );
        assert_eq!(last_start_failure(&log).as_deref(), Some(failed));
        let recovered = format!("{log}Axocoatl server starting on 127.0.0.1:8080\n");
        assert_eq!(last_start_failure(&recovered), None);
        assert_eq!(last_start_failure("Axocoatl is running.\n"), None);
        assert_eq!(last_start_failure(""), None);
        let long = format!("{START_FAILED}{}", "x".repeat(5000));
        let shown = last_start_failure(&long).unwrap();
        assert_eq!(shown.chars().count(), FAILURE_LINE_CHARS + 1);
        assert!(shown.ends_with('…'));
    }

    /// Every way `serve` exits before it starts is a failed start: an
    /// invalid configuration (its reason on the next line), a port in use,
    /// another daemon holding the IPC socket.
    #[test]
    fn every_failure_serve_prints_before_it_starts_is_a_failed_start() {
        let configuration = "Configuration error:\nagents[0].model: missing\n";
        assert_eq!(
            last_start_failure(configuration).as_deref(),
            Some("Configuration error: agents[0].model: missing")
        );
        let port = "✗ Port 8080 on 127.0.0.1 is already in use (by another program, or another Axocoatl), so Axocoatl did not start.\n  (Address already in use)\n";
        assert!(last_start_failure(port)
            .unwrap()
            .starts_with("✗ Port 8080 on 127.0.0.1 is already in use"));
        let ipc = "Failed to reserve the CLI IPC socket: busy\nAnother Axocoatl daemon may already be running.\n";
        assert_eq!(
            last_start_failure(ipc).as_deref(),
            Some("Failed to reserve the CLI IPC socket: busy")
        );
        assert_eq!(
            last_start_failure(&format!(
                "{ipc}Axocoatl server starting on 127.0.0.1:8080\n"
            )),
            None
        );
    }

    #[test]
    fn only_the_end_of_a_long_log_is_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.log");
        let mut log = format!("{START_FAILED}old\n").repeat(10_000);
        log.push_str("Axocoatl server starting on 127.0.0.1:8080\n");
        log.push_str(&"filler line\n".repeat(100));
        log.push_str(&format!("{START_FAILED}newest\n"));
        std::fs::write(&path, &log).unwrap();
        let tail = read_log_tail(&path).unwrap();
        assert_eq!(tail.len() as u64, LOG_TAIL_BYTES);
        assert_eq!(
            last_start_failure(&tail).as_deref(),
            Some(format!("{START_FAILED}newest").as_str())
        );
        // From an offset: what came after it, or its end when more came
        // after it than is read; from the start of a file emptied since.
        let offset = log.len() as u64 - 30;
        assert_eq!(
            read_log_from(&path, offset).unwrap(),
            &log[log.len() - 30..]
        );
        assert_eq!(read_log_from(&path, 4).unwrap(), tail);
        std::fs::write(&path, "fresh\n").unwrap();
        assert_eq!(read_log_from(&path, offset).unwrap(), "fresh\n");
    }

    /// A service manager whose daemon logs `lines` one more per look and
    /// exits after `exits_after` looks.
    struct Daemon {
        lines: Vec<&'static str>,
        looks: AtomicUsize,
        exits_after: Option<usize>,
        marks: Mutex<Vec<LogMark>>,
    }

    impl Daemon {
        fn new(lines: &[&'static str], exits_after: Option<usize>) -> Self {
            Self {
                lines: lines.to_vec(),
                looks: AtomicUsize::new(0),
                exits_after,
                marks: Mutex::new(Vec::new()),
            }
        }
    }

    impl ServiceManager for Daemon {
        fn backend(&self) -> &'static str {
            "test"
        }
        fn install(&self, _: &Path, _: &Path, _: &ServiceEnvironment) -> Result<(), ServiceError> {
            unreachable!()
        }
        fn uninstall(&self) -> Result<(), ServiceError> {
            unreachable!()
        }
        fn start(&self) -> Result<ServiceStart, ServiceError> {
            unreachable!()
        }
        fn restart(&self) -> Result<ServiceStart, ServiceError> {
            unreachable!()
        }
        fn stop(&self) -> Result<(), ServiceError> {
            unreachable!()
        }
        fn status(&self) -> Result<ServiceStatus, ServiceError> {
            unreachable!()
        }
        fn logs(&self) -> String {
            "daemon.log".into()
        }
        fn log_since(&self, mark: &LogMark) -> Option<String> {
            self.marks.lock().unwrap().push(mark.clone());
            let looks = self.looks.fetch_add(1, Ordering::SeqCst) + 1;
            Some(
                self.lines
                    .iter()
                    .take(looks)
                    .map(|line| format!("{line}\n"))
                    .collect(),
            )
        }
        fn exited_since_start(&self) -> bool {
            self.exits_after
                .is_some_and(|after| self.looks.load(Ordering::SeqCst) >= after)
        }
    }

    fn wait(daemon: &Daemon, timeout: Duration) -> DaemonStart {
        let mark = LogMark {
            offset: 7,
            cursor: None,
        };
        let started = wait_for_daemon(daemon, &mark, timeout, Duration::from_millis(1));
        assert!(daemon
            .marks
            .lock()
            .unwrap()
            .iter()
            .all(|seen| *seen == mark));
        started
    }

    /// The wait ends when the daemon says it started, or when it exits
    /// having said why it failed (`service start` said it started while
    /// launchd or systemd started a failing daemon again) or not (its last
    /// line); without either, at the timeout. The last line that says how a
    /// start ended counts.
    #[test]
    fn the_wait_reads_what_the_started_daemon_says() {
        let started = Daemon::new(
            &[
                "2026-10-23T01:00:00Z  INFO opening",
                "Axocoatl server starting on 127.0.0.1:8080",
            ],
            None,
        );
        assert_eq!(wait(&started, Duration::from_secs(5)), DaemonStart::Started);

        let failed =
            "Failed to bootstrap daemon: Session error: the data directory is group-writable";
        let failing = Daemon::new(&["2026-10-23T01:00:00Z  INFO opening", failed], Some(1));
        assert_eq!(
            wait(&failing, Duration::from_secs(5)),
            DaemonStart::Failed(failed.to_string())
        );

        let panicked = Daemon::new(
            &["thread 'main' panicked at src/main.rs:1:1:", "boom", ""],
            Some(1),
        );
        assert_eq!(
            wait(&panicked, Duration::from_secs(5)),
            DaemonStart::Failed("boom".to_string())
        );
        let silent = Daemon::new(&[], Some(0));
        assert_eq!(
            wait(&silent, Duration::from_secs(5)),
            DaemonStart::Failed("the daemon exited before it said anything".to_string())
        );

        // The daemon of the definition a start replaced, started again just
        // after the mark, failed; this start's daemon then started, or
        // failed too.
        let old = "Failed to bootstrap daemon: the replaced definition's";
        let replaced = Daemon::new(
            &[
                old,
                "2026-10-23T01:00:00Z  INFO opening",
                "Axocoatl server starting on 127.0.0.1:8080",
            ],
            None,
        );
        assert_eq!(
            wait(&replaced, Duration::from_secs(5)),
            DaemonStart::Started
        );
        let again = Daemon::new(&[old, failed], Some(2));
        assert_eq!(
            wait(&again, Duration::from_secs(5)),
            DaemonStart::Failed(failed.to_string())
        );
        // A failure whose exit launchd or systemd did not show is reported
        // at the end of the wait.
        let unseen = Daemon::new(&[failed], None);
        assert_eq!(
            wait(&unseen, Duration::from_millis(50)),
            DaemonStart::Failed(failed.to_string())
        );

        let slow = Daemon::new(
            &["2026-10-23T01:00:00Z  INFO upgrading Session storage"],
            None,
        );
        assert_eq!(
            wait(&slow, Duration::from_millis(50)),
            DaemonStart::StillStarting
        );
    }
}
