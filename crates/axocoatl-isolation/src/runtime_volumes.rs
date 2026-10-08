//! Session runtime volumes a daemon left behind.
//!
//! A local Session's runtime fills four volumes named after the Session (or
//! a Way's container): the egress proxy's socket volume (`axo-egr-`), the
//! identity-socket volume (`axo-egi-`), the service-socket volume
//! (`axo-svc-`) and the trust volume (`axo-ca-`). Close removes them, but a
//! daemon that stopped before Close (a crash, `kill`, a test harness that
//! stops it), a Delete that failed half-way, or a release before Close
//! removed them leaves them behind. Each one carries the runtime authority
//! label of the daemon that made it (`io.axocoatl.runtime-authority`, the
//! digest of that daemon's data root), the same label its containers carry.
//!
//! At start, once every container of its authority is gone, a daemon removes
//! its own runtime volumes whose Session is closed, deleted or unknown to its
//! data root ([`reap_leaked_runtime_volumes`]). It keeps an open Session's
//! (they are filled again when the Session starts) and never touches a volume
//! that carries another daemon's authority. A volume without the label (none
//! is made without it since these volumes exist) is removed only when its
//! name is a closed Session of this data root.
use crate::error::IsolationError;
use crate::session_sandbox::{SessionSandbox, RUNTIME_AUTHORITY_LABEL};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;

/// The name prefixes of a Session's runtime volumes.
pub const RUNTIME_VOLUME_PREFIXES: [&str; 4] = ["axo-egr-", "axo-egi-", "axo-svc-", "axo-ca-"];

/// One listing of every runtime volume.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// One `podman volume rm` of up to [`REMOVE_BATCH`] volumes.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(60);
const REMOVE_BATCH: usize = 64;

/// What this daemon's data root knows of the Session a runtime volume is
/// named after (for a Way's volume, the Session the Way belongs to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeOwner {
    /// An open Session: its runtime fills the volume again when it starts.
    Open,
    /// A closed Session of this data root.
    Closed,
    /// No Session of this data root: deleted, or never this data root's.
    Unknown,
}

/// One runtime volume as Podman lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedVolume {
    pub name: String,
    /// The Session id or Way container id the volume is named after.
    pub owner_id: String,
    /// Its runtime authority label; `None` when it has none.
    pub authority: Option<String>,
}

/// What [`reap_leaked_runtime_volumes`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeVolumeReap {
    /// Removed: this daemon's volumes of closed, deleted or unknown Sessions.
    pub removed: Vec<String>,
    /// This daemon's volumes kept because their Session is open.
    pub kept_open: usize,
    /// Volumes carrying another daemon's runtime authority, left alone.
    pub other_daemons: usize,
    /// Volumes without a runtime authority that are not a closed Session's
    /// of this data root, left alone.
    pub unlabelled_kept: usize,
    /// Volumes that could not be removed, each with Podman's reason.
    pub failed: Vec<String>,
}

/// `podman volume ls` for every runtime volume, with its authority label.
pub fn list_args() -> Vec<String> {
    let mut args = vec!["volume".to_string(), "ls".to_string()];
    for prefix in RUNTIME_VOLUME_PREFIXES {
        args.push("--filter".into());
        args.push(format!("name={prefix}"));
    }
    args.push("--format".into());
    args.push(format!(
        "{{{{.Name}}}}\t{{{{index .Labels \"{RUNTIME_AUTHORITY_LABEL}\"}}}}"
    ));
    args
}

/// Parse [`list_args`]' output. Podman's name filter matches anywhere in
/// the name, so only a name that starts with a runtime prefix and names
/// something after it is kept.
pub fn parse_listing(stdout: &str) -> Vec<ListedVolume> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, authority) = match line.split_once('\t') {
                Some((name, authority)) => (name.trim(), authority.trim()),
                None => (line.trim(), ""),
            };
            let prefix = RUNTIME_VOLUME_PREFIXES
                .into_iter()
                .find(|prefix| name.starts_with(prefix) && name.len() > prefix.len())?;
            let owner_id = &name[prefix.len()..];
            if !owner_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return None;
            }
            Some(ListedVolume {
                name: name.to_string(),
                owner_id: owner_id.to_string(),
                authority: (!authority.is_empty() && authority != "<no value>")
                    .then(|| authority.to_string()),
            })
        })
        .collect()
}

/// Which of `listed` to remove for the daemon whose runtime authority is
/// `authority`, and the counts of the rest (see the module documentation).
pub fn plan(
    listed: &[ListedVolume],
    authority: &str,
    owner: impl Fn(&str) -> VolumeOwner,
) -> (Vec<String>, RuntimeVolumeReap) {
    let mut remove = Vec::new();
    let mut report = RuntimeVolumeReap::default();
    for volume in listed {
        match volume.authority.as_deref() {
            Some(label) if label == authority => match owner(&volume.owner_id) {
                VolumeOwner::Open => report.kept_open += 1,
                VolumeOwner::Closed | VolumeOwner::Unknown => remove.push(volume.name.clone()),
            },
            Some(_) => report.other_daemons += 1,
            None => match owner(&volume.owner_id) {
                VolumeOwner::Closed => remove.push(volume.name.clone()),
                VolumeOwner::Open | VolumeOwner::Unknown => report.unlabelled_kept += 1,
            },
        }
    }
    (remove, report)
}

