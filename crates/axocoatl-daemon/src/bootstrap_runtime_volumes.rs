//! Leaked Session volumes, removed when the daemon starts.
//!
//! Every container of this daemon's runtime authority is removed at start
//! (or, when Podman was not running then, before the first local Session
//! starts). Right after, and before any Session can start, the daemon
//! removes the runtime volumes (`axo-egr-`, `axo-egi-`, `axo-svc-`,
//! `axo-ca-`) of that authority whose Session is closed, deleted or unknown
//! to its data root, keeping an open Session's, and the Node dependency
//! volumes (`axo-ses-<id>-node-modules`) of that authority whose Session is
//! deleted or unknown, keeping an open or closed Session's. A Session record
//! it did not load (it could not, or hid it) counts as a closed Session. It
//! never touches another daemon's; one without the label goes only when its
//! name is a Session of this data root (see
//! `axocoatl_isolation::runtime_volumes`).
//! The counts go to the log and to `axocoatl doctor`
//! ([`AxocoatlDaemon::runtime_volume_check`]).
//!
//! The authority is the digest of the data root's path. When the data root
//! was moved (the same directory at another path), its Sessions' volumes
//! carry the authority of the path it had: the start that records the move
//! keeps that authority in [`PREVIOUS_AUTHORITIES`], and each start treats
//! a volume carrying it as its own when its name is a Session of this data
//! root, until none does.
//!
//! A copy (another directory, as a cross-volume move or a restored backup
//! makes) keeps none: its original may still run and own those volumes, and
//! its Sessions are the copy's too. The start that records the copy keeps
//! the original's authorities in [`COPIED_FROM_AUTHORITIES`] while a volume
//! carries one, so `axocoatl doctor` names those volumes; when the original
//! was deleted, `axocoatl session record-location --original-deleted` makes
//! them this data root's previous ones, as a move's.
use super::*;
use axocoatl_isolation::runtime_volumes::{self, OtherAuthorities, RuntimeVolumeReap, VolumeOwner};
use axocoatl_session::execution_ownership::{DataRootFormatOwnership, PathLease};

/// The runtime authorities of the paths this data root had before it was
/// moved, while a volume of its Sessions still carries one
/// ([`PreviousAuthorities`]).
pub(super) const PREVIOUS_AUTHORITIES: &str = "previous-runtime-authorities.json";
/// The runtime authorities of the data roots this one was copied from (and
/// of the paths they were moved from), a JSON array, while a volume carries
/// one.
pub(super) const COPIED_FROM_AUTHORITIES: &str = "copied-from-runtime-authorities.json";
/// At most this many are kept in each (the latest).
const MAX_AUTHORITIES: usize = 16;

/// What [`PREVIOUS_AUTHORITIES`] holds: the authorities, and the directory
/// (`dev:inode`) they were recorded in. A copy of the data root is another
/// directory, so a file it copied names its original's paths, which it never
/// takes for its own: also when the start that recorded the copy stopped
/// before it moved them to [`COPIED_FROM_AUTHORITIES`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PreviousAuthorities {
    directory: String,
    authorities: Vec<String>,
}

