//! Native history begins only from an acknowledged, actual Session creation.
//! Missing legacy files, arbitrary Session values and format upgrade alone do
//! not manufacture this receipt. A crash before the origin journal write leaves
//! an unclassified Session, which must fail closed rather than infer its origin.

use super::{Session, SessionError, SessionMode, SessionStore};
use crate::execution_ownership::UpgradedFormatOwnership;
use crate::execution_store::ExecutionStoreOwner;
use crate::turn_contract::SessionId;
use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeHistoryOrigin {
    schema_version: u32,
    ownership_id: String,
    owner: ExecutionStoreOwner,
    created_at: u64,
    creation_sha256: String,
}

impl NativeHistoryOrigin {
    pub(crate) fn validate(
        &self,
        ownership: &UpgradedFormatOwnership,
        owner: &ExecutionStoreOwner,
    ) -> std::io::Result<()> {
        if self.schema_version != 1
            || self.ownership_id != ownership.manifest().ownership_id
            || &self.owner != owner
            || !super::is_canonical_persisted_id(owner.session_id.as_str(), "ses-")
            || self.creation_sha256.len() != 64
            || !self
                .creation_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(std::io::Error::other(
                "native Session origin differs from the actual canonical owner",
            ));
        }
        Ok(())
    }
}

/// Host-only creation evidence. No deserializer or public constructor.
pub struct NativeSessionCreationReceipt {
    pub(crate) origin: NativeHistoryOrigin,
    directory: SecureDir,
}

impl NativeSessionCreationReceipt {
    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.origin.owner
    }

    pub(crate) fn verify(
        &self,
        ownership: &UpgradedFormatOwnership,
        owner: &ExecutionStoreOwner,
    ) -> std::io::Result<()> {
        self.origin.validate(ownership, owner)?;
        let directory = ownership
            .sessions_directory()
            .map_err(std::io::Error::other)?;
        if directory.inode_identity()? != self.directory.inode_identity()? {
            return Err(std::io::Error::other(
                "native creation receipt belongs to another Session store",
            ));
        }
        self.directory.verify_ambient_identity()?;
        let bytes = self
            .directory
            .read(format!("{}.json", owner.session_id.as_str()))?;
        if format!("{:x}", Sha256::digest(&bytes)) != self.origin.creation_sha256 {
            return Err(std::io::Error::other(
                "new Session record changed before its native origin was retained",
            ));
        }
        Ok(())
    }
}

