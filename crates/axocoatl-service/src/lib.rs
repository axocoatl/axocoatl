//! OS background-service management for the Axocoatl daemon.
//!
//! Powers `axocoatl service install/start/stop/status/uninstall`: registering
//! the daemon as a real OS service — a **systemd user unit** on Linux, a
//! **launchd user agent** on macOS — so it runs continuously and survives
//! reboots. This is the "Always-On Service" mode: it is about keeping the
//! daemon *process* alive, distinct from "Proactive Agents" (agents that act
//! on their own while the daemon runs).

use std::path::Path;

#[cfg(target_os = "linux")]
mod systemd;

#[cfg(target_os = "macos")]
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
    /// `<exe> serve --config <config>`. Both paths must be absolute.
    fn install(&self, exe: &Path, config: &Path) -> Result<(), ServiceError>;

    /// Stop, deregister, and remove the service definition.
    fn uninstall(&self) -> Result<(), ServiceError>;

    /// Start the service now and enable it for login/boot.
    fn start(&self) -> Result<(), ServiceError>;

    /// Stop the running service.
    fn stop(&self) -> Result<(), ServiceError>;

    /// Report the current service state.
    fn status(&self) -> Result<ServiceStatus, ServiceError>;

    /// A one-line hint to show the user after `install` (e.g. enabling
    /// systemd linger), or `None` when nothing extra is needed.
    fn post_install_hint(&self) -> Option<String> {
        None
    }
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
