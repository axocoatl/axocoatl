//! launchd **user-agent** backend (macOS).
//!
//! A `LaunchAgent` under `~/Library/LaunchAgents` runs in the user's GUI
//! session, needs no root, and (with `RunAtLoad` + `KeepAlive`) is restarted
//! automatically and started at login. launchd starts it with a minimal
//! environment and discards its output unless the plist says otherwise, so
//! the plist records a `PATH` that reaches Podman and a log file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::{ServiceEnvironment, ServiceError, ServiceManager, ServiceStart, ServiceStatus};

/// launchd label for the daemon agent.
const LABEL: &str = "ai.axocoatl.daemon";

/// How long a stopped daemon may take to exit. launchd kills a job that has
/// not exited 20 seconds after it was asked to (its default `ExitTimeOut`).
const EXIT_WAIT: Duration = Duration::from_secs(60);
const EXIT_POLL: Duration = Duration::from_millis(250);

/// Manages the daemon as a launchd user agent.
pub struct LaunchdManager {
    /// `~/Library/LaunchAgents/ai.axocoatl.daemon.plist`
    plist_path: PathBuf,
    /// `~/Library/Logs/Axocoatl/daemon.log`, the daemon's stdout and stderr.
    log_path: PathBuf,
    /// Current user id — the `gui/<uid>` domain target.
    uid: String,
    /// The `launchctl` program (a test's stand-in).
    launchctl: PathBuf,
    /// How long [`Self::wait_unloaded`] waits, and how often it looks.
    exit_wait: Duration,
    exit_poll: Duration,
}

impl LaunchdManager {
    /// Resolve the plist path and the current uid.
    pub fn resolve() -> Result<Self, ServiceError> {
        let home = std::env::var("HOME").map_err(|_| ServiceError::NoHome)?;
        let library = PathBuf::from(home).join("Library");
        let plist_path = library.join("LaunchAgents").join(format!("{LABEL}.plist"));
        let log_path = library.join("Logs").join("Axocoatl").join("daemon.log");

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

        Ok(Self {
            plist_path,
            log_path,
            uid,
            launchctl: PathBuf::from("launchctl"),
            exit_wait: EXIT_WAIT,
            exit_poll: EXIT_POLL,
        })
    }

    /// `gui/<uid>` — the launchd domain a LaunchAgent lives in.
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    /// `gui/<uid>/<label>` — the agent itself.
    fn service_target(&self) -> String {
        format!("{}/{LABEL}", self.domain())
    }

    /// Run `launchctl <args>`, returning trimmed stdout on success.
    fn launchctl(&self, args: &[&str]) -> Result<String, ServiceError> {
        let out = Command::new(&self.launchctl)
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

    /// What `launchctl print` says of the agent: `None` when it is not
    /// loaded, else whether its process runs.
    fn loaded(&self) -> Option<bool> {
        let printed = self.launchctl(&["print", &self.service_target()]).ok()?;
        Some(runs(&printed))
    }

    /// Unload the agent and wait until its process has exited. `bootout`
    /// returns while the daemon still ends its Sessions; launchd lists the
    /// agent until its process has exited, and refuses to load it again
    /// until then ("Bootstrap failed: 5: Input/output error").
    fn unload(&self) -> Result<(), ServiceError> {
        let target = self.service_target();
        if let Err(error) = self.launchctl(&["bootout", &target]) {
            // Not loaded (any more): nothing to stop.
            if self.loaded().is_some() {
                return Err(error);
            }
        }
        let deadline = Instant::now() + self.exit_wait;
        while self.loaded().is_some() {
            if Instant::now() >= deadline {
                return Err(ServiceError::Control(format!(
                    "the Always-On Service did not exit within {} seconds of `launchctl bootout \
                     {target}`; see `launchctl print {target}`",
                    self.exit_wait.as_secs()
                )));
            }
            std::thread::sleep(self.exit_poll);
        }
        Ok(())
    }

    /// Load the agent from the plist on disk; `RunAtLoad` starts it.
    fn load(&self) -> Result<(), ServiceError> {
        let plist = self.plist_path.to_string_lossy().to_string();
        self.launchctl(&["bootstrap", &self.domain(), &plist])
            .map(drop)
    }
}

/// Whether `launchctl print` output says the agent's process runs.
fn runs(printed: &str) -> bool {
    printed.lines().any(|line| line.trim() == "state = running")
}

fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_path(path: &Path) -> String {
    xml_text(&path.to_string_lossy())
}

/// The agent's plist. It leaves `AbandonProcessGroup` unset: when the
/// daemon stops, launchd kills what is left of its process group, so no
/// Podman client the daemon started outlives it. A Podman machine the
/// daemon starts is not in that group (`podman machine start` runs in a
/// session of its own), so stopping or restarting the service leaves the VM
/// running.
fn render_plist(exe: &Path, config: &Path, environment: &ServiceEnvironment, log: &Path) -> String {
    let working_dir = config.parent().unwrap_or_else(|| Path::new("."));
    let variables: String = environment
        .variables()
        .iter()
        .map(|(name, value)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_text(name),
                xml_text(value)
            )
        })
        .collect();
    format!(
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
  <key>WorkingDirectory</key>
  <string>{working_dir}</string>
  <key>EnvironmentVariables</key>
  <dict>
{variables}  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
"#,
        exe = xml_path(exe),
        config = xml_path(config),
        working_dir = xml_path(working_dir),
        log = xml_path(log),
    )
}

