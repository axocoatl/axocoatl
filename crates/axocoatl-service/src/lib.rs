//! OS background-service management for the Axocoatl daemon.
//!
//! Powers `axocoatl service install/start/stop/status/uninstall`: registering
//! the daemon as a real OS service — a **systemd user unit** on Linux, a
//! **launchd user agent** on macOS — so it runs continuously and survives
//! reboots. This is the "Always-On Service" mode: it is about keeping the
//! daemon *process* alive, distinct from "Proactive Agents" (agents that act
//! on their own while the daemon runs).

use std::path::Path;

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

    /// Start the service now and enable it for login/boot.
    fn start(&self) -> Result<(), ServiceError>;

    /// Stop the running service.
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
}

/// What `serve` prints when bootstrap fails, before it exits.
const START_FAILED: &str = "Failed to bootstrap daemon: ";
/// What `serve` prints once bootstrap succeeded.
const STARTED: &str = "Axocoatl server starting on ";
/// How much of the end of a log is read, and how long a reported line may be.
pub(crate) const LOG_TAIL_BYTES: u64 = 64 * 1024;
const FAILURE_LINE_CHARS: usize = 1000;

/// The last `Failed to bootstrap daemon: …` line in `log`, when no
/// successful start was logged after it: what a service that keeps
/// restarting and failing (a launchd `KeepAlive` loop) last said.
pub fn last_start_failure(log: &str) -> Option<String> {
    let line = log
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with(START_FAILED) || line.starts_with(STARTED))?;
    if !line.starts_with(START_FAILED) {
        return None;
    }
    let mut shown: String = line.chars().take(FAILURE_LINE_CHARS).collect();
    if shown.len() < line.len() {
        shown.push('…');
    }
    Some(shown)
}

/// The last [`LOG_TAIL_BYTES`] of the file at `path`, lossily decoded.
/// Used by the launchd backend, whose log is a file.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn read_log_tail(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)))?;
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

#[cfg(test)]
mod start_failure_tests {
    use super::*;

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
    }
}