/// The full SHA-256 runtime authorities of `authorities`.
fn valid(authorities: Vec<String>) -> Vec<String> {
    authorities
        .into_iter()
        .filter(|authority| {
            authority.len() == 64 && authority.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .collect()
}

/// [`PREVIOUS_AUTHORITIES`] as it is, whichever directory wrote it; `None`
/// when it is missing or unreadable. A bare array (written by a build
/// before the directory was recorded) names no directory.
fn read_previous(data_root: &SecureDir) -> Option<PreviousAuthorities> {
    let bytes = data_root
        .read_limited(PREVIOUS_AUTHORITIES, 64 * 1024)
        .ok()?;
    serde_json::from_slice::<PreviousAuthorities>(&bytes)
        .ok()
        .or_else(|| {
            serde_json::from_slice::<Vec<String>>(&bytes)
                .ok()
                .map(|authorities| PreviousAuthorities {
                    directory: String::new(),
                    authorities,
                })
        })
}

/// Whether [`PREVIOUS_AUTHORITIES`] was written in this directory.
fn written_here(data_root: &SecureDir, previous: &PreviousAuthorities) -> bool {
    data_root
        .inode_identity()
        .is_ok_and(|directory| directory == previous.directory)
}

/// The runtime authorities kept in [`PREVIOUS_AUTHORITIES`] for this
/// directory; none when the file is missing, unreadable, or another
/// directory's (nothing is then treated as this data root's that is not).
pub(super) fn previous_runtime_authorities(data_root: &SecureDir) -> Vec<String> {
    read_previous(data_root)
        .filter(|previous| written_here(data_root, previous))
        .map_or_else(Vec::new, |previous| valid(previous.authorities))
}

/// The runtime authorities kept in [`COPIED_FROM_AUTHORITIES`] (a JSON
/// array); none when it is missing or unreadable.
pub(super) fn copied_from_runtime_authorities(data_root: &SecureDir) -> Vec<String> {
    valid(
        data_root
            .read_limited(COPIED_FROM_AUTHORITIES, 64 * 1024)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
            .unwrap_or_default(),
    )
}

/// The latest [`MAX_AUTHORITIES`] of `authorities`.
fn latest(authorities: &[String]) -> &[String] {
    &authorities[authorities.len().saturating_sub(MAX_AUTHORITIES)..]
}

/// Remove `file` from the data root, if it is there.
fn remove_kept(data_root: &SecureDir, file: &str) -> std::io::Result<()> {
    match data_root.remove_file(file) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => data_root.sync_all(),
    }
}

/// Keep `authorities` in [`COPIED_FROM_AUTHORITIES`], removing it when
/// there are none.
fn write_copied_from(data_root: &SecureDir, authorities: &[String]) -> std::io::Result<()> {
    if authorities.is_empty() {
        return remove_kept(data_root, COPIED_FROM_AUTHORITIES);
    }
    let bytes = serde_json::to_vec(latest(authorities)).map_err(std::io::Error::other)?;
    data_root.atomic_write(COPIED_FROM_AUTHORITIES, &bytes)
}

/// Keep `authorities` in [`PREVIOUS_AUTHORITIES`] for this directory,
/// removing it when there are none.
fn write_previous(data_root: &SecureDir, authorities: &[String]) -> std::io::Result<()> {
    if authorities.is_empty() {
        return remove_kept(data_root, PREVIOUS_AUTHORITIES);
    }
    let bytes = serde_json::to_vec(&PreviousAuthorities {
        directory: data_root.inode_identity()?,
        authorities: latest(authorities).to_vec(),
    })
    .map_err(std::io::Error::other)?;
    data_root.atomic_write(PREVIOUS_AUTHORITIES, &bytes)
}

fn recording_failed(data_root: &SecureDir, error: std::io::Error) -> DaemonError {
    DaemonError::Session(format!(
        "recording the data directory's previous locations in '{}': {error}",
        data_root.path().display()
    ))
}

/// `kept` and then each of `added` that is not in it nor `own`.
fn joined(mut kept: Vec<String>, added: &[String], own: &str) -> Vec<String> {
    for authority in added {
        if authority != own && !kept.contains(authority) {
            kept.push(authority.clone());
        }
    }
    kept
}

/// Record that this data root is a copy of the data root whose runtime
/// authority was `original` (a digest of its path): the authorities it had
/// as its own (that one and the paths it was moved from) become ones it was
/// copied from, and it keeps none of them as its own.
pub(super) fn record_copied_from(
    data_root: &SecureDir,
    original: &str,
    own: &str,
) -> Result<(), DaemonError> {
    let mut originals = read_previous(data_root)
        .map(|previous| valid(previous.authorities))
        .unwrap_or_default();
    originals.push(original.to_string());
    let copied_from = joined(copied_from_runtime_authorities(data_root), &originals, own);
    write_copied_from(data_root, &copied_from)
        .and_then(|()| write_previous(data_root, &[]))
        .map_err(|error| recording_failed(data_root, error))
}

/// [`PREVIOUS_AUTHORITIES`] written in another directory (this data root is
/// a copy, and the start that recorded the copy stopped before it moved
/// them): they become ones it was copied from.
fn settle_copied_previous(data_root: &SecureDir, own: &str) -> Result<(), DaemonError> {
    match read_previous(data_root) {
        Some(previous) if !written_here(data_root, &previous) => {
            let copied_from = joined(
                copied_from_runtime_authorities(data_root),
                &valid(previous.authorities),
                own,
            );
            write_copied_from(data_root, &copied_from)
                .and_then(|()| write_previous(data_root, &[]))
                .map_err(|error| recording_failed(data_root, error))
        }
        _ => Ok(()),
    }
}

/// Keep `authority`, the runtime authority of the path this data root was
/// moved from, in [`PREVIOUS_AUTHORITIES`].
pub(super) fn record_previous_runtime_authority(
    data_root: &SecureDir,
    authority: &str,
) -> Result<(), DaemonError> {
    let previous = previous_runtime_authorities(data_root);
    if previous.iter().any(|kept| kept == authority) {
        return Ok(());
    }
    let previous = joined(previous, &[authority.to_string()], "");
    write_previous(data_root, &previous).map_err(|error| recording_failed(data_root, error))
}

/// The data root this one was copied from was deleted: make the
/// authorities kept in [`COPIED_FROM_AUTHORITIES`] previous ones, as a
/// move's, so the next start treats the volumes of its Sessions that carry
/// them as its own. How many it made previous.
pub(super) fn adopt_copied_from(data_root: &SecureDir, own: &str) -> Result<usize, DaemonError> {
    let copied_from = copied_from_runtime_authorities(data_root);
    let previous = previous_runtime_authorities(data_root);
    let adopted = copied_from
        .iter()
        .filter(|authority| *authority != own && !previous.contains(authority))
        .count();
    write_previous(data_root, &joined(previous, &copied_from, own))
        .and_then(|()| write_copied_from(data_root, &[]))
        .map_err(|error| recording_failed(data_root, error))?;
    Ok(adopted)
}

/// What the daemon did about leaked Session runtime volumes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeVolumeCheck {
    /// Checked when the daemon started, or before the first local Session
    /// started when Podman was not running at start.
    Checked { report: RuntimeVolumeReap },
    /// Not checked yet: Podman was not running when the daemon started. It
    /// is checked before the first local Session starts.
    Deferred,
    /// The check failed; nothing it could not prove leaked was removed.
    Failed { error: String },
}

