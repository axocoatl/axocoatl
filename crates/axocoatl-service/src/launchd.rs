//! launchd **user-agent** backend (macOS).
//!
//! A `LaunchAgent` under `~/Library/LaunchAgents` runs in the user's GUI
//! session, needs no root, and (with `RunAtLoad` + `KeepAlive`) is restarted
//! automatically and started at login.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{ServiceError, ServiceManager, ServiceStatus};

/// launchd label for the daemon agent.
const LABEL: &str = "ai.axocoatl.daemon";

/// Manages the daemon as a launchd user agent.
pub struct LaunchdManager {
    /// `~/Library/LaunchAgents/ai.axocoatl.daemon.plist`
    plist_path: PathBuf,
    /// Current user id — the `gui/<uid>` domain target.
    uid: String,
}

impl LaunchdManager {
    /// Resolve the plist path and the current uid.
    pub fn resolve() -> Result<Self, ServiceError> {
        let home = std::env::var("HOME").map_err(|_| ServiceError::NoHome)?;
        let plist_path = PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist"));

        // `id -u` avoids pulling in a libc dependency just for getuid().
        let uid = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| ServiceError::Control("could not determine uid".into()))?;

        Ok(Self { plist_path, uid })
    }

    /// `gui/<uid>` — the launchd domain a LaunchAgent lives in.
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    /// Run `launchctl <args>`, returning trimmed stdout on success.
    fn launchctl(&self, args: &[&str]) -> Result<String, ServiceError> {
        let out = Command::new("launchctl")
            .args(args)
            .output()
            .map_err(|e| ServiceError::Control(format!("running launchctl: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(ServiceError::Control(format!(
                "launchctl {}: {}",
                args.join(" "),
                if stderr.is_empty() { stdout } else { stderr }
            )))
        }
    }
}

impl ServiceManager for LaunchdManager {
    fn backend(&self) -> &'static str {
        "launchd"
    }

    fn install(&self, exe: &Path, config: &Path) -> Result<(), ServiceError> {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#,
            exe = exe.display(),
            config = config.display(),
        );
        if let Some(dir) = self.plist_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.plist_path, plist)?;
        Ok(())
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        // Best-effort bootout — the agent may not be loaded.
        let _ = self.launchctl(&["bootout", &format!("{}/{LABEL}", self.domain())]);
        if self.plist_path.exists() {
            std::fs::remove_file(&self.plist_path)?;
        }
        Ok(())
    }

    fn start(&self) -> Result<(), ServiceError> {
        // bootstrap loads the agent; RunAtLoad starts it immediately.
        let plist = self.plist_path.to_string_lossy().to_string();
        self.launchctl(&["bootstrap", &self.domain(), &plist])?;
        Ok(())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        self.launchctl(&["bootout", &format!("{}/{LABEL}", self.domain())])?;
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let installed = self.plist_path.exists();
        // `launchctl print` exits 0 only when the agent is loaded.
        let printed = self
            .launchctl(&["print", &format!("{}/{LABEL}", self.domain())])
            .ok();
        let running = printed
            .as_deref()
            .map(|p| p.contains("state = running"))
            .unwrap_or(false);
        let detail = if !installed {
            "not installed".to_string()
        } else if printed.is_some() {
            "loaded".to_string()
        } else {
            "installed, not loaded".to_string()
        };
        Ok(ServiceStatus {
            installed,
            running,
            // A loaded LaunchAgent with RunAtLoad is effectively enabled.
            enabled: printed.is_some(),
            detail,
        })
    }
}