/// Create the log directory owner-only and the log file `0600`, so the
/// daemon's output is private however launchd would create it. An existing
/// directory or file is left as it is.
fn prepare_log(log: &Path) -> std::io::Result<()> {
    if let Some(directory) = log.parent() {
        if !directory.exists() {
            std::fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(log).map(drop)
}

impl ServiceManager for LaunchdManager {
    fn backend(&self) -> &'static str {
        "launchd"
    }

    fn install(
        &self,
        exe: &Path,
        config: &Path,
        environment: &ServiceEnvironment,
    ) -> Result<(), ServiceError> {
        let plist = render_plist(exe, config, environment, &self.log_path);
        prepare_log(&self.log_path)?;
        if let Some(dir) = self.plist_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.plist_path, plist)?;
        Ok(())
    }

    fn logs(&self) -> String {
        self.log_path.display().to_string()
    }

    fn last_start_failure(&self) -> Option<String> {
        crate::last_start_failure(&crate::read_log_tail(&self.log_path).ok()?)
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        // Best-effort bootout — the agent may not be loaded.
        let _ = self.launchctl(&["bootout", &self.service_target()]);
        if self.plist_path.exists() {
            std::fs::remove_file(&self.plist_path)?;
        }
        Ok(())
    }

    fn start(&self) -> Result<ServiceStart, ServiceError> {
        match self.loaded() {
            Some(true) => Ok(ServiceStart::AlreadyRunning),
            // Loaded but not running (its start keeps failing, or launchd
            // waits to start it again): load the plist on disk again.
            Some(false) => {
                self.unload()?;
                self.load()?;
                Ok(ServiceStart::Started)
            }
            None => {
                self.load()?;
                Ok(ServiceStart::Started)
            }
        }
    }

    fn restart(&self) -> Result<ServiceStart, ServiceError> {
        let loaded = self.loaded();
        if loaded.is_some() {
            self.unload()?;
        }
        self.load()?;
        Ok(match loaded {
            Some(true) => ServiceStart::Restarted,
            _ => ServiceStart::Started,
        })
    }

    fn stop(&self) -> Result<(), ServiceError> {
        self.unload()
    }

    fn status(&self) -> Result<ServiceStatus, ServiceError> {
        let installed = self.plist_path.exists();
        // `launchctl print` exits 0 only when the agent is loaded.
        let printed = self.launchctl(&["print", &self.service_target()]).ok();
        let running = printed.as_deref().is_some_and(runs);
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
            |candidate| candidate == Path::new("/opt/homebrew/bin/podman"),
        )
    }

    fn homebrew() -> ServiceEnvironment {
        environment(&[
            ("PATH", "/opt/homebrew/bin:/usr/bin"),
            ("CONTAINER_CONNECTION", "axocoatl <ci> & co"),
            ("OPENROUTER_API_KEY", "sk-or-secret"),
        ])
    }

    const LOG: &str = "/Users/test/Library/Logs/Axocoatl/daemon.log";

    /// A manager whose plist and log are under `root`, which runs
    /// `launchctl`.
    fn manager(root: &Path, launchctl: &Path) -> LaunchdManager {
        LaunchdManager {
            plist_path: root.join("LaunchAgents").join(format!("{LABEL}.plist")),
            log_path: root.join("Logs").join("Axocoatl").join("daemon.log"),
            uid: "501".into(),
            launchctl: launchctl.to_path_buf(),
            exit_wait: Duration::from_secs(5),
            exit_poll: Duration::from_millis(10),
        }
    }

    /// A `launchctl` that keeps the agent's state in files beside it, as
    /// launchd behaves: `bootstrap` fails while the agent is loaded
    /// ("Bootstrap failed: 5: Input/output error"); `bootout` returns at
    /// once while the daemon still exits, and `print` lists the agent for
    /// two more calls (forever when `never-exits` exists); `print` of an
    /// agent that is not loaded fails with 113. Each call is logged to
    /// `calls`.
    #[cfg(unix)]
    struct FakeLaunchctl {
        dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl FakeLaunchctl {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            crate::test_support::write_program(
                &dir.path().join("launchctl"),
                r#"#!/bin/sh
D="$(dirname "$0")"
echo "$*" >> "$D/calls"
case "$1" in
print)
  [ -f "$D/loaded" ] || { echo 'Could not find service "ai.axocoatl.daemon" in domain for user gui: 501' >&2; exit 113; }
  if [ -f "$D/exiting" ]; then
    left=$(cat "$D/exiting")
    if [ "$left" -le 0 ]; then
      rm -f "$D/loaded" "$D/exiting"
      echo 'Could not find service "ai.axocoatl.daemon" in domain for user gui: 501' >&2
      exit 113
    fi
    echo $((left - 1)) > "$D/exiting"
  fi
  printf 'gui/501/ai.axocoatl.daemon = {
	state = %s
	endpoints = {
		state = active
	}
}
' "$(cat "$D/loaded")"
  ;;
bootstrap)
  if [ -f "$D/loaded" ]; then echo 'Bootstrap failed: 5: Input/output error' >&2; exit 5; fi
  echo running > "$D/loaded"
  ;;
