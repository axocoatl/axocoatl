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
use super::*;
use axocoatl_isolation::runtime_volumes::{self, RuntimeVolumeReap, VolumeOwner};
use axocoatl_session::execution_ownership::DataRootFormatOwnership;

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
        let check = match runtime_volumes::reap_leaked_runtime_volumes(
            &self.local_runtime_authority,
            owner,
        )
        .await
        {
            Ok(report) => {
                tracing::info!(
                    removed = report.removed.len(),
                    removed_dependencies = report.removed_dependencies(),
                    kept_open = report.kept_open,
                    kept_dependencies = report.kept_dependencies,
                    other_daemons = report.other_daemons,
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
