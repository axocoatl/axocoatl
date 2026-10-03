//! launchd **user-agent** backend (macOS).
//!
//! A `LaunchAgent` under `~/Library/LaunchAgents` runs in the user's GUI
//! session, needs no root, and (with `RunAtLoad` + `KeepAlive`) is restarted
//! automatically and started at login. launchd starts it with a minimal
//! environment and discards its output unless the plist says otherwise, so
//! the plist records a `PATH` that reaches Podman and a log file.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{ServiceEnvironment, ServiceError, ServiceManager, ServiceStatus};

/// launchd label for the daemon agent.
const LABEL: &str = "ai.axocoatl.daemon";

/// Manages the daemon as a launchd user agent.
pub struct LaunchdManager {
    /// `~/Library/LaunchAgents/ai.axocoatl.daemon.plist`
    plist_path: PathBuf,
    /// `~/Library/Logs/Axocoatl/daemon.log`, the daemon's stdout and stderr.
    log_path: PathBuf,
    /// Current user id — the `gui/<uid>` domain target.
    uid: String,
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
        })
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

fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_path(path: &Path) -> String {
    xml_text(&path.to_string_lossy())
}

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
        let manager = LaunchdManager {
            plist_path: root
                .path()
                .join("LaunchAgents")
                .join(format!("{LABEL}.plist")),
            log_path: root.path().join("Logs").join("Axocoatl").join("daemon.log"),
            uid: "0".into(),
        };
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
