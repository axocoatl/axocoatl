//! The process environment recorded in a service definition.
//!
//! A service manager starts the daemon with a minimal environment, not the
//! installing shell's: launchd gives a LaunchAgent
//! `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, which does not contain a Homebrew
//! Podman in `/opt/homebrew/bin`. Install therefore records what the daemon
//! needs to reach Podman, and nothing else: a `PATH` with the directory of the
//! `podman` found at install time plus the standard system directories, and
//! the Podman connection selection (`CONTAINER_CONNECTION`, `CONTAINER_HOST`)
//! when it is set. Provider keys and other secrets are never read here, so
//! they cannot reach the service definition.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Standard system directories, after the directory of `podman`.
pub const SYSTEM_PATH: [&str; 5] = ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Podman's connection selection, carried from the install-time environment.
pub const CARRIED_VARIABLES: [&str; 2] = ["CONTAINER_CONNECTION", "CONTAINER_HOST"];

/// Environment variables written into the service definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEnvironment {
    variables: Vec<(String, String)>,
    podman: Option<PathBuf>,
    warnings: Vec<String>,
}

impl ServiceEnvironment {
    /// Capture from the current process: the `podman` the installing shell
    /// would run, and its Podman connection selection.
    pub fn capture() -> Self {
        Self::from_lookup(|name| std::env::var_os(name), is_executable_file)
    }

    /// Capture through `var`, the only way this reads the environment, and
    /// `is_executable`, which decides whether a `PATH` candidate is runnable.
    pub fn from_lookup(
        var: impl Fn(&str) -> Option<OsString>,
        is_executable: impl Fn(&Path) -> bool,
    ) -> Self {
        let mut warnings = Vec::new();
        let mut directories: Vec<String> = Vec::new();

        // A relative `PATH` entry means nothing to a service, so only an
        // absolute one can supply `podman`.
        let podman = var("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join("podman"))
                .find(|candidate| is_executable(candidate))
        });
        match &podman {
            Some(binary) => {
                let directory = binary.parent().and_then(Path::to_str);
                match directory.filter(|directory| usable_path_entry(directory)) {
                    Some(directory) => directories.push(directory.to_string()),
                    None => warnings.push(format!(
                        "podman was found at {}, but its directory cannot be written into a PATH, so the service cannot find it. Install Podman in a directory without ':' in its name, then run `axocoatl service install` again.",
                        binary.display()
                    )),
                }
            }
            None => warnings.push(
                "podman was not found on PATH, so the service cannot find it either. Install Podman, then run `axocoatl service install` again.".to_string(),
            ),
        }
        for directory in SYSTEM_PATH {
            if !directories.iter().any(|existing| existing == directory) {
                directories.push(directory.to_string());
            }
        }

        let mut variables = vec![("PATH".to_string(), directories.join(":"))];
        for name in CARRIED_VARIABLES {
            let Some(value) = var(name).filter(|value| !value.is_empty()) else {
                continue;
            };
            match value.into_string() {
                Err(_) => warnings.push(format!(
                    "{name} is not valid UTF-8 and was not recorded in the service definition."
                )),
                Ok(value) if value.chars().any(char::is_control) => warnings.push(format!(
                    "{name} contains a control character and was not recorded in the service definition."
                )),
                Ok(value) if url_has_password(&value) => warnings.push(format!(
                    "{name} contains a password and was not recorded: the service definition never holds secrets. Use a named connection (CONTAINER_CONNECTION) or an SSH key instead."
                )),
                Ok(value) => variables.push((name.to_string(), value)),
            }
        }

        Self {
            variables,
            podman,
            warnings,
        }
    }

    /// The variables to write, in order: `PATH`, then any carried Podman
    /// connection selection.
    pub fn variables(&self) -> &[(String, String)] {
        &self.variables
    }

    /// The `podman` binary found on the install-time `PATH`.
    pub fn podman(&self) -> Option<&Path> {
        self.podman.as_deref()
    }

    /// What install could not record, phrased for the person installing.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// A `PATH` entry must not contain the separator or a control character.
fn usable_path_entry(directory: &str) -> bool {
    !directory.is_empty() && !directory.contains(':') && !directory.chars().any(char::is_control)
}

