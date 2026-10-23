//! What onboarding learned when the install or reinstall of the id-map
//! helpers' package ran and did not fix them: it ran and they still cannot
//! map (an install hook, a configuration tool or a policy changes them
//! afterwards, which no package manager record shows), or it failed in a
//! way the same command cannot get past (dnf cannot reinstall a build no
//! enabled repository has; a failure the network may have caused is not
//! kept, as the same command works once it is fixed). Kept in the person's
//! Axocoatl data directory (`onboarding/id-map-attempt.json`), so doctor
//! and every later onboarding say it and never offer the same command
//! again, while the helpers are still the files they were then: an upgrade
//! or a reinstall by hand writes new files, after which the command is
//! offered again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::host::Host;

/// How the command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Outcome {
    /// It ran, and the helpers still could not map.
    DidNotFix,
    /// It failed, or changed nothing, and would again as it is.
    Failed,
}

/// One helper file as it was: where `PATH` found it, and which file it was
/// (`None`: missing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HelperFile {
    pub(crate) name: String,
    pub(crate) file: Option<FileIdentity>,
}

/// Device, inode and change time: a package manager writing the file
/// again (a reinstall, an upgrade) changes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileIdentity {
    pub(crate) path: String,
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) changed: i64,
    pub(crate) changed_nanos: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attempt {
    /// As shown (`sudo dnf reinstall -y shadow-utils`).
    pub(crate) command: String,
    pub(crate) outcome: Outcome,
    /// What onboarding said then: why, and what to do instead.
    pub(crate) said: String,
    /// `YYYY-MM-DD`, UTC.
    pub(crate) date: String,
    pub(crate) helpers: Vec<HelperFile>,
}

const NAMES: [&str; 2] = ["newuidmap", "newgidmap"];

const DIRECTORY: &str = "onboarding";
const FILE: &str = "id-map-attempt.json";

fn path(host: &Host) -> PathBuf {
    host.data_home.join(DIRECTORY).join(FILE)
}

/// The helpers as `PATH` finds them now.
pub(crate) fn helpers(host: &Host) -> Vec<HelperFile> {
    NAMES
        .iter()
        .map(|name| HelperFile {
            name: name.to_string(),
            file: host.find_on_path(name).and_then(|path| identity(&path)),
        })
        .collect()
}

#[cfg(unix)]
fn identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileIdentity {
        path: path.display().to_string(),
        device: metadata.dev(),
        inode: metadata.ino(),
        changed: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    })
}

#[cfg(not(unix))]
fn identity(_: &Path) -> Option<FileIdentity> {
    None
}

/// Keep `command`'s `outcome`, and what onboarding `said` about it, with
/// the helpers as they are now. Best effort: a failure to write it is
/// logged, and the next run offers the command again, as before.
///
/// Onboarding gets here before anything else has made the data root, so
/// this may make it: owner-only, as the daemon makes it, whatever the
/// umask. Under `umask 002` (the default for a user with a group of their
/// own, as on Ubuntu 24.04 and Fedora) a directory made with the umask's
/// mode is group-writable, and the daemon and every command refuse a data
/// root a group can write.
pub(crate) fn record(host: &Host, command: &str, outcome: Outcome, said: &str) {
    let attempt = Attempt {
        command: command.to_string(),
        outcome,
        said: said.to_string(),
        date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        helpers: helpers(host),
    };
    let written = serde_json::to_vec_pretty(&attempt)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            // Each directory it makes `0700`, the file `0600`.
            axocoatl_core::SecureDir::open_or_create_all(&host.data_home)?
                .child(DIRECTORY)?
                .atomic_write(FILE, &bytes)
        });
    if let Err(error) = written {
        tracing::warn!(
            path = %path(host).display(),
            %error,
            "could not keep what the id-map helpers' install did"
        );
    }
}

/// What a recorded `command` did, when it is the command offered now
/// (`install`) and the helpers are still the files they were then.
pub(crate) fn recorded(host: &Host, install: &str) -> Option<Attempt> {
    let bytes = std::fs::read(path(host)).ok()?;
    let attempt: Attempt = serde_json::from_slice(&bytes).ok()?;
    (attempt.command == install && attempt.helpers == helpers(host)).then_some(attempt)
}

impl Attempt {
    /// Why the command is not offered again, in words that start with the
    /// command (`` `sudo dnf reinstall -y shadow-utils` ``), and what to do.
    pub(crate) fn words(&self, unprivileged: &[String]) -> String {
        match self.outcome {
            Outcome::DidNotFix => {
                let (them, are) = match unprivileged.len() {
                    1 => ("it", "it is"),
                    _ => ("them", "they are"),
                };
                let fix = if unprivileged.is_empty() {
                    String::new()
                } else {
                    format!(
                        ", or make {them} setuid root (`sudo chmod u+s {}`)",
                        unprivileged.join(" ")
                    )
                };
                format!(
                    "`{}` ran on {} and the helpers still could not map, so something on this \
                     computer changes them after the package manager writes them (an install \
                     hook, a configuration tool or a security policy), and running it again \
                     would not help: find what does{fix}, and check that {are} able to map \
                     after the next update",
                    self.command, self.date
                )
            }
            // `said` starts with the command, in backticks.
            Outcome::Failed => format!(
                "on {}, {}, and running it again as it is would not help",
                self.date, self.said
            ),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::onboard::host::testing::Shims;
    use std::os::unix::fs::PermissionsExt;

    /// An attempt kept before the data root exists makes it owner-only
    /// under `umask 002` (measured as the default in an Ubuntu 24.04 VM),
    /// so the daemon admits it afterwards; with the umask's modes it was
    /// `0775`, which the daemon refuses as group-writable. Run in a child
    /// process of this test binary, because the umask is the whole
    /// process's.
    #[test]
    fn an_attempt_kept_first_leaves_the_data_root_owner_only_under_umask_002() {
        const CHILD: &str = "AXOCOATL_TEST_ID_MAP_ATTEMPT_UMASK";
        const NAME: &str = "onboard::id_map_attempt::tests::\
                            an_attempt_kept_first_leaves_the_data_root_owner_only_under_umask_002";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // SAFETY: umask only replaces this process's file-creation mask.
        unsafe { libc::umask(0o002) };
        let shims = Shims::new();
        shims.id_map_tools(Some(0o755));
        let host = shims.host(false);
        assert!(!host.data_home.exists());
        let command = "sudo dnf reinstall -y shadow-utils";
        record(&host, command, Outcome::DidNotFix, "");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        // Every directory it made, from the home directory down.
        let mut made = host.data_home.clone();
        for _ in 0..3 {
            assert_eq!(mode(&made), 0o700, "{}", made.display());
            made.pop();
        }
        assert_eq!(made, shims.home.path());
        assert_eq!(mode(&host.data_home.join(DIRECTORY)), 0o700);
        assert_eq!(mode(&path(&host)), 0o600);
        let attempt = recorded(&host, command).expect("kept");
        assert_eq!(attempt.outcome, Outcome::DidNotFix);
        axocoatl_daemon::AxocoatlDaemon::initialize_data_root(&host.data_home)
            .expect("the daemon admits the data root");
        // Kept again over the first: the file is replaced, still `0600`.
        record(&host, command, Outcome::DidNotFix, "");
        assert_eq!(mode(&path(&host)), 0o600);
        assert!(recorded(&host, command).is_some());
    }
}
