//! Leaked Session runtime volumes, removed when the daemon starts.
//!
//! Every container of this daemon's runtime authority is removed at start
//! (or, when Podman was not running then, before the first local Session
//! starts). Right after, and before any Session can start, the daemon
//! removes the runtime volumes (`axo-egr-`, `axo-egi-`, `axo-svc-`,
//! `axo-ca-`) of that authority whose Session is closed, deleted or unknown
//! to its data root, keeps an open Session's, and never touches another
//! daemon's (see `axocoatl_isolation::runtime_volumes`). The counts go to
//! the log and to `axocoatl doctor` ([`AxocoatlDaemon::runtime_volume_check`]).
use super::*;
use axocoatl_isolation::runtime_volumes::{self, RuntimeVolumeReap, VolumeOwner};

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

    /// Remove this daemon's leaked runtime volumes (see the module
    /// documentation). Call it only after every container of this daemon's
    /// authority was removed and before any Session starts.
    pub(super) async fn reap_leaked_runtime_volumes(&self) {
        let sessions = self.session_store.lock().await.list();
        let known: HashMap<String, VolumeOwner> = sessions
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
        // A Way's volumes are named after its container,
        // `attempt-<session key>-<set key>-<index>`.
        let ways: HashMap<String, VolumeOwner> = known
            .iter()
            .map(|(id, owner)| (crate::attempts::session_key(id), *owner))
            .collect();
        let owner = |name: &str| {
            if let Some(owner) = known.get(name) {
                return *owner;
            }
            name.strip_prefix("attempt-")
                .and_then(|rest| rest.split('-').next())
                .and_then(|key| ways.get(key))
                .copied()
                .unwrap_or(VolumeOwner::Unknown)
        };
        let check = match runtime_volumes::reap_leaked_runtime_volumes(
            &self.local_runtime_authority,
            owner,
        )
        .await
        {
            Ok(report) => {
                tracing::info!(
                    removed = report.removed.len(),
                    kept_open = report.kept_open,
                    other_daemons = report.other_daemons,
                    unlabelled_kept = report.unlabelled_kept,
                    failed = report.failed.len(),
                    "checked leaked Session runtime volumes: removed this daemon's volumes of \
                     closed, deleted and unknown Sessions"
                );
                if !report.failed.is_empty() {
                    tracing::warn!(
                        volumes = ?report.failed,
                        "leaked Session runtime volumes that could not be removed"
                    );
                }
                RuntimeVolumeCheck::Checked { report }
            }
            Err(error) => {
                tracing::warn!(%error, "leaked Session runtime volumes were not checked");
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