/// `scheme://user:password@host/...` carries a password in its userinfo.
fn url_has_password(value: &str) -> bool {
    let Some((_, rest)) = value.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn capture(
        variables: &[(&str, &str)],
        podman_at: &[&str],
    ) -> (ServiceEnvironment, Vec<String>) {
        let variables: HashMap<_, _> = variables.iter().copied().collect();
        let requested = RefCell::new(Vec::new());
        let environment = ServiceEnvironment::from_lookup(
            |name| {
                requested.borrow_mut().push(name.to_string());
                variables.get(name).map(OsString::from)
            },
            |candidate| {
                podman_at
                    .iter()
                    .any(|binary| Path::new(binary) == candidate)
            },
        );
        (environment, requested.into_inner())
    }

    fn value<'a>(environment: &'a ServiceEnvironment, name: &str) -> Option<&'a str> {
        environment
            .variables()
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn records_the_homebrew_podman_directory_before_the_system_directories() {
        let (environment, _) = capture(
            &[(
                "PATH",
                "relative/bin:/Users/test/.cargo/bin:/opt/homebrew/bin:/usr/bin",
            )],
            &[
                "relative/bin/podman",
                "/opt/homebrew/bin/podman",
                "/usr/bin/podman",
            ],
        );
        assert_eq!(
            environment.podman(),
            Some(Path::new("/opt/homebrew/bin/podman"))
        );
        assert_eq!(
            value(&environment, "PATH"),
            Some("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        );
        assert!(environment.warnings().is_empty());
    }

    #[test]
    fn a_system_podman_keeps_its_directory_first_without_duplicates() {
        let (environment, _) = capture(
            &[("PATH", "/usr/bin:/usr/local/bin")],
            &["/usr/bin/podman", "/usr/local/bin/podman"],
        );
        assert_eq!(
            value(&environment, "PATH"),
            Some("/usr/bin:/usr/local/bin:/bin:/usr/sbin:/sbin")
        );
    }

    #[test]
    fn a_missing_podman_leaves_the_system_path_and_warns() {
        let (environment, _) = capture(&[("PATH", "/Users/test/bin")], &[]);
        assert_eq!(environment.podman(), None);
        assert_eq!(
            value(&environment, "PATH"),
            Some("/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        );
        assert_eq!(environment.warnings().len(), 1);
        assert!(environment.warnings()[0].contains("podman was not found"));
    }

    #[test]
    fn carries_the_podman_connection_selection() {
        let (environment, _) = capture(
            &[
                ("PATH", "/opt/homebrew/bin"),
                ("CONTAINER_CONNECTION", "axocoatl-ci"),
                (
                    "CONTAINER_HOST",
                    "ssh://core@127.0.0.1:52341/run/user/501/podman/podman.sock",
                ),
            ],
            &["/opt/homebrew/bin/podman"],
        );
        let names: Vec<_> = environment
            .variables()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, ["PATH", "CONTAINER_CONNECTION", "CONTAINER_HOST"]);
        assert_eq!(
            value(&environment, "CONTAINER_CONNECTION"),
            Some("axocoatl-ci")
        );
        assert!(environment.warnings().is_empty());
    }

    #[test]
    fn never_reads_or_records_anything_else() {
        let (environment, requested) = capture(
            &[
                ("PATH", "/opt/homebrew/bin"),
                ("OPENROUTER_API_KEY", "sk-or-secret"),
                ("CONTAINER_CONNECTION", ""),
                (
                    "CONTAINER_HOST",
                    "ssh://core:hunter2@127.0.0.1:52341/run/podman/podman.sock",
                ),
            ],
            &["/opt/homebrew/bin/podman"],
        );
        assert_eq!(
            requested,
            ["PATH", "CONTAINER_CONNECTION", "CONTAINER_HOST"]
        );
        let names: Vec<_> = environment
            .variables()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            names,
            ["PATH"],
            "an empty or password-bearing value is dropped"
        );
        assert!(!format!("{environment:?}").contains("hunter2"));
        assert_eq!(environment.warnings().len(), 1);
        assert!(environment.warnings()[0].contains("CONTAINER_HOST contains a password"));
    }

    #[test]
    fn a_control_character_is_not_recorded() {
        let (environment, _) = capture(
            &[("PATH", "/usr/bin"), ("CONTAINER_CONNECTION", "one\ntwo")],
            &["/usr/bin/podman"],
        );
        assert_eq!(value(&environment, "CONTAINER_CONNECTION"), None);
        assert!(environment.warnings()[0].contains("control character"));
    }

    #[test]
    fn url_password_detection_reads_only_the_authority() {
        assert!(url_has_password("ssh://user:pw@host:22/run/podman.sock"));
        assert!(!url_has_password("ssh://user@host:22/run/podman.sock"));
        assert!(!url_has_password(
            "unix:///run/user/1000/podman/podman.sock"
        ));
        assert!(!url_has_password("tcp://localhost:8080"));
        assert!(!url_has_password("ssh://host/path:with@colon"));
    }
}
