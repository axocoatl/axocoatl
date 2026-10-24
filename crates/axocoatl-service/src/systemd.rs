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
#[cfg(test)]
use std::time::Duration;

use crate::{
    LogMark, ServiceEnvironment, ServiceError, ServiceManager, ServiceStart, ServiceStatus,
    SERVICE_NAME,
};

/// Manages the daemon as a systemd user service.
pub struct SystemdManager {
    /// `~/.config/systemd/user/axocoatl.service`
    unit_path: PathBuf,
    /// The `systemctl` program (a test's stand-in).
    systemctl: PathBuf,
    /// The `journalctl` program (a test's stand-in).
    journalctl: PathBuf,
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
            journalctl: PathBuf::from("journalctl"),
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

    /// Forget the unit's failed state and its recent starts. systemd refuses
    /// a sixth start within 10 seconds ("the control process exited with
    /// error code"), counting the restarts of a start that keeps failing, so
    /// a `restart` right after fixing the cause would not start it. Best
    /// effort: a unit systemd did not load has nothing to forget.
    fn reset_start_limit(&self) {
        let _ = self.systemctl(&["reset-failed", SERVICE_NAME]);
    }

    /// The unit's journal with `args` (`-o cat`, the daemon's lines as it
    /// printed them), or `None` when it cannot be read.
    fn journal(&self, args: &[&str]) -> Option<String> {
        Command::new(&self.journalctl)
            .args(["--user", "-u", SERVICE_NAME, "--no-pager", "-o", "cat"])
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
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
/// line cannot hold the path: not absolute, ending with whitespace, which
/// systemd strips, or with a backslash, which joins the next line to it.
fn systemd_working_directory(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    (text.starts_with('/') && !text.ends_with(char::is_whitespace) && !text.ends_with('\\'))
        .then(|| text.replace('%', "%%"))
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
             or a backslash, or is not absolute; move the configuration to another directory"
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
        crate::last_start_failure(&self.journal(&["-n", "500"])?)
    }

    fn log_mark(&self) -> LogMark {
        // The unit's last entry; with none, all its entries come after.
        let cursor = self
            .journal(&["-n", "1", "--show-cursor"])
            .and_then(|printed| {
                printed
                    .lines()
                    .rev()
                    .find_map(|line| line.trim().strip_prefix("-- cursor: "))
                    .map(str::to_string)
            });
        LogMark { offset: 0, cursor }
    }

    fn log_since(&self, mark: &LogMark) -> Option<String> {
        let mut log = match &mark.cursor {
            Some(cursor) => self.journal(&[&format!("--after-cursor={cursor}")])?,
            None => self.journal(&[])?,
        };
        if log.len() as u64 > crate::LOG_TAIL_BYTES {
            let mut end = crate::LOG_TAIL_BYTES as usize;
            while !log.is_char_boundary(end) {
                end -= 1;
            }
            log.truncate(end);
        }
        Some(log)
    }

    fn exited_since_start(&self) -> bool {
        // `start` and `restart` reset `NRestarts`; a start that failed is
        // `auto-restart` until systemd starts it again, and `failed` once
        // it gives up.
        let shown = self.printed(&[
            "show",
            SERVICE_NAME,
            "--property=ActiveState,SubState,NRestarts",
        ]);
        let value = |key: &str| {
            shown
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or_default()
        };
        matches!(value("ActiveState"), "failed" | "inactive")
            || value("SubState") == "auto-restart"
            || !matches!(value("NRestarts"), "" | "0")
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
        self.reset_start_limit();
        // `start`, not `enable --now`, which exits 0 when systemd cannot
        // start the unit.
        self.systemctl(&["start", SERVICE_NAME])?;
        Ok(ServiceStart::Started)
    }

    fn restart(&self) -> Result<ServiceStart, ServiceError> {
        let running = self.says("is-active", "active");
        self.systemctl(&["enable", SERVICE_NAME])?;
        self.reset_start_limit();
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
    /// process) and starts a stopped one, enabling it either way. Both first
    /// make systemd forget the unit's recent failed starts, which would
    /// otherwise refuse a start after five within 10 seconds.
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
            journalctl: PathBuf::from("journalctl"),
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
        assert_eq!(
            calls(),
            ["enable axocoatl", "reset-failed axocoatl", "start axocoatl"]
        );
        assert_eq!(manager.start().unwrap(), ServiceStart::AlreadyRunning);
        assert_eq!(calls()[3..], ["enable axocoatl"]);
        assert_eq!(manager.restart().unwrap(), ServiceStart::Restarted);
        assert_eq!(
            calls()[4..],
            [
                "enable axocoatl",
                "reset-failed axocoatl",
                "restart axocoatl"
            ]
        );
        manager.stop().unwrap();
        assert_eq!(manager.restart().unwrap(), ServiceStart::Started);
        assert_eq!(
            calls()[7..],
            [
                "stop axocoatl",
                "enable axocoatl",
                "reset-failed axocoatl",
                "restart axocoatl"
            ]
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

    /// What a start waits on: the journal entries after the cursor taken
    /// before it (all of them when the unit had none), and whether systemd
    /// says the daemon exited since it was started.
    #[cfg(unix)]
    #[test]
    fn a_start_reads_the_journal_after_it_and_sees_the_daemon_exit() {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::write_program(
            &dir.path().join("journalctl"),
            r#"#!/bin/sh
J="$(dirname "$0")/journal"
touch "$J"
ARGS="$*"
case "$ARGS" in
*--show-cursor*) tail -n 1 "$J"; if [ -s "$J" ]; then echo "-- cursor: s=$(wc -l < "$J" | tr -d ' ')"; fi ;;
*--after-cursor=s=*) tail -n +"$((${ARGS##*--after-cursor=s=} + 1))" "$J" ;;
*) cat "$J" ;;
esac
"#,
        );
        crate::test_support::write_program(
            &dir.path().join("systemctl"),
            r#"#!/bin/sh
cat "$(dirname "$0")/shown"
"#,
        );
        let manager = SystemdManager {
            unit_path: dir.path().join("axocoatl.service"),
            systemctl: dir.path().join("systemctl"),
            journalctl: dir.path().join("journalctl"),
        };
        let journal = dir.path().join("journal");
        let shown = |text: &str| std::fs::write(dir.path().join("shown"), text).unwrap();

        // No entries yet: what the start logs is everything.
        let mark = manager.log_mark();
        assert_eq!(mark.cursor, None);
        std::fs::write(&journal, "Axocoatl server starting on 127.0.0.1:8080\n").unwrap();
        assert_eq!(
            manager.log_since(&mark).as_deref(),
            Some("Axocoatl server starting on 127.0.0.1:8080\n")
        );

        let mark = manager.log_mark();
        assert_eq!(mark.cursor.as_deref(), Some("s=1"));
        assert_eq!(manager.log_since(&mark).as_deref(), Some(""));
        std::fs::write(
            &journal,
            "Axocoatl server starting on 127.0.0.1:8080\nFailed to bootstrap daemon: no\n",
        )
        .unwrap();
        assert_eq!(
            manager.log_since(&mark).as_deref(),
            Some("Failed to bootstrap daemon: no\n")
        );
        assert_eq!(
            crate::wait_for_daemon(
                &manager,
                &mark,
                Duration::from_secs(5),
                Duration::from_millis(10)
            ),
            crate::DaemonStart::Failed("Failed to bootstrap daemon: no".into())
        );

        shown("ActiveState=active\nSubState=running\nNRestarts=0\n");
        assert!(!manager.exited_since_start());
        for exited in [
            "ActiveState=activating\nSubState=auto-restart\nNRestarts=0\n",
            "ActiveState=active\nSubState=running\nNRestarts=2\n",
            "ActiveState=failed\nSubState=failed\nNRestarts=5\n",
            "ActiveState=inactive\nSubState=dead\nNRestarts=0\n",
        ] {
            shown(exited);
            assert!(manager.exited_since_start(), "{exited}");
        }
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
            "/home/test/joins\\/axocoatl.yaml",
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