bootout)
  [ -f "$D/loaded" ] || { echo 'Boot-out failed: 3: No such process' >&2; exit 3; }
  [ -f "$D/never-exits" ] || echo 2 > "$D/exiting"
  ;;
esac
"#,
            );
            Self { dir }
        }

        fn program(&self) -> PathBuf {
            self.dir.path().join("launchctl")
        }

        /// Load the agent in `state` (`running`, `not running`), or unload it.
        fn set(&self, state: Option<&str>) {
            let loaded = self.dir.path().join("loaded");
            match state {
                Some(state) => std::fs::write(loaded, format!("{state}\n")).unwrap(),
                None => {
                    let _ = std::fs::remove_file(loaded);
                }
            }
        }

        fn state(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join("loaded"))
                .ok()
                .map(|state| state.trim().to_string())
        }

        /// The calls, without `print`.
        fn changes(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .filter(|call| !call.starts_with("print"))
                .map(str::to_string)
                .collect()
        }
    }

    /// `start` loads an agent that is not loaded, leaves a running one
    /// running (a bare `bootstrap` failed on it with "Input/output error"),
    /// and loads again one that is loaded but does not run, waiting until
    /// launchd has let go of it.
    #[cfg(unix)]
    #[test]
    fn start_loads_once_and_leaves_a_running_agent_running() {
        let root = tempfile::tempdir().unwrap();
        let launchctl = FakeLaunchctl::new();
        let manager = manager(root.path(), &launchctl.program());
        let plist = manager.plist_path.display().to_string();

        assert_eq!(manager.start().unwrap(), ServiceStart::Started);
        assert_eq!(launchctl.changes(), [format!("bootstrap gui/501 {plist}")]);
        assert_eq!(launchctl.state().as_deref(), Some("running"));

        assert_eq!(manager.start().unwrap(), ServiceStart::AlreadyRunning);
        assert_eq!(launchctl.changes().len(), 1, "{:?}", launchctl.changes());

        launchctl.set(Some("not running"));
        assert_eq!(manager.start().unwrap(), ServiceStart::Started);
        assert_eq!(
            launchctl.changes()[1..],
            [
                "bootout gui/501/ai.axocoatl.daemon".to_string(),
                format!("bootstrap gui/501 {plist}")
            ]
        );
        assert_eq!(launchctl.state().as_deref(), Some("running"));
    }

    /// `restart` stops a running daemon, waits until its process has exited
    /// (launchd still lists it for a while after `bootout` returns), and
    /// only then loads the plist on disk, so the bootstrap never fails;
    /// with nothing running it starts it.
    #[cfg(unix)]
    #[test]
    fn restart_waits_for_the_old_daemon_to_exit_before_loading_the_definition() {
        let root = tempfile::tempdir().unwrap();
        let launchctl = FakeLaunchctl::new();
        let manager = manager(root.path(), &launchctl.program());
        let plist = manager.plist_path.display().to_string();

        assert_eq!(manager.restart().unwrap(), ServiceStart::Started);
        assert_eq!(launchctl.changes(), [format!("bootstrap gui/501 {plist}")]);

        assert_eq!(manager.restart().unwrap(), ServiceStart::Restarted);
        assert_eq!(
            launchctl.changes()[1..],
            [
                "bootout gui/501/ai.axocoatl.daemon".to_string(),
                format!("bootstrap gui/501 {plist}")
            ]
        );
        assert_eq!(launchctl.state().as_deref(), Some("running"));
        let calls = std::fs::read_to_string(launchctl.dir.path().join("calls")).unwrap();
        let after_bootout = calls.split("bootout").nth(1).unwrap();
        assert!(
            after_bootout.matches("print").count() >= 3,
            "it waited for the exit:\n{calls}"
        );
    }

    /// `stop` returns once the daemon has exited, so `stop` then `start`
    /// starts it again; it leaves a stopped agent alone, and says so when
    /// the daemon does not exit.
    #[cfg(unix)]
    #[test]
    fn stop_waits_for_the_daemon_to_exit() {
        let root = tempfile::tempdir().unwrap();
        let launchctl = FakeLaunchctl::new();
        let manager = manager(root.path(), &launchctl.program());
        launchctl.set(Some("running"));

        manager.stop().unwrap();
        assert_eq!(launchctl.state(), None);
        assert_eq!(manager.start().unwrap(), ServiceStart::Started);

        let before = launchctl.changes().len();
        launchctl.set(None);
        manager.stop().unwrap();
        assert_eq!(launchctl.changes().len(), before + 1);

        launchctl.set(Some("running"));
        std::fs::write(launchctl.dir.path().join("never-exits"), "").unwrap();
        let manager = LaunchdManager {
            exit_wait: Duration::from_millis(200),
            ..manager
        };
        let error = manager.stop().unwrap_err().to_string();
        assert!(
            error.contains("did not exit within")
                && error.contains("launchctl print gui/501/ai.axocoatl.daemon"),
            "{error}"
        );
        let error = manager.restart().unwrap_err().to_string();
        assert!(error.contains("did not exit"), "{error}");
        assert!(!launchctl.changes().last().unwrap().starts_with("bootstrap"));
    }

    #[test]
    fn plist_uses_config_directory_and_escapes_paths() {
        let plist = render_plist(
            Path::new("/Applications/Axo & tools/axocoatl"),
            Path::new("/Users/test/My <project>/axocoatl.yaml"),
            &homebrew(),
            Path::new(LOG),
        );
        assert!(plist.contains(
            "<key>WorkingDirectory</key>\n  <string>/Users/test/My &lt;project&gt;</string>"
        ));
        assert!(plist.contains("/Applications/Axo &amp; tools/axocoatl"));
        assert!(plist.contains("/Users/test/My &lt;project&gt;/axocoatl.yaml"));
    }

    #[test]
    fn plist_records_podman_path_connection_and_log_but_no_secret() {
        let plist = render_plist(
            Path::new("/usr/local/bin/axocoatl"),
            Path::new("/Users/test/Library/Application Support/Axocoatl/config.yaml"),
            &homebrew(),
            Path::new(LOG),
        );
        assert!(plist.contains(
            "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>\n    <key>CONTAINER_CONNECTION</key>\n    <string>axocoatl &lt;ci&gt; &amp; co</string>\n  </dict>\n"
        ));
        assert!(plist.contains(&format!(
            "  <key>StandardOutPath</key>\n  <string>{LOG}</string>\n  <key>StandardErrorPath</key>\n  <string>{LOG}</string>\n"
        )));
        assert!(!plist.contains("OPENROUTER"));
        assert!(!plist.contains("sk-or-secret"));
    }

    /// launchd itself must read what install writes: the property list
    /// parser accepts it and returns the recorded values.
    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_reads_the_environment_and_log_back() {
        use std::io::Write;
        use std::process::Stdio;

        let plist = render_plist(
            Path::new("/usr/local/bin/axocoatl"),
            Path::new("/Users/test/Library/Application Support/Axocoatl/config.yaml"),
            &environment(&[
                ("PATH", "/opt/homebrew/bin"),
                ("CONTAINER_CONNECTION", "axocoatl <ci> & co"),
                (
                    "CONTAINER_HOST",
                    "ssh://core@127.0.0.1:52341/run/podman.sock",
                ),
            ]),
            Path::new(LOG),
        );
        let extract = |key: &str| {
            let mut child = Command::new("plutil")
                .args(["-extract", key, "raw", "-o", "-", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(plist.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "plutil cannot read {key}:\n{plist}"
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .trim_end()
                .to_string()
        };
        assert_eq!(
            extract("EnvironmentVariables.PATH"),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert_eq!(
            extract("EnvironmentVariables.CONTAINER_CONNECTION"),
            "axocoatl <ci> & co"
        );
        assert_eq!(
            extract("EnvironmentVariables.CONTAINER_HOST"),
            "ssh://core@127.0.0.1:52341/run/podman.sock"
        );
        assert_eq!(extract("StandardOutPath"), LOG);
        assert_eq!(extract("StandardErrorPath"), LOG);
        assert_eq!(extract("ProgramArguments.1"), "serve");
    }

    #[test]
    fn install_writes_the_plist_and_a_private_log_without_loading_it() {
        let root = tempfile::tempdir().unwrap();
        let manager = manager(root.path(), Path::new("launchctl"));
        let config = root.path().join("config.yaml");
        manager
            .install(Path::new("/usr/local/bin/axocoatl"), &config, &homebrew())
            .unwrap();

        let plist = std::fs::read_to_string(&manager.plist_path).unwrap();
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains(&manager.log_path.display().to_string()));
        assert_eq!(manager.logs(), manager.log_path.display().to_string());
        assert_eq!(std::fs::read(&manager.log_path).unwrap(), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(manager.log_path.parent().unwrap()), 0o700);
            assert_eq!(mode(&manager.log_path), 0o600);
        }

        // Reinstalling keeps what the daemon already logged.
        std::fs::write(&manager.log_path, b"earlier output\n").unwrap();
        manager
            .install(Path::new("/usr/local/bin/axocoatl"), &config, &homebrew())
            .unwrap();
        assert_eq!(
            std::fs::read(&manager.log_path).unwrap(),
            b"earlier output\n"
        );
    }
}
