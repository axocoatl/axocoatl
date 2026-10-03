//! systemd **user**-service backend (Linux).
//!
//! A user unit (not a system unit) needs no root. For the daemon to keep
//! running after logout, the user must enable linger — surfaced as a hint by
//! [`SystemdManager::post_install_hint`]. The user manager's default `PATH`
//! need not contain the `podman` the installing shell found, so the unit
//! records the same environment as the launchd agent. Its output goes to the
//! journal, systemd's own log.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{ServiceEnvironment, ServiceError, ServiceManager, ServiceStatus, SERVICE_NAME};

/// Manages the daemon as a systemd user service.
pub struct SystemdManager {
    /// `~/.config/systemd/user/axocoatl.service`
    unit_path: PathBuf,
}

impl SystemdManager {
    /// Resolve the unit path from `$XDG_CONFIG_HOME` (or `$HOME/.config`).
    pub fn resolve() -> Result<Self, ServiceError> {
        let config_home = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map_err(|_| ServiceError::NoHome)?;
        let unit_path = config_home
            .join("systemd")
            .join("user")
            .join(format!("{SERVICE_NAME}.service"));
        Ok(Self { unit_path })
    }

    /// Run `systemctl --user <args>`, returning trimmed stdout on success.
    fn systemctl(&self, args: &[&str]) -> Result<String, ServiceError> {
        let out = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| ServiceError::Control(format!("running systemctl: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(ServiceError::Control(format!(
                "systemctl {}: {}",
                args.join(" "),
                if stderr.is_empty() { stdout } else { stderr }
            )))
        }
    }
}

/// One double-quoted systemd word. Quoting handles spaces, the backslash
/// escapes reverse C-style unescaping, and `%%` keeps specifier expansion
/// from rewriting the text. `ServiceEnvironment` admits no control
/// characters, so a value cannot end its line.
fn systemd_quote(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn systemd_path(path: &Path) -> String {
    systemd_quote(&path.to_string_lossy())
}

fn render_unit(exe: &Path, config: &Path, environment: &ServiceEnvironment) -> String {
    let working_dir = config.parent().unwrap_or_else(|| Path::new("."));
    let environment: String = environment
        .variables()
        .iter()
        .map(|(name, value)| {
            format!(
                "Environment={}\n",
                systemd_quote(&format!("{name}={value}"))
            )
        })
        .collect();
    format!(
        "[Unit]\n\
         Description=Axocoatl always-on agent daemon\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={working_dir}\n\
         {environment}\
         ExecStart={exe} serve --config {config}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = systemd_path(exe),
        config = systemd_path(config),
        working_dir = systemd_path(working_dir),
    )
}

impl ServiceManager for SystemdManager {
    fn backend(&self) -> &'static str {
        "systemd"
    }

    fn install(
        &self,
        exe: &Path,
        config: &Path,
        environment: &ServiceEnvironment,
    ) -> Result<(), ServiceError> {
        let unit = render_unit(exe, config, environment);
        if let Some(dir) = self.unit_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.unit_path, unit)?;
        self.systemctl(&["daemon-reload"])?;
        Ok(())
    }

    fn logs(&self) -> String {
        format!("journalctl --user -u {SERVICE_NAME}")
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        // Stop + disable best-effort — the unit may already be inactive.
        let _ = self.systemctl(&["disable", "--now", SERVICE_NAME]);
        if self.unit_path.exists() {
            std::fs::remove_file(&self.unit_path)?;
        }
        let _ = self.systemctl(&["daemon-reload"]);
        Ok(())
    }

    fn start(&self) -> Result<(), ServiceError> {
        // `enable --now` starts the service AND sets it to run at login.
        self.systemctl(&["enable", "--now", SERVICE_NAME])?;
        Ok(())
    }

    fn stop(&self) -> Result<(), ServiceError> {
        self.systemctl(&["stop", SERVICE_NAME])?;
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let installed = self.unit_path.exists();

        // `is-active` / `is-enabled` exit non-zero when inactive/disabled —
        // that is information, not an error, so inspect the output directly.
        let probe = |verb: &str, want: &str| -> bool {
            Command::new("systemctl")
                .args(["--user", verb, SERVICE_NAME])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == want)
                .unwrap_or(false)
        };
        let running = probe("is-active", "active");
        let enabled = probe("is-enabled", "enabled");

        let detail = if !installed {
            "not installed".to_string()
        } else if running {
            self.systemctl(&["show", SERVICE_NAME, "--property=MainPID"])
                .unwrap_or_else(|_| "running".to_string())
        } else {
            "installed, stopped".to_string()
        };

        Ok(ServiceStatus {
            installed,
            running,
            enabled,
            detail,
        })
    }

    fn post_install_hint(&self) -> Option<String> {
        Some(
            "For the daemon to keep running after you log out, enable linger:\n\
             \x20 loginctl enable-linger \"$USER\""
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn environment(variables: &[(&str, &str)]) -> ServiceEnvironment {
        ServiceEnvironment::from_lookup(
            |name| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            },
            |candidate| candidate == Path::new("/home/test/.local/bin/podman"),
        )
    }

    #[test]
    fn unit_uses_config_directory_and_quotes_paths() {
        let unit = render_unit(
            Path::new("/opt/Axo tools/axocoatl"),
            Path::new("/home/test/My % project/axocoatl.yaml"),
            &environment(&[("PATH", "/home/test/.local/bin")]),
        );
        assert!(unit.contains("WorkingDirectory=\"/home/test/My %% project\""));
        assert!(unit.contains("ExecStart=\"/opt/Axo tools/axocoatl\" serve --config \"/home/test/My %% project/axocoatl.yaml\""));
    }

    #[test]
    fn unit_records_podman_path_and_connection_but_no_secret() {
        let unit = render_unit(
            Path::new("/usr/local/bin/axocoatl"),
            Path::new("/home/test/.config/axocoatl/config.yaml"),
            &environment(&[
                ("PATH", "/home/test/.local/bin:/usr/bin"),
                ("CONTAINER_CONNECTION", "build \"50%\" box"),
                ("CONTAINER_HOST", "unix:///run/user/1000/podman/podman.sock"),
                ("OPENROUTER_API_KEY", "sk-or-secret"),
            ]),
        );
        assert!(unit.contains(
            "[Service]\nType=simple\nWorkingDirectory=\"/home/test/.config/axocoatl\"\n\
             Environment=\"PATH=/home/test/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\"\n\
             Environment=\"CONTAINER_CONNECTION=build \\\"50%%\\\" box\"\n\
             Environment=\"CONTAINER_HOST=unix:///run/user/1000/podman/podman.sock\"\n\
             ExecStart="
        ));
        assert!(!unit.contains("OPENROUTER"));
        assert!(!unit.contains("sk-or-secret"));
        assert!(
            !unit.contains("StandardOutput"),
            "output stays in the journal"
        );
    }
}