impl AxocoatlDaemon {
    /// What the daemon did about leaked runtime volumes, for `doctor`.
    pub fn runtime_volume_check(&self) -> RuntimeVolumeCheck {
        self.runtime_volume_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Remove this daemon's leaked Session volumes (see the module
    /// documentation). Call it only after every container of this daemon's
    /// authority was removed and before any Session starts.
    pub(super) async fn reap_leaked_runtime_volumes(&self) {
        let (sessions, records) = {
            let store = self.session_store.lock().await;
            (store.list(), store.record_ids())
        };
        let records = match records {
            Ok(records) => records,
            Err(error) => {
                let error = format!("reading this data root's Session records: {error}");
                tracing::warn!(%error, "leaked Session volumes were not checked");
                *self
                    .runtime_volume_check
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    RuntimeVolumeCheck::Failed { error };
                return;
            }
        };
        // A Session this data root created keeps its native History after
        // Delete.
        let history = match &self._data_dir_lease.ownership {
            DataRootFormatOwnership::Upgraded(ownership) => Some(ownership.clone()),
            DataRootFormatOwnership::Legacy(_) => None,
        };
        let owners = VolumeOwners::new(&sessions, records, move |id| {
            history
                .as_ref()
                .is_some_and(|ownership| ownership.holds_session_history(id))
        });
        let owner = |name: &str| owners.owner(name);
        if let Err(error) = settle_copied_previous(&self.data_root, &self.local_runtime_authority) {
            tracing::warn!(%error, "could not record the locations of the data directory this one was copied from");
        }
        let kept_previous = previous_runtime_authorities(&self.data_root);
        let kept_copied_from = copied_from_runtime_authorities(&self.data_root);
        let own = &self.local_runtime_authority;
        // A root moved back to a path it had is at its own authority again.
        let mut other = OtherAuthorities {
            previous: kept_previous
                .iter()
                .filter(|a| *a != own)
                .cloned()
                .collect(),
            previous_unused: Vec::new(),
            copied_from: kept_copied_from
                .iter()
                .filter(|a| *a != own)
                .cloned()
                .collect(),
        };
        // No daemon runs at a path whose lease this holds, so what a daemon
        // killed there left still running is no one's. Held through the
        // reap.
        let mut leases = Vec::new();
        for previous in &other.previous {
            match PathLease::hold(previous) {
                Ok(Some(lease)) => {
                    leases.push(lease);
                    other.previous_unused.push(previous.clone());
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not tell whether a daemon runs at a path this data root was moved from");
                }
            }
        }
        let reaped = runtime_volumes::reap_leaked_runtime_volumes_of_moved_root(
            &self.local_runtime_authority,
            &other,
            owner,
        )
        .await;
        drop(leases);
        let check = match reaped {
            Ok(reaped) => {
                let written = [
                    (reaped.previous != kept_previous)
                        .then(|| write_previous(&self.data_root, &reaped.previous)),
                    (reaped.copied_from != kept_copied_from)
                        .then(|| write_copied_from(&self.data_root, &reaped.copied_from)),
                ];
                for error in written.into_iter().flatten().filter_map(Result::err) {
                    tracing::warn!(
                        %error,
                        "could not record which other locations' volumes are left"
                    );
                }
                let report = reaped.report;
                tracing::info!(
                    removed = report.removed.len(),
                    removed_dependencies = report.removed_dependencies(),
                    kept_open = report.kept_open,
                    kept_dependencies = report.kept_dependencies,
                    other_daemons = report.other_daemons,
                    copied_from = report.copied_from,
                    unlabelled_kept = report.unlabelled_kept,
                    failed = report.failed.len(),
                    "checked leaked Session volumes: removed this daemon's runtime \
                     volumes of closed, deleted and unknown Sessions and Node dependency volumes \
                     of deleted and unknown Sessions"
                );
                if !report.failed.is_empty() {
                    tracing::warn!(
                        volumes = ?report.failed,
                        "leaked Session volumes that could not be removed"
                    );
                }
                RuntimeVolumeCheck::Checked { report }
            }
            Err(error) => {
                tracing::warn!(%error, "leaked Session volumes were not checked");
                RuntimeVolumeCheck::Failed {
                    error: error.to_string(),
                }
            }
        };
        *self
            .runtime_volume_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = check;
    }
}

/// What this data root knows of the Session each volume is named after.
struct VolumeOwners<H> {
    /// Each Session of this data root: the loaded ones, open or closed, and
    /// every record bootstrap did not load (it could not, or hid it), which
    /// counts as closed so its dependency volume stays.
    sessions: HashMap<String, VolumeOwner>,
    /// The same, by the session key a Way's container name holds.
    ways: HashMap<String, VolumeOwner>,
    /// Whether this data root holds native History for a Session id.
    holds_history: H,
}

impl<H: Fn(&str) -> bool> VolumeOwners<H> {
    fn new(sessions: &[axocoatl_session::Session], records: Vec<String>, holds_history: H) -> Self {
        let mut known: HashMap<String, VolumeOwner> = sessions
            .iter()
            .map(|session| {
                let owner = if session.status == axocoatl_session::SessionStatus::Closed {
                    VolumeOwner::Closed
                } else {
                    VolumeOwner::Open
                };
                (session.id.clone(), owner)
            })
            .collect();
        for id in records {
            known.entry(id).or_insert(VolumeOwner::Closed);
        }
        let ways = known
            .iter()
            .map(|(id, owner)| (crate::attempts::session_key(id), *owner))
            .collect();
        Self {
            sessions: known,
            ways,
            holds_history,
        }
    }