impl SessionStore {
    /// Same creation behavior as v1, plus an opaque receipt from its actual
    /// acknowledged write. Callers retain the origin before preparing a runtime.
    #[allow(clippy::too_many_arguments)]
    pub fn create_native_with_environment(
        &mut self,
        ownership: &UpgradedFormatOwnership,
        name: impl Into<String>,
        workspace_id: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
        setup_command: Option<String>,
        setup_approved: bool,
        setup_reviewed: bool,
    ) -> Result<(Session, NativeSessionCreationReceipt), SessionError> {
        let directory = ownership
            .sessions_directory()
            .map_err(std::io::Error::other)?;
        if directory.inode_identity()? != self.secure_dir.inode_identity()? {
            return Err(std::io::Error::other(
                "native creation requires the exact owned Session store",
            )
            .into());
        }
        let session = self.create_with_environment(
            name,
            workspace_id,
            working_dir,
            mode,
            enabled_skills,
            exposed_ports,
            image,
            setup_command,
            setup_approved,
            setup_reviewed,
        )?;
        let bytes = serde_json::to_vec_pretty(&session)?;
        let receipt = NativeSessionCreationReceipt {
            origin: NativeHistoryOrigin {
                schema_version: 1,
                ownership_id: ownership.manifest().ownership_id.clone(),
                owner: ExecutionStoreOwner {
                    workspace_id: session.workspace_id.clone(),
                    session_id: SessionId::new(&session.id).map_err(std::io::Error::other)?,
                },
                created_at: session.created_at,
                creation_sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
            directory: self.secure_dir.clone(),
        };
        receipt.verify(ownership, receipt.owner())?;
        Ok((session, receipt))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution_content::ExecutionContentStore;
    use crate::execution_namespace::ExecutionComponent;
    use crate::execution_ownership::LegacyFormatOwnership;
    use crate::execution_store::SessionExecutionStore;
    use crate::session_history::{HistoryVisibility, SessionHistory};
    use std::sync::Arc;

    #[test]
    fn actual_creation_origin_is_durable_and_foreign_receipt_never_reclassifies_history() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let data = SecureDir::open(root.path()).unwrap();
        let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
        let owner = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let (session, receipt) = sessions
            .create_native_with_environment(
                &owner,
                "Native",
                "workspace",
                workspace.path(),
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let mut canonical =
            SessionExecutionStore::open(owner.clone(), receipt.owner().clone()).unwrap();
        let content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        assert!(SessionHistory::from_upgraded(&canonical, &content).is_err());
        canonical.record_native_origin(&receipt).unwrap();
        let bytes = std::fs::read(canonical.path()).unwrap();
        canonical.record_native_origin(&receipt).unwrap();
        assert_eq!(std::fs::read(canonical.path()).unwrap(), bytes);
        assert!(SessionHistory::from_upgraded(&canonical, &content)
            .unwrap()
            .entries(HistoryVisibility::IncludingSuperseded)
            .is_empty());
        assert!(canonical.legacy_history_snapshot().is_err());
        assert_eq!(
            data.existing_child("sessions")
                .unwrap()
                .read(format!("{}.json", session.id))
                .unwrap(),
            serde_json::to_vec_pretty(&session).unwrap()
        );
        let (_, foreign) = sessions
            .create_native_with_environment(
                &owner,
                "Other",
                "workspace",
                workspace.path(),
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        assert!(canonical.record_native_origin(&foreign).is_err());
        assert_eq!(std::fs::read(canonical.path()).unwrap(), bytes);
        let identity = canonical.identity().unwrap();
        drop(content);
        drop(canonical);
        let canonical =
            SessionExecutionStore::open(owner.clone(), receipt.owner().clone()).unwrap();
        assert_eq!(canonical.identity().unwrap(), identity);
        assert!(canonical.native_origin().unwrap().is_some());
        assert!(canonical.legacy_seal().unwrap().is_none());
        assert!(!root.path().join("session-history").exists());
        let foreign_root = tempfile::tempdir().unwrap();
        let foreign_data = SecureDir::open(foreign_root.path()).unwrap();
        let mut foreign_sessions = SessionStore::new_in_secure(&foreign_data, "sessions").unwrap();
        assert!(foreign_sessions
            .create_native_with_environment(
                &owner,
                "Foreign",
                "workspace",
                workspace.path(),
                SessionMode::SingleAgent {
                    agent_id: "agent".into()
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true
            )
            .is_err());
        assert!(foreign_sessions.list().is_empty());
    }

    /// A data root holds native History for each Session it created, and
    /// still does once the Session is deleted; for any other id it holds
    /// none, so the daemon's start can tell a Session it deleted from one
    /// another data root made.
    #[test]
    fn a_created_session_keeps_its_history_after_deletion() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let data = SecureDir::open(root.path()).unwrap();
        let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let (session, receipt) = sessions
            .create_native_with_environment(
                &ownership,
                "Native",
                "workspace",
                workspace.path(),
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let canonical =
            SessionExecutionStore::open(ownership.clone(), receipt.owner().clone()).unwrap();
        drop(canonical);
        assert!(ownership.holds_session_history(&session.id));
        sessions.remove(&session.id).unwrap();
        assert!(sessions.get(&session.id).is_none());
        assert!(ownership.holds_session_history(&session.id));
        for other in [
            "ses-00000000-0000-4000-8000-000000000000",
            "",
            "../sessions",
            "attempt-abc-def-0",
        ] {
            assert!(!ownership.holds_session_history(other), "{other:?}");
        }
    }

    #[test]
    fn changed_new_session_bytes_refuse_origin_before_any_native_work() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let data = SecureDir::open(root.path()).unwrap();
        let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let (session, receipt) = sessions
            .create_native_with_environment(
                &ownership,
                "Native",
                "workspace",
                workspace.path(),
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let mut canonical =
            SessionExecutionStore::open(ownership, receipt.owner().clone()).unwrap();
        let before = std::fs::read(canonical.path()).unwrap();
        let mut altered = session;
        altered.name = "Changed before origin retention".into();
        data.existing_child("sessions")
            .unwrap()
            .atomic_write(
                format!("{}.json", altered.id),
                &serde_json::to_vec_pretty(&altered).unwrap(),
            )
            .unwrap();
        assert!(canonical.record_native_origin(&receipt).is_err());
        assert!(canonical.native_origin().unwrap().is_none());
        assert_eq!(std::fs::read(canonical.path()).unwrap(), before);
    }
}
