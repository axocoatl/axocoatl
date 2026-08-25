//! Session-owned references to immutable context blobs.
//!
//! Blob bytes and global extraction caches remain owned by the blob store.
//! This module owns the session-local name, scope, extraction snapshot, and
//! consumption lifecycle needed to make context visible and reproducible.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::{BeginSessionTurn, SessionTurnContextReference, TurnContextScope};

const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_FILE_NAME: &str = "session-attachments.v1.json";

fn active_by_default() -> bool {
    true
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub enum SessionAttachmentError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unsupported session attachment schema version {0}")]
    UnsupportedVersion(u32),
    #[error("session attachment not found: {0}")]
    NotFound(String),
    #[error("attachment reference {reference_id} already belongs to session {session_id}")]
    ReferenceConflict {
        reference_id: String,
        session_id: String,
    },
    #[error("attachment {reference_id} belongs to session {actual_session_id}, not {expected_session_id}")]
    SessionMismatch {
        reference_id: String,
        expected_session_id: String,
        actual_session_id: String,
    },
    #[error("attachment {reference_id} was already consumed by turn {turn_id}")]
    AlreadyConsumed {
        reference_id: String,
        turn_id: String,
    },
    #[error("attachment {0} is no longer active context")]
    Inactive(String),
}

/// Extraction state captured when a session references a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionAttachmentExtractionStatus {
    #[default]
    Pending,
    Ready,
    Partial,
    Failed,
    Unsupported,
}

/// Immutable-at-turn-time snapshot of extraction details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionAttachmentExtractionSnapshot {
    #[serde(default)]
    pub status: SessionAttachmentExtractionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_char_count: Option<u64>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Whether a one-turn reference remains available or has been assigned to an
/// accepted durable turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionAttachmentConsumedState {
    #[default]
    Available,
    Consumed {
        turn_id: String,
        consumed_at: u64,
    },
}

/// Session-local relation to an immutable blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAttachmentRef {
    pub reference_id: String,
    pub session_id: String,
    pub blob_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_mime: Option<String>,
    pub size: u64,
    pub scope: TurnContextScope,
    pub created_at: u64,
    /// Whether the relation is eligible for composer selection and future
    /// turns. Inactive relations remain as immutable historical blob pins.
    #[serde(default = "active_by_default")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deactivated_at: Option<u64>,
    #[serde(default)]
    pub extraction: SessionAttachmentExtractionSnapshot,
    #[serde(default)]
    pub consumed: SessionAttachmentConsumedState,
    /// Canonical turns that captured this immutable relation. This makes an
    /// inactive relation usable for exact idempotent replay, but never for a
    /// different future turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub historical_turn_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Values supplied when creating a session attachment relation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionAttachmentRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_id: Option<String>,
    pub session_id: String,
    pub blob_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_mime: Option<String>,
    pub size: u64,
    pub scope: TurnContextScope,
    #[serde(default)]
    pub extraction: SessionAttachmentExtractionSnapshot,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAttachmentStore {
    schema_version: u32,
    #[serde(default)]
    references: Vec<SessionAttachmentRef>,
}

/// Atomic snapshot store for session attachment relations.
pub struct SessionAttachmentStore {
    path: PathBuf,
    secure_dir: SecureDir,
    file_name: OsString,
    references: HashMap<String, SessionAttachmentRef>,
}

