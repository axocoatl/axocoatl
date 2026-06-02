//! Podman detection, setup, and lifecycle for session sandboxes.
//!
//! Podman is the only supported container runtime — it is rootless,
//! daemonless, and cross-platform: native on Linux/WSL, and a managed Linux VM
//! (`podman machine`) on macOS and Windows. Docker is deliberately not used.
//!
//! [`ensure_ready`] brings podman to a usable state best-effort: installing it
//! via the OS package manager if missing, and starting its VM if stopped. When
//! it cannot (locked-down host, no package manager), it returns an error
//! carrying the exact manual command for the current OS.

use tokio::process::Command;

use crate::error::IsolationError;

/// Whether podman is usable, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodmanReadiness {
    /// Podman is installed and (where applicable) its VM is running.
    Ready,
    /// Podman is not installed.
    NotInstalled,
    /// macOS/Windows: podman is installed but no `podman machine` exists yet.
    MachineMissing,
    /// macOS/Windows: a `podman machine` exists but is stopped.
    MachineStopped,
}

impl PodmanReadiness {
    /// A human-readable status line for `axocoatl doctor`.
    pub fn summary(&self) -> String {
        match self {
            PodmanReadiness::Ready => "podman ready".to_string(),
            PodmanReadiness::NotInstalled => {
                format!("podman not installed — {}", manual_install_hint())
            }
            PodmanReadiness::MachineMissing => {
                "podman installed, but no VM — run: podman machine init && podman machine start"
                    .to_string()
            }
            PodmanReadiness::MachineStopped => {
                "podman installed, VM stopped — run: podman machine start".to_string()
            }
        }
    }
}

/// True on platforms where podman runs containers inside a managed VM.
fn needs_machine() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Is `bin` on PATH and runnable?
async fn has(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `(machine exists, machine running)` — only meaningful on macOS/Windows.
async fn machine_state() -> (bool, bool) {
    let out = match Command::new("podman")
        .args(["machine", "list", "--format", "json"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return (false, false),
    };
    let machines: Vec<serde_json::Value> = serde_json::from_slice(&out).unwrap_or_default();
    let running = machines
        .iter()
        .any(|m| m.get("Running").and_then(|v| v.as_bool()).unwrap_or(false));
    (!machines.is_empty(), running)
}

/// Inspect the current podman readiness.
pub async fn detect() -> PodmanReadiness {
    if !has("podman").await {
        return PodmanReadiness::NotInstalled;
    }
    if !needs_machine() {
        // Linux / WSL — podman runs containers natively, no VM.
        return PodmanReadiness::Ready;
    }
    match machine_state().await {
        (false, _) => PodmanReadiness::MachineMissing,
        (true, false) => PodmanReadiness::MachineStopped,
        (true, true) => PodmanReadiness::Ready,
    }
}

/// Bring podman to a [`PodmanReadiness::Ready`] state, best-effort:
/// install it if missing, start its VM if stopped. Returns a precise,
/// OS-specific error if it cannot (e.g. no package manager, or the VM has
/// never been initialised — `podman machine init` downloads a VM image and is
/// left for the user to run deliberately).
pub async fn ensure_ready() -> Result<(), IsolationError> {
    match detect().await {
        PodmanReadiness::Ready => Ok(()),
        PodmanReadiness::MachineStopped => machine_start().await,
        PodmanReadiness::MachineMissing => Err(IsolationError::OciSetupFailed(
            "podman is installed but has no VM — run: podman machine init && podman machine start"
                .to_string(),
        )),
        PodmanReadiness::NotInstalled => {
            tracing::info!("podman not found — attempting an automatic install");
            install().await?;
            // Re-check: an install can succeed yet still need a VM step.
            match detect().await {
                PodmanReadiness::Ready => Ok(()),
                PodmanReadiness::MachineStopped => machine_start().await,
                PodmanReadiness::MachineMissing => Err(IsolationError::OciSetupFailed(
                    "podman installed — now run: podman machine init && podman machine start"
                        .to_string(),
                )),
                PodmanReadiness::NotInstalled => Err(IsolationError::OciSetupFailed(format!(
                    "podman could not be installed automatically. {}",
                    manual_install_hint()
                ))),
            }
        }
    }
}

/// Start an existing (stopped) `podman machine`.
async fn machine_start() -> Result<(), IsolationError> {
    tracing::info!("starting the podman machine");
    let out = Command::new("podman")
        .args(["machine", "start"])
        .output()
        .await
        .map_err(|e| IsolationError::OciSetupFailed(format!("podman machine start: {e}")))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() || stderr.contains("already running") {
        Ok(())
    } else {
        Err(IsolationError::OciSetupFailed(format!(
            "could not start the podman machine: {}",
            stderr.trim()
        )))
    }
}

/// Best-effort, OS-appropriate automatic install of podman.
async fn install() -> Result<(), IsolationError> {
    #[cfg(target_os = "linux")]
    {
        // `sudo -n` is non-interactive: it fails fast instead of hanging for a
        // password when run from a daemon with no TTY.
        let (pm, args): (&str, &[&str]) = if has("apt-get").await {
            ("apt-get", &["install", "-y", "podman"])
        } else if has("dnf").await {
            ("dnf", &["install", "-y", "podman"])
        } else if has("pacman").await {
            ("pacman", &["-S", "--noconfirm", "podman"])
        } else if has("zypper").await {
            ("zypper", &["--non-interactive", "install", "podman"])
        } else {
            return Err(IsolationError::OciSetupFailed(format!(
                "no supported package manager found. {}",
                manual_install_hint()
            )));
        };
        tracing::info!(manager = pm, "installing podman via sudo (non-interactive)");
        run_install("sudo", &[&["-n", pm], args].concat()).await
    }
    #[cfg(target_os = "macos")]
    {
        if !has("brew").await {
            return Err(IsolationError::OciSetupFailed(format!(
                "Homebrew not found. {}",
                manual_install_hint()
            )));
        }
        run_install("brew", &["install", "podman"]).await
    }
    #[cfg(target_os = "windows")]
    {
        run_install(
            "winget",
            &[
                "install",
                "-e",
                "--id",
                "RedHat.Podman",
                "--accept-package-agreements",
                "--accept-source-agreements",
            ],
        )
        .await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(IsolationError::OciSetupFailed(
            manual_install_hint().to_string(),
        ))
    }
}

/// Run an install command, mapping a non-zero exit to a guidance-bearing error.
#[allow(dead_code)]
async fn run_install(program: &str, args: &[&str]) -> Result<(), IsolationError> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            IsolationError::OciSetupFailed(format!(
                "running '{program}' failed: {e}. {}",
                manual_install_hint()
            ))
        })?;
    if out.status.success() {
        tracing::info!("podman installed");
        Ok(())
    } else {
        Err(IsolationError::OciSetupFailed(format!(
            "automatic podman install failed: {}. {}",
            String::from_utf8_lossy(&out.stderr).trim(),
            manual_install_hint()
        )))
    }
}

/// The exact command to install podman on the current OS.
pub fn manual_install_hint() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "install it, e.g.: sudo apt-get install -y podman  (or dnf/pacman/zypper)"
    }
    #[cfg(target_os = "macos")]
    {
        "install it: brew install podman && podman machine init && podman machine start"
    }
    #[cfg(target_os = "windows")]
    {
        "install it: winget install RedHat.Podman, then: podman machine init && podman machine start"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "install podman — see https://podman.io/docs/installation"
    }
}