/// List every runtime volume, remove the leaked ones of the daemon whose
/// runtime authority is `authority` (see the module documentation), and
/// report. Call it only when no container of that authority exists, before
/// any Session of it starts: a volume a container still uses is never
/// forced, but reported as not removed.
pub async fn reap_leaked_runtime_volumes(
    authority: &str,
    owner: impl Fn(&str) -> VolumeOwner,
) -> Result<RuntimeVolumeReap, IsolationError> {
    if authority.len() != 64 || !authority.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(IsolationError::OciSetupFailed(
            "Axocoatl runtime authority is not a full SHA-256 identity".to_string(),
        ));
    }
    let listed = list().await?;
    let (remove, mut report) = plan(&listed, authority, owner);
    for batch in remove.chunks(REMOVE_BATCH) {
        let mut command = Command::new("podman");
        command.args(["volume", "rm"]).args(batch);
        let output = SessionSandbox::run_bounded_command(command, REMOVE_TIMEOUT).await?;
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // Podman removes what it can and names each volume it could not.
        let remaining = present(batch).await?;
        for name in batch {
            if remaining.contains(name) {
                let reason = if output.timed_out {
                    format!("timed out after {} s", REMOVE_TIMEOUT.as_secs())
                } else {
                    stderr
                        .lines()
                        .find(|line| line.contains(name.as_str()))
                        .unwrap_or_else(|| stderr.trim())
                        .trim()
                        .to_string()
                };
                report.failed.push(format!("{name}: {reason}"));
            } else {
                report.removed.push(name.clone());
            }
        }
    }
    Ok(report)
}

async fn list() -> Result<Vec<ListedVolume>, IsolationError> {
    let mut command = Command::new("podman");
    command.args(list_args());
    let output = SessionSandbox::run_bounded_command(command, LIST_TIMEOUT).await?;
    if output.timed_out {
        return Err(IsolationError::Timeout(LIST_TIMEOUT));
    }
    if !output.status.success() {
        return Err(IsolationError::OciContainerFailed(format!(
            "listing Session runtime volumes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if output.stdout_truncated {
        return Err(IsolationError::OciContainerFailed(
            "listing Session runtime volumes: the listing was too long to read whole".to_string(),
        ));
    }
    Ok(parse_listing(&String::from_utf8_lossy(&output.stdout)))
}

/// Which of `names` Podman still lists.
async fn present(names: &[String]) -> Result<Vec<String>, IsolationError> {
    let listed = list().await?;
    Ok(names
        .iter()
        .filter(|name| listed.iter().any(|volume| &volume.name == *name))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(name: &str, authority: Option<&str>) -> ListedVolume {
        parse_listing(&format!("{name}\t{}", authority.unwrap_or("")))
            .pop()
            .unwrap()
    }

    #[test]
    fn the_listing_reads_each_runtime_volume_and_its_authority() {
        let ours = "a".repeat(64);
        let listed = parse_listing(&format!(
            "axo-egr-ses-1\t{ours}\naxo-ca-ses-2\t<no value>\naxo-svc-attempt-k-s-0\t\n\
             not-axo-egr-ses-3\t{ours}\naxo-egr-\t{ours}\naxo-egi-ses-4/../x\t{ours}\n\
             axo-ses-ses-5-node-modules\t{ours}\n"
        ));
        assert_eq!(
            listed,
            vec![
                ListedVolume {
                    name: "axo-egr-ses-1".into(),
                    owner_id: "ses-1".into(),
                    authority: Some(ours.clone()),
                },
                ListedVolume {
                    name: "axo-ca-ses-2".into(),
                    owner_id: "ses-2".into(),
                    authority: None,
                },
                ListedVolume {
                    name: "axo-svc-attempt-k-s-0".into(),
                    owner_id: "attempt-k-s-0".into(),
                    authority: None,
                },
            ]
        );
        let args = list_args().join(" ");
        assert!(
            args.starts_with("volume ls --filter name=axo-egr- "),
            "{args}"
        );
        assert!(
            args.ends_with(&format!(
                "--format {{{{.Name}}}}\t{{{{index .Labels \"{RUNTIME_AUTHORITY_LABEL}\"}}}}"
            )),
            "{args}"
        );
    }

    #[test]
    fn only_this_daemons_volumes_of_sessions_it_has_not_open_are_removed() {
        let ours = "a".repeat(64);
        let theirs = "b".repeat(64);
        let listed = vec![
            volume("axo-egr-ses-open", Some(&ours)),
            volume("axo-egi-ses-closed", Some(&ours)),
            volume("axo-svc-ses-deleted", Some(&ours)),
            volume("axo-ca-ses-deleted", Some(&ours)),
            // Another daemon's, whatever this data root knows of its name.
            volume("axo-egr-ses-closed", Some(&theirs)),
            volume("axo-egr-ses-theirs", Some(&theirs)),
            // No label: only a closed Session of this data root.
            volume("axo-egr-ses-old-closed", None),
            volume("axo-egr-ses-old-open", None),
            volume("axo-egr-ses-old-unknown", None),
        ];
        let owner = |id: &str| match id {
            "ses-open" | "ses-old-open" => VolumeOwner::Open,
            "ses-closed" | "ses-old-closed" => VolumeOwner::Closed,
            _ => VolumeOwner::Unknown,
        };
        let (remove, report) = plan(&listed, &ours, owner);
        assert_eq!(
            remove,
            [
                "axo-egi-ses-closed",
                "axo-svc-ses-deleted",
                "axo-ca-ses-deleted",
                "axo-egr-ses-old-closed"
            ]
        );
        assert_eq!(
            report,
            RuntimeVolumeReap {
                removed: Vec::new(),
                kept_open: 1,
                other_daemons: 2,
                unlabelled_kept: 2,
                failed: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn an_authority_that_is_not_a_data_root_digest_is_refused() {
        for authority in ["", "short", &"g".repeat(64)] {
            assert!(
                reap_leaked_runtime_volumes(authority, |_| VolumeOwner::Unknown)
                    .await
                    .is_err(),
                "{authority:?}"
            );
        }
    }
}