impl SessionAttachmentStore {
    /// Open `{dir}/session-attachments.v1.json`.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, SessionAttachmentError> {
        let dir = dir.into();
        let secure_dir = SecureDir::open_or_create_all(&dir)?;
        Self::open_at(secure_dir, OsString::from(STORE_FILE_NAME))
    }

    pub fn open_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SessionAttachmentError> {
        let data_root = SecureDir::open(data_root)?;
        Self::open_in_secure(&data_root, relative)
    }

    pub fn open_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SessionAttachmentError> {
        let secure_dir = data_root.child(relative)?;
        Self::open_at(secure_dir, OsString::from(STORE_FILE_NAME))
    }

    pub fn open_file(path: impl Into<PathBuf>) -> Result<Self, SessionAttachmentError> {
        let path = path.into();
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("attachment store path has no parent: {}", path.display()),
            )
        })?;
        let file_name = path
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("attachment store path has no filename: {}", path.display()),
                )
            })?
            .to_os_string();
        let secure_dir = SecureDir::open_or_create_all(parent)?;
        Self::open_at(secure_dir, file_name)
    }

    fn open_at(secure_dir: SecureDir, file_name: OsString) -> Result<Self, SessionAttachmentError> {
        let path = secure_dir.path().join(&file_name);
        if !secure_dir.is_file(&file_name)? {
            let store = Self {
                path,
                secure_dir,
                file_name,
                references: HashMap::new(),
            };
            store.persist(&store.references)?;
            return Ok(store);
        }
        let bytes = secure_dir.read(&file_name)?;
        let persisted: PersistedAttachmentStore = serde_json::from_slice(&bytes)?;
        if persisted.schema_version > STORE_SCHEMA_VERSION {
            return Err(SessionAttachmentError::UnsupportedVersion(
                persisted.schema_version,
            ));
        }
        let references = persisted
            .references
            .into_iter()
            .map(|reference| (reference.reference_id.clone(), reference))
            .collect();
        Ok(Self {
            path,
            secure_dir,
            file_name,
            references,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create idempotently by reference id.
    pub fn create(
        &mut self,
        create: CreateSessionAttachmentRef,
    ) -> Result<SessionAttachmentRef, SessionAttachmentError> {
        let reference_id = create
            .reference_id
            .clone()
            .unwrap_or_else(|| format!("ctx-{}", uuid::Uuid::new_v4()));
        let reference = SessionAttachmentRef {
            reference_id: reference_id.clone(),
            session_id: create.session_id,
            blob_id: create.blob_id,
            display_name: create.display_name,
            declared_mime: create.declared_mime,
            size: create.size,
            scope: create.scope,
            created_at: now_millis(),
            active: true,
            deactivated_at: None,
            extraction: create.extraction,
            consumed: SessionAttachmentConsumedState::Available,
            historical_turn_ids: Vec::new(),
            metadata: create.metadata,
        };
        if let Some(existing) = self.references.get(&reference_id) {
            if equivalent_create(existing, &reference) {
                return Ok(existing.clone());
            }
            return Err(SessionAttachmentError::ReferenceConflict {
                reference_id,
                session_id: existing.session_id.clone(),
            });
        }
        let mut next = self.references.clone();
        next.insert(reference_id, reference.clone());
        self.persist_and_replace(next)?;
        Ok(reference)
    }

    pub fn get(&self, reference_id: &str) -> Option<SessionAttachmentRef> {
        self.references.get(reference_id).cloned()
    }

    /// List active relations for a session in creation order.
    pub fn list(&self, session_id: &str) -> Vec<SessionAttachmentRef> {
        let mut references: Vec<_> = self
            .references
            .values()
            .filter(|reference| reference.session_id == session_id && reference.active)
            .cloned()
            .collect();
        references.sort_by_key(|reference| reference.created_at);
        references
    }

    /// List active and historical relations for internal cleanup and repair.
    pub fn list_all(&self, session_id: &str) -> Vec<SessionAttachmentRef> {
        let mut references: Vec<_> = self
            .references
            .values()
            .filter(|reference| reference.session_id == session_id)
            .cloned()
            .collect();
        references.sort_by_key(|reference| reference.created_at);
        references
    }

    pub fn update_scope(
        &mut self,
        reference_id: &str,
        scope: TurnContextScope,
    ) -> Result<SessionAttachmentRef, SessionAttachmentError> {
        let mut next = self.references.clone();
        let reference = next
            .get_mut(reference_id)
            .ok_or_else(|| SessionAttachmentError::NotFound(reference_id.to_string()))?;
        if let SessionAttachmentConsumedState::Consumed { turn_id, .. } = &reference.consumed {
            return Err(SessionAttachmentError::AlreadyConsumed {
                reference_id: reference_id.to_string(),
                turn_id: turn_id.clone(),
            });
        }
        if !reference.active {
            return Err(SessionAttachmentError::Inactive(reference_id.to_string()));
        }
        reference.scope = scope;
        let result = reference.clone();
        self.persist_and_replace(next)?;
        Ok(result)
    }

    pub fn update_extraction(
        &mut self,
        reference_id: &str,
        extraction: SessionAttachmentExtractionSnapshot,
    ) -> Result<SessionAttachmentRef, SessionAttachmentError> {
        let mut next = self.references.clone();
        let reference = next
            .get_mut(reference_id)
            .ok_or_else(|| SessionAttachmentError::NotFound(reference_id.to_string()))?;
        reference.extraction = extraction;
        let result = reference.clone();
        self.persist_and_replace(next)?;
        Ok(result)
    }

    /// Remove one session relation. The returned blob id lets the caller ask
    /// the blob store whether garbage collection is now possible.
    pub fn detach(
        &mut self,
        session_id: &str,
        reference_id: &str,
    ) -> Result<SessionAttachmentRef, SessionAttachmentError> {
        let reference = self.ensure_session(session_id, reference_id)?;
        if let SessionAttachmentConsumedState::Consumed { turn_id, .. } = &reference.consumed {
            // A consumed relation is the durable pin for immutable context
            // already named by the canonical turn ledger. Removing it would
            // let global blob deletion make historical turns dangling.
            return Err(SessionAttachmentError::AlreadyConsumed {
                reference_id: reference_id.to_string(),
                turn_id: turn_id.clone(),
            });
        }
        let mut next = self.references.clone();
        let removed = next
            .remove(reference_id)
            .expect("validated attachment exists");
        self.persist_and_replace(next)?;
        Ok(removed)
    }

    /// Stop a historically used relation from participating in future turns
    /// without destroying the relation that keeps old turn context resolvable.
    /// Repeating the operation is idempotent.
    pub fn deactivate(
        &mut self,
        session_id: &str,
        reference_id: &str,
    ) -> Result<SessionAttachmentRef, SessionAttachmentError> {
        self.ensure_session(session_id, reference_id)?;
        let mut next = self.references.clone();
        let reference = next
            .get_mut(reference_id)
            .expect("validated attachment exists");
        if reference.active {
            reference.active = false;
            reference.deactivated_at = Some(now_millis());
        }
        let result = reference.clone();
        if next != self.references {
            self.persist_and_replace(next)?;
        }
        Ok(result)
    }

    /// Remove all relations owned by a deleted session.
    pub fn delete_session(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<SessionAttachmentRef>, SessionAttachmentError> {
        let removed = self.list_all(session_id);
        if removed.is_empty() {
            return Ok(Vec::new());
        }
        let mut next = self.references.clone();
        next.retain(|_, reference| reference.session_id != session_id);
        self.persist_and_replace(next)?;
        Ok(removed)
    }

    /// Snapshot selected references into a turn request without consuming
    /// them. Call `mark_consumed` only after `SessionTurnStore::begin` has
    /// durably accepted that exact request.
    pub fn snapshot_into_begin(
        &self,
        begin: &mut BeginSessionTurn,
        reference_ids: &[String],
    ) -> Result<Vec<SessionTurnContextReference>, SessionAttachmentError> {
        let mut context = Vec::with_capacity(reference_ids.len());
        let mut seen = HashSet::new();
        for reference_id in reference_ids {
            if !seen.insert(reference_id) {
                continue;
            }
            let reference = self.ensure_session(&begin.session_id, reference_id)?;
            let idempotent_historical_turn = begin.turn_id.as_deref().is_some_and(|turn_id| {
                reference
                    .historical_turn_ids
                    .iter()
                    .any(|historical_turn_id| historical_turn_id == turn_id)
                    || matches!(
                        &reference.consumed,
                        SessionAttachmentConsumedState::Consumed {
                            turn_id: consumed_turn_id,
                            ..
                        } if consumed_turn_id == turn_id
                    )
            });
            if let SessionAttachmentConsumedState::Consumed { turn_id, .. } = &reference.consumed {
                if begin.turn_id.as_deref() != Some(turn_id.as_str()) {
                    return Err(SessionAttachmentError::AlreadyConsumed {
                        reference_id: reference_id.clone(),
                        turn_id: turn_id.clone(),
                    });
                }
            }
            if !reference.active && !idempotent_historical_turn {
                return Err(SessionAttachmentError::Inactive(reference_id.clone()));
            }
            context.push(to_turn_context(reference));
        }
        begin.context.extend(context.clone());
        Ok(context)
    }

    /// Consume one-turn references atomically after the turn ledger accepts
    /// the turn. Retained session context intentionally remains available.
    /// Repeating the call for the same turn is idempotent.
    pub fn mark_consumed(
        &mut self,
        session_id: &str,
        reference_ids: &[String],
        turn_id: &str,
    ) -> Result<Vec<SessionAttachmentRef>, SessionAttachmentError> {
        let mut next = self.references.clone();
        let mut seen = HashSet::new();
        let consumed_at = now_millis();
        for reference_id in reference_ids {
            if !seen.insert(reference_id) {
                continue;
            }
            let reference = next
                .get_mut(reference_id)
                .ok_or_else(|| SessionAttachmentError::NotFound(reference_id.clone()))?;
            if reference.session_id != session_id {
                return Err(SessionAttachmentError::SessionMismatch {
                    reference_id: reference_id.clone(),
                    expected_session_id: session_id.to_string(),
                    actual_session_id: reference.session_id.clone(),
                });
            }
            if !reference
                .historical_turn_ids
                .iter()
                .any(|historical_turn_id| historical_turn_id == turn_id)
            {
                reference.historical_turn_ids.push(turn_id.to_string());
            }
            if reference.scope != TurnContextScope::Session {
                match &reference.consumed {
                    SessionAttachmentConsumedState::Available => {
                        reference.consumed = SessionAttachmentConsumedState::Consumed {
                            turn_id: turn_id.to_string(),
                            consumed_at,
                        };
                        reference.active = false;
                        reference.deactivated_at = Some(consumed_at);
                    }
                    SessionAttachmentConsumedState::Consumed {
                        turn_id: existing_turn_id,
                        ..
                    } if existing_turn_id == turn_id => {}
                    SessionAttachmentConsumedState::Consumed {
                        turn_id: existing_turn_id,
                        ..
                    } => {
                        return Err(SessionAttachmentError::AlreadyConsumed {
                            reference_id: reference_id.clone(),
                            turn_id: existing_turn_id.clone(),
                        });
                    }
                }
            }
        }
        let result: Vec<_> = reference_ids
            .iter()
            .filter_map(|id| next.get(id).cloned())
            .collect();
        if next != self.references {
            self.persist_and_replace(next)?;
        }
        Ok(result)
    }

    /// Blob ids still referenced by any session relation.
    pub fn blob_ids_in_use(&self) -> HashSet<String> {
        self.references
            .values()
            .map(|reference| reference.blob_id.clone())
            .collect()
    }

    fn ensure_session(
        &self,
        session_id: &str,
        reference_id: &str,
    ) -> Result<&SessionAttachmentRef, SessionAttachmentError> {
        let reference = self
            .references
            .get(reference_id)
            .ok_or_else(|| SessionAttachmentError::NotFound(reference_id.to_string()))?;
        if reference.session_id != session_id {
            return Err(SessionAttachmentError::SessionMismatch {
                reference_id: reference_id.to_string(),
                expected_session_id: session_id.to_string(),
                actual_session_id: reference.session_id.clone(),
            });
        }
        Ok(reference)
    }

    fn persist_and_replace(
        &mut self,
        references: HashMap<String, SessionAttachmentRef>,
    ) -> Result<(), SessionAttachmentError> {
        self.persist(&references)?;
        self.references = references;
        Ok(())
    }

    fn persist(
        &self,
        references: &HashMap<String, SessionAttachmentRef>,
    ) -> Result<(), SessionAttachmentError> {
        let mut values: Vec<_> = references.values().cloned().collect();
        values.sort_by(|left, right| left.reference_id.cmp(&right.reference_id));
        let persisted = PersistedAttachmentStore {
            schema_version: STORE_SCHEMA_VERSION,
            references: values,
        };
        let mut bytes = serde_json::to_vec_pretty(&persisted)?;
        bytes.push(b'\n');
        self.secure_dir.atomic_write(&self.file_name, &bytes)?;
        Ok(())
    }
}

fn equivalent_create(existing: &SessionAttachmentRef, candidate: &SessionAttachmentRef) -> bool {
    existing.reference_id == candidate.reference_id
        && existing.session_id == candidate.session_id
        && existing.blob_id == candidate.blob_id
        && existing.display_name == candidate.display_name
        && existing.declared_mime == candidate.declared_mime
        && existing.size == candidate.size
        && existing.scope == candidate.scope
        && existing.extraction == candidate.extraction
        && existing.metadata == candidate.metadata
}

fn to_turn_context(reference: &SessionAttachmentRef) -> SessionTurnContextReference {
    let mut metadata = reference.metadata.clone();
    metadata.insert(
        "blob_id".to_string(),
        serde_json::Value::String(reference.blob_id.clone()),
    );
    metadata.insert(
        "size".to_string(),
        serde_json::Value::Number(reference.size.into()),
    );
    metadata.insert(
        "extraction".to_string(),
        serde_json::to_value(&reference.extraction).expect("serializable extraction snapshot"),
    );
    SessionTurnContextReference {
        reference_id: reference.reference_id.clone(),
        display_name: reference.display_name.clone(),
        kind: "upload".to_string(),
        scope: reference.scope,
        media_type: reference.declared_mime.clone(),
        content_sha256: reference
            .blob_id
            .strip_prefix("sha256:")
            .map(str::to_string),
        origin: Some("session_attachment".to_string()),
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create(
        session_id: &str,
        reference_id: &str,
        scope: TurnContextScope,
    ) -> CreateSessionAttachmentRef {
        CreateSessionAttachmentRef {
            reference_id: Some(reference_id.to_string()),
            session_id: session_id.to_string(),
            blob_id: "sha256:abc".to_string(),
            display_name: "design.pdf".to_string(),
            declared_mime: Some("application/pdf".to_string()),
            size: 42,
            scope,
            extraction: SessionAttachmentExtractionSnapshot {
                status: SessionAttachmentExtractionStatus::Ready,
                extractor: Some("pdf-extract".to_string()),
                extracted_char_count: Some(100),
                truncated: false,
                error: None,
                metadata: serde_json::Map::new(),
            },
            metadata: serde_json::Map::new(),
        }
    }

    fn begin(session_id: &str) -> BeginSessionTurn {
        BeginSessionTurn {
            turn_id: Some("turn-a".to_string()),
            session_id: session_id.to_string(),
            user_input: "Use the design".to_string(),
            agent_id: None,
            model: None,
            context: Vec::new(),
            idempotency_key: Some("request-a".to_string()),
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn relations_roundtrip_and_keep_session_local_names() {
        let dir = tempdir().unwrap();
        {
            let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
            store
                .create(create("ses-a", "ctx-a", TurnContextScope::ThisTurn))
                .unwrap();
            let mut second = create("ses-b", "ctx-b", TurnContextScope::Session);
            second.display_name = "same-bytes-different-name.pdf".to_string();
            store.create(second).unwrap();
        }
        let store = SessionAttachmentStore::open(dir.path()).unwrap();
        assert_eq!(store.list("ses-a")[0].display_name, "design.pdf");
        assert_eq!(
            store.list("ses-b")[0].display_name,
            "same-bytes-different-name.pdf"
        );
        assert_eq!(store.blob_ids_in_use().len(), 1);
    }

    #[test]
    fn snapshot_then_consume_is_atomic_and_idempotent() {
        let dir = tempdir().unwrap();
        let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::ThisTurn))
            .unwrap();
        let mut request = begin("ses-a");
        store
            .snapshot_into_begin(&mut request, &["ctx-a".to_string()])
            .unwrap();
        assert_eq!(request.context.len(), 1);
        store
            .mark_consumed("ses-a", &["ctx-a".to_string()], "turn-a")
            .unwrap();
        store
            .mark_consumed("ses-a", &["ctx-a".to_string()], "turn-a")
            .unwrap();
        assert!(matches!(
            store.get("ctx-a").unwrap().consumed,
            SessionAttachmentConsumedState::Consumed { ref turn_id, .. } if turn_id == "turn-a"
        ));
        assert!(!store.get("ctx-a").unwrap().active);
        assert!(store.list("ses-a").is_empty());
        let mut retry_as_new_turn = begin("ses-a");
        retry_as_new_turn.turn_id = Some("turn-b".to_string());
        assert!(matches!(
            store.snapshot_into_begin(&mut retry_as_new_turn, &["ctx-a".to_string()]),
            Err(SessionAttachmentError::AlreadyConsumed { .. })
        ));
        let mut idempotent_retry = begin("ses-a");
        store
            .snapshot_into_begin(&mut idempotent_retry, &["ctx-a".to_string()])
            .unwrap();
    }

    #[test]
    fn retained_session_context_is_not_consumed() {
        let dir = tempdir().unwrap();
        let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::Session))
            .unwrap();
        store
            .mark_consumed("ses-a", &["ctx-a".to_string()], "turn-a")
            .unwrap();
        assert_eq!(
            store.get("ctx-a").unwrap().consumed,
            SessionAttachmentConsumedState::Available
        );
        assert!(store.get("ctx-a").unwrap().active);
        assert_eq!(
            store.get("ctx-a").unwrap().historical_turn_ids,
            vec!["turn-a".to_string()]
        );
    }

    #[test]
    fn once_and_session_context_diverge_only_after_durable_turn_acceptance() {
        let dir = tempdir().unwrap();
        {
            let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
            store
                .create(create("ses-a", "ctx-once", TurnContextScope::ThisTurn))
                .unwrap();
            store
                .create(create("ses-a", "ctx-session", TurnContextScope::Session))
                .unwrap();

            let selected = store
                .list("ses-a")
                .into_iter()
                .map(|reference| reference.reference_id)
                .collect::<Vec<_>>();
            let mut first = begin("ses-a");
            store.snapshot_into_begin(&mut first, &selected).unwrap();
            assert_eq!(first.context.len(), 2);
            store.mark_consumed("ses-a", &selected, "turn-a").unwrap();
        }

        let mut reopened = SessionAttachmentStore::open(dir.path()).unwrap();
        let active = reopened.list("ses-a");
        assert_eq!(active.len(), 1, "Once context must leave future selection");
        assert_eq!(active[0].reference_id, "ctx-session");
        assert_eq!(active[0].scope, TurnContextScope::Session);

        let mut second = begin("ses-a");
        second.turn_id = Some("turn-b".to_string());
        reopened
            .snapshot_into_begin(&mut second, &["ctx-session".to_string()])
            .unwrap();
        reopened
            .mark_consumed("ses-a", &["ctx-session".to_string()], "turn-b")
            .unwrap();
        assert_eq!(
            second
                .context
                .iter()
                .map(|reference| reference.reference_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ctx-session"]
        );
        assert_eq!(
            reopened.get("ctx-once").unwrap().historical_turn_ids,
            vec!["turn-a"]
        );
        assert_eq!(
            reopened.get("ctx-session").unwrap().historical_turn_ids,
            vec!["turn-a", "turn-b"]
        );
    }

    #[test]
    fn used_session_context_can_be_deactivated_without_losing_history_pin() {
        let dir = tempdir().unwrap();
        let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::Session))
            .unwrap();
        let mut accepted = begin("ses-a");
        let historical_context = store
            .snapshot_into_begin(&mut accepted, &["ctx-a".to_string()])
            .unwrap();
        store
            .mark_consumed("ses-a", &["ctx-a".to_string()], "turn-a")
            .unwrap();

        let historical = store.deactivate("ses-a", "ctx-a").unwrap();
        assert!(!historical.active);
        assert!(historical.deactivated_at.is_some());
        assert_eq!(historical.historical_turn_ids, vec!["turn-a".to_string()]);
        assert!(store.list("ses-a").is_empty());
        assert_eq!(store.get("ctx-a"), Some(historical.clone()));
        assert_eq!(historical_context[0].reference_id, historical.reference_id);
        assert!(store.blob_ids_in_use().contains("sha256:abc"));

        let mut future = begin("ses-a");
        future.turn_id = Some("turn-b".to_string());
        assert!(matches!(
            store.snapshot_into_begin(&mut future, &["ctx-a".to_string()]),
            Err(SessionAttachmentError::Inactive(ref reference_id)) if reference_id == "ctx-a"
        ));
        let mut exact_replay = begin("ses-a");
        store
            .snapshot_into_begin(&mut exact_replay, &["ctx-a".to_string()])
            .unwrap();

        let reopened = SessionAttachmentStore::open(dir.path()).unwrap();
        assert!(reopened.list("ses-a").is_empty());
        assert!(!reopened.get("ctx-a").unwrap().active);
        assert!(reopened.blob_ids_in_use().contains("sha256:abc"));
    }

    #[test]
    fn consumed_reference_cannot_be_detached() {
        let dir = tempdir().unwrap();
        let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::ThisTurn))
            .unwrap();
        store
            .mark_consumed("ses-a", &["ctx-a".to_string()], "turn-a")
            .unwrap();

        assert!(matches!(
            store.detach("ses-a", "ctx-a"),
            Err(SessionAttachmentError::AlreadyConsumed {
                ref reference_id,
                ref turn_id,
            }) if reference_id == "ctx-a" && turn_id == "turn-a"
        ));
        assert!(store.get("ctx-a").is_some(), "the blob pin remains live");
    }

    #[test]
    fn cross_session_snapshots_are_rejected() {
        let dir = tempdir().unwrap();
        let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::ThisTurn))
            .unwrap();
        let mut request = begin("ses-b");
        assert!(matches!(
            store.snapshot_into_begin(&mut request, &["ctx-a".to_string()]),
            Err(SessionAttachmentError::SessionMismatch { .. })
        ));
    }

    #[test]
    fn detach_and_delete_session_remove_only_relations() {
        let dir = tempdir().unwrap();
        let mut store = SessionAttachmentStore::open(dir.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::ThisTurn))
            .unwrap();
        store
            .create(create("ses-a", "ctx-b", TurnContextScope::Session))
            .unwrap();
        store
            .create(create("ses-b", "ctx-c", TurnContextScope::Session))
            .unwrap();
        store.detach("ses-a", "ctx-a").unwrap();
        store.deactivate("ses-a", "ctx-b").unwrap();
        assert_eq!(store.delete_session("ses-a").unwrap().len(), 1);
        assert!(store.list("ses-a").is_empty());
        assert_eq!(store.list("ses-b").len(), 1);
    }

    #[test]
    fn future_versions_are_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(STORE_FILE_NAME);
        std::fs::write(&path, r#"{"schema_version":2,"references":[]}"#).unwrap();
        assert!(matches!(
            SessionAttachmentStore::open_file(path),
            Err(SessionAttachmentError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn version_one_relations_without_active_field_migrate_as_active() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(STORE_FILE_NAME);
        std::fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "references": [{
    "reference_id": "ctx-old",
    "session_id": "ses-a",
    "blob_id": "sha256:abc",
    "display_name": "design.pdf",
    "declared_mime": "application/pdf",
    "size": 42,
    "scope": "session",
    "created_at": 1,
    "extraction": {},
    "consumed": { "status": "available" },
    "metadata": {}
  }]
}"#,
        )
        .unwrap();

        let store = SessionAttachmentStore::open_file(path).unwrap();
        assert!(store.get("ctx-old").unwrap().active);
        assert_eq!(store.list("ses-a").len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_and_predictable_temp_cannot_escape_store() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        std::fs::write(&sentinel, b"safe").unwrap();
        symlink(outside.path(), parent.path().join("linked")).unwrap();
        assert!(SessionAttachmentStore::open(parent.path().join("linked")).is_err());

        symlink(
            &sentinel,
            parent.path().join("session-attachments.v1.json.tmp"),
        )
        .unwrap();
        let mut store = SessionAttachmentStore::open(parent.path()).unwrap();
        store
            .create(create("ses-a", "ctx-a", TurnContextScope::Session))
            .unwrap();
        assert_eq!(std::fs::read(sentinel).unwrap(), b"safe");
    }
}
