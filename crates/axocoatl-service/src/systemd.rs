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

use crate::{
    ServiceEnvironment, ServiceError, ServiceManager, ServiceStart, ServiceStatus, SERVICE_NAME,
};

/// Manages the daemon as a systemd user service.
pub struct SystemdManager {
    /// `~/.config/systemd/user/axocoatl.service`
    unit_path: PathBuf,
    /// The `systemctl` program (a test's stand-in).
    systemctl: PathBuf,
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
        Ok(Self {
            unit_path,
            systemctl: PathBuf::from("systemctl"),
        })
    }

    /// Run `systemctl --user <args>`, returning trimmed stdout on success.
    fn systemctl(&self, args: &[&str]) -> Result<String, ServiceError> {
        let out = Command::new(&self.systemctl)
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

    /// `systemctl --user <verb> axocoatl` printed `want`. `is-active` and
    /// `is-enabled` exit non-zero when inactive/disabled; that is
    /// information, not an error, so only the output is read.
    fn says(&self, verb: &str, want: &str) -> bool {
        self.printed(&[verb, SERVICE_NAME]) == want
    }

    /// What `systemctl --user <args>` printed, whatever its exit status.
    fn printed(&self, args: &[&str]) -> String {
        Command::new(&self.systemctl)
            .arg("--user")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    /// The unit's state as `is-active` prints it: `active`, `activating`
    /// (between the restarts of a start that keeps failing), `inactive`
    /// (also for a unit systemd did not load), `failed`, ...
    fn active_state(&self) -> String {
        self.printed(&["is-active", SERVICE_NAME])
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

/// `path` as `WorkingDirectory=` takes it: the rest of the line, unquoted,
/// with `%%` keeping specifier expansion from rewriting it. systemd keeps
/// quotes as part of the path, and then refuses the unit ("WorkingDirectory=
/// path is not absolute"), so a quoted one never started. `None` when the
/// line cannot hold the path: not absolute, or ending with whitespace,
/// which systemd strips.
fn systemd_working_directory(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    (text.starts_with('/') && !text.ends_with(char::is_whitespace)).then(|| text.replace('%', "%%"))
}

fn render_unit(
    exe: &Path,
    config: &Path,
    environment: &ServiceEnvironment,
) -> Result<String, ServiceError> {
    // A unit file is lines: a path with a line break (or another control
    // character) would end its line.
    if let Some(path) = [exe, config]
        .into_iter()
        .find(|path| path.to_string_lossy().chars().any(char::is_control))
    {
        return Err(ServiceError::Control(format!(
            "a systemd unit cannot name {path:?}, which holds a control character"
        )));
    }
    let working_dir = config.parent().unwrap_or_else(|| Path::new("."));
    let working_dir_line = systemd_working_directory(working_dir).ok_or_else(|| {
        ServiceError::Control(format!(
            "systemd cannot run the service in {working_dir:?}: the path ends with whitespace \
             or is not absolute; move the configuration to another directory"
        ))
    })?;
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
    Ok(format!(
        "[Unit]\n\
         Description=Axocoatl always-on agent daemon\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={working_dir_line}\n\
         {environment}\
         ExecStart={exe} serve --config {config}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = systemd_path(exe),
        config = systemd_path(config),
    ))
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
        let unit = render_unit(exe, config, environment)?;
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

    fn last_start_failure(&self) -> Option<String> {
        let output = Command::new("journalctl")
            .args([
                "--user",
                "-u",
                SERVICE_NAME,
                "-n",
                "500",
                "--no-pager",
                "-o",
                "cat",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        crate::last_start_failure(&String::from_utf8_lossy(&output.stdout))
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

    fn start(&self) -> Result<ServiceStart, ServiceError> {
        let running = self.says("is-active", "active");
        self.systemctl(&["enable", SERVICE_NAME])?;
        if running {
            return Ok(ServiceStart::AlreadyRunning);
        }
        // `start`, not `enable --now`, which exits 0 when systemd cannot
        // start the unit.
        self.systemctl(&["start", SERVICE_NAME])?;
        Ok(ServiceStart::Started)
    }

    fn restart(&self) -> Result<ServiceStart, ServiceError> {
        let running = self.says("is-active", "active");
        self.systemctl(&["enable", SERVICE_NAME])?;
        // `restart` waits for the old process to stop, then starts the unit
        // `install` wrote and reloaded; it starts a stopped unit.
        self.systemctl(&["restart", SERVICE_NAME])?;
        Ok(if running {
            ServiceStart::Restarted
        } else {
            ServiceStart::Started
        })
    }

    fn stop(&self) -> Result<(), ServiceError> {
        // Nothing runs: `stop` would fail on a unit systemd did not load.
        if matches!(self.active_state().as_str(), "inactive" | "failed") {
            return Ok(());
        }
        // `stop` returns once the process has exited.
        self.systemctl(&["stop", SERVICE_NAME])?;
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let installed = self.unit_path.exists();

        let state = self.active_state();
        let running = state == "active";
        let enabled = self.says("is-enabled", "enabled");

        let detail = if !installed {
            "not installed".to_string()
        } else if running {
            self.systemctl(&["show", SERVICE_NAME, "--property=MainPID"])
                .unwrap_or_else(|_| "running".to_string())
        } else {
            match self
                .printed(&["show", SERVICE_NAME, "--property=LoadState", "--value"])
                .as_str()
            {
                load @ ("bad-setting" | "error" | "not-found" | "masked") => format!(
                    "installed, but systemd does not load the unit ({load}): see `systemctl \
                     --user status {SERVICE_NAME}`"
                ),
                _ => format!("installed, not running ({state})"),
            }
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

    /// `start` starts and enables a stopped unit and only enables a running
    /// one; `restart` restarts a running unit (systemd waits for the old
    /// process) and starts a stopped one, enabling it either way.
    #[cfg(unix)]
    #[test]
    fn start_leaves_a_running_unit_running_and_restart_restarts_it() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("systemctl");
        crate::test_support::write_program(
            &program,
            r#"#!/bin/sh
D="$(dirname "$0")"
shift
echo "$*" >> "$D/calls"
case "$1" in
is-active) if [ -f "$D/active" ]; then cat "$D/active"; else echo inactive; exit 3; fi ;;
is-enabled) echo enabled ;;
show) case "$*" in *LoadState*) cat "$D/load" 2>/dev/null || echo loaded ;; *) echo MainPID=42 ;; esac ;;
restart|start)
  if [ -f "$D/load" ]; then echo "Failed to $1 axocoatl.service: Unit axocoatl.service has a bad unit file setting." >&2; exit 1; fi
  echo active > "$D/active" ;;
enable) [ "$2" = --now ] && echo active > "$D/active" ;;
stop)
  [ -f "$D/load" ] && { echo 'Failed to stop axocoatl.service: Unit axocoatl.service not loaded.' >&2; exit 5; }
  rm -f "$D/active" ;;
esac
exit 0
"#,
        );
        let manager = SystemdManager {
            unit_path: dir.path().join("axocoatl.service"),
            systemctl: program,
        };
        let calls = || {
            std::fs::read_to_string(dir.path().join("calls"))
                .unwrap()
                .lines()
                .filter(|call| !call.starts_with("is-") && !call.starts_with("show"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(manager.start().unwrap(), ServiceStart::Started);
        assert_eq!(calls(), ["enable axocoatl", "start axocoatl"]);
        assert_eq!(manager.start().unwrap(), ServiceStart::AlreadyRunning);
        assert_eq!(calls()[2..], ["enable axocoatl"]);
        assert_eq!(manager.restart().unwrap(), ServiceStart::Restarted);
        assert_eq!(calls()[3..], ["enable axocoatl", "restart axocoatl"]);
        manager.stop().unwrap();
        assert_eq!(manager.restart().unwrap(), ServiceStart::Started);
        assert_eq!(
            calls()[5..],
            ["stop axocoatl", "enable axocoatl", "restart axocoatl"]
        );
        std::fs::write(dir.path().join("axocoatl.service"), "").unwrap();
        assert!(manager.status().unwrap().running);

        // Between the restarts of a start that keeps failing it is not
        // running, and `stop` still stops it.
        std::fs::write(dir.path().join("active"), "activating\n").unwrap();
        let status = manager.status().unwrap();
        assert!(!status.running);
        assert_eq!(status.detail, "installed, not running (activating)");
        manager.stop().unwrap();
        assert_eq!(calls().last().unwrap(), "stop axocoatl");

        // A unit systemd refuses: `start` says so (`enable --now` exited 0),
        // `stop` has nothing to stop, and status names it.
        std::fs::write(dir.path().join("load"), "bad-setting\n").unwrap();
        let error = manager.start().unwrap_err().to_string();
        assert!(error.contains("bad unit file setting"), "{error}");
        manager.stop().unwrap();
        let status = manager.status().unwrap();
        assert!(!status.running);
        assert!(
            status
                .detail
                .starts_with("installed, but systemd does not load the unit (bad-setting)"),
            "{}",
            status.detail
        );
    }

    /// `WorkingDirectory=` takes the rest of its line as the path, so it is
    /// written unquoted (systemd refused the quoted one as "not absolute",
    /// and the service never started); `ExecStart=` quotes its words. A
    /// path a unit line cannot hold is refused.
    #[test]
    fn unit_uses_config_directory_and_quotes_paths() {
        let unit = render_unit(
            Path::new("/opt/Axo tools/axocoatl"),
            Path::new("/home/test/My % project/axocoatl.yaml"),
            &environment(&[("PATH", "/home/test/.local/bin")]),
        )
        .unwrap();
        assert!(unit.contains("\nWorkingDirectory=/home/test/My %% project\n"));
        assert!(unit.contains("ExecStart=\"/opt/Axo tools/axocoatl\" serve --config \"/home/test/My %% project/axocoatl.yaml\""));
        for config in [
            "/home/test/trailing /axocoatl.yaml",
            "/home/test/line\nExecStartPre=/bin/true/axocoatl.yaml",
        ] {
            assert!(render_unit(
                Path::new("/usr/bin/axocoatl"),
                Path::new(config),
                &environment(&[])
            )
            .is_err());
        }
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
        )
        .unwrap();
        assert!(unit.contains(
            "[Service]\nType=simple\nWorkingDirectory=/home/test/.config/axocoatl\n\
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
