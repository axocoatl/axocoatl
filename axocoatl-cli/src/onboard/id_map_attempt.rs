//! What onboarding learned when the install or reinstall of the id-map
//! helpers' package ran and did not fix them: it ran and they still cannot
//! map (an install hook, a configuration tool or a policy changes them
//! afterwards, which no package manager record shows), or it failed (dnf
//! cannot reinstall a build no enabled repository has). Kept in the person's
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
    /// It failed, or changed nothing.
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

fn path(host: &Host) -> PathBuf {
    host.data_home
        .join("onboarding")
        .join("id-map-attempt.json")
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
pub(crate) fn record(host: &Host, command: &str, outcome: Outcome, said: &str) {
    let attempt = Attempt {
        command: command.to_string(),
        outcome,
        said: said.to_string(),
        date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        helpers: helpers(host),
    };
    let file = path(host);
    let written = serde_json::to_vec_pretty(&attempt)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
            let partial = file.with_extension("json.partial");
            std::fs::write(&partial, bytes)?;
            std::fs::rename(&partial, &file)
        });
    if let Err(error) = written {
        tracing::warn!(
            path = %file.display(),
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