    /// The owner of a volume named after `name`: a Session id, or a Way's
    /// container, `attempt-<session key>-<set key>-<index>`, whose Session
    /// it is. A name that is no Session now but has native History here is
    /// a Session this data root deleted; a Way's whose Session is gone
    /// cannot be traced.
    fn owner(&self, name: &str) -> VolumeOwner {
        if let Some(owner) = self.sessions.get(name) {
            return *owner;
        }
        if let Some(rest) = name.strip_prefix("attempt-") {
            return rest
                .split('-')
                .next()
                .and_then(|key| self.ways.get(key))
                .copied()
                .unwrap_or(VolumeOwner::Unknown);
        }
        if (self.holds_history)(name) {
            return VolumeOwner::Deleted;
        }
        VolumeOwner::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The previous authorities are a data root's own only in the directory
    /// that wrote them: in a copy (which copied the file) they are its
    /// original's, and become ones it was copied from.
    #[test]
    fn previous_authorities_are_the_directorys_that_wrote_them() {
        let private = |path: &std::path::Path| {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
            SecureDir::open(path).unwrap()
        };
        let root = tempfile::tempdir().unwrap();
        let original = private(&root.path().join("original"));
        let (old, older, own) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        record_previous_runtime_authority(&original, &old).unwrap();
        record_previous_runtime_authority(&original, &older).unwrap();
        record_previous_runtime_authority(&original, &old).unwrap();
        assert_eq!(
            previous_runtime_authorities(&original),
            [old.clone(), older.clone()]
        );

        let copy = private(&root.path().join("copy"));
        std::fs::copy(
            original.path().join(PREVIOUS_AUTHORITIES),
            copy.path().join(PREVIOUS_AUTHORITIES),
        )
        .unwrap();
        assert!(previous_runtime_authorities(&copy).is_empty());
        settle_copied_previous(&copy, &own).unwrap();
        assert!(!copy.path().join(PREVIOUS_AUTHORITIES).exists());
        assert_eq!(
            copied_from_runtime_authorities(&copy),
            [old.clone(), older.clone()]
        );
        // Settled: nothing more to move, and the original keeps its own.
        settle_copied_previous(&copy, &own).unwrap();
        assert_eq!(
            copied_from_runtime_authorities(&copy),
            [old.clone(), older.clone()]
        );
        settle_copied_previous(&original, &own).unwrap();
        assert_eq!(
            previous_runtime_authorities(&original),
            [old.clone(), older.clone()]
        );

        // A bare array names no directory: no data root's own.
        std::fs::write(
            original.path().join(PREVIOUS_AUTHORITIES),
            serde_json::to_vec(&[&old]).unwrap(),
        )
        .unwrap();
        assert!(previous_runtime_authorities(&original).is_empty());

        // Adopted once the original is deleted: the copy's own previous
        // ones, written for its directory.
        assert_eq!(adopt_copied_from(&copy, &own).unwrap(), 2);
        assert_eq!(
            previous_runtime_authorities(&copy),
            [old.clone(), older.clone()]
        );
        assert!(copied_from_runtime_authorities(&copy).is_empty());
    }

    #[test]
    fn a_volume_belongs_to_a_session_this_data_root_has_had() {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut store = axocoatl_session::SessionStore::new(data.path().join("sessions")).unwrap();
        let mut create = || {
            store
                .create(
                    "Volumes",
                    "wsp-volumes",
                    work.path(),
                    axocoatl_session::SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                )
                .unwrap()
                .id
        };
        let (open, closed, hidden) = (create(), create(), create());
        store.close(&closed).unwrap();
        assert!(store.quarantine_loaded(&hidden).is_some());
        let deleted = "ses-00000000-0000-4000-8000-00000000000d".to_string();
        let owners = VolumeOwners::new(&store.list(), store.record_ids().unwrap(), |id| {
            id == deleted
        });
        assert_eq!(owners.owner(&open), VolumeOwner::Open);
        assert_eq!(owners.owner(&closed), VolumeOwner::Closed);
        // A record bootstrap hid is still this data root's.
        assert_eq!(owners.owner(&hidden), VolumeOwner::Closed);
        assert_eq!(owners.owner(&deleted), VolumeOwner::Deleted);
        assert_eq!(
            owners.owner("ses-00000000-0000-4000-8000-00000000000f"),
            VolumeOwner::Unknown
        );
        // A Way's container names its Session by its key.
        let way = |session: &str| format!("attempt-{}-k-0", crate::attempts::session_key(session));
        assert_eq!(owners.owner(&way(&open)), VolumeOwner::Open);
        assert_eq!(owners.owner(&way(&closed)), VolumeOwner::Closed);
        assert_eq!(owners.owner(&way(&hidden)), VolumeOwner::Closed);
        assert_eq!(owners.owner(&way(&deleted)), VolumeOwner::Unknown);
    }
}
