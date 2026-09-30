use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use axocoatl_core::{secure_fs::SecureDir, AgentId, TokenUsageStats};

use crate::error::MemoryError;
use crate::legacy_checkpoint::{self, LegacyCheckpointSchema};
use crate::session::StoredMessage;
#[cfg(test)]
use crate::storage::storage_path;
use crate::storage::{legacy_storage_component, storage_key};

#[path = "checkpoint_legacy_capture.rs"]
mod legacy_capture;
pub use legacy_capture::LegacySessionCheckpointSnapshot;

const CHECKPOINT_MAGIC: &[u8; 8] = b"AXOCKPT\0";
const CHECKPOINT_FORMAT_VERSION_V1: u8 = 1;
const CHECKPOINT_FORMAT_VERSION_V2: u8 = 2;
const CHECKPOINT_FORMAT_VERSION: u8 = 3;
const SESSION_TURN_TRANSACTION_FORMAT_VERSION: u8 = 1;
const SESSION_TURN_TRANSACTION_MANIFEST: &str = "transaction.json";
const SESSION_TURN_AGENT_IDENTITY: &str = "agent.json";
const SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES: usize = 16 * 1024;
/// Maximum encoded checkpoint size accepted for both current and legacy caches.
/// Canonical Session history is stored separately; a checkpoint is bounded
/// model-facing recovery state, not the product record.
pub const MAX_CHECKPOINT_BYTES: usize = legacy_checkpoint::MAX_CHECKPOINT_BYTES;

/// Return the exact current-envelope size without writing it.
pub fn encoded_checkpoint_size(checkpoint: &AgentCheckpoint) -> Result<usize, MemoryError> {
    encode_current(checkpoint).map(|bytes| bytes.len())
}

/// Return the exact Postcard size of a model-history message vector. Segment
/// sizes can be added conservatively because each separately encoded vector has
/// its own length prefix, while the combined checkpoint has only one.
pub fn encoded_checkpoint_messages_size(messages: &[StoredMessage]) -> Result<usize, MemoryError> {
    postcard::to_stdvec(messages)
        .map(|bytes| bytes.len())
        .map_err(|error| MemoryError::Serialization(error.to_string()))
}

/// Complete serializable snapshot of agent state.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentCheckpoint {
    /// Monotonically increasing version.
    pub version: u64,
    pub agent_id: String,
    pub checkpoint_time: u64,
    /// All session messages (Tier 1).
    pub session_messages: Vec<StoredMessage>,
    /// Cumulative token usage.
    pub cumulative_token_usage: TokenUsageStats,
    /// Whether cumulative usage covers every dispatched provider call. Legacy
    /// checkpoints omit this field and therefore decode conservatively as an
    /// unknown lower bound.
    #[serde(default)]
    pub cumulative_token_usage_known: bool,
    /// Agent-specific state (behavior-defined, stored as JSON).
    pub behavior_state: Option<String>,
}

/// On-disk encoding used by a successfully decoded checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointEncoding {
    /// Versioned Postcard envelope introduced for 1.0. The decoder accepts
    /// schema revisions 1, 2, and the current revision 3.
    PostcardV1,
    /// Temporary raw Postcard files written during 1.0 launch development,
    /// before the versioned envelope was introduced. Markerless bytes that
    /// are also an exact 0.1.x Bincode checkpoint use the shipped format.
    UnframedPostcard,
    LegacyBincodeV0_1_0,
    LegacyBincodeV0_1_1ThroughV0_1_4,
}

impl CheckpointEncoding {
    pub fn is_legacy(self) -> bool {
        matches!(
            self,
            Self::LegacyBincodeV0_1_0 | Self::LegacyBincodeV0_1_1ThroughV0_1_4
        )
    }

    pub fn needs_history_import(self) -> bool {
        !matches!(self, Self::PostcardV1)
    }
}

/// A checkpoint plus the information needed for a one-time safe migration.
#[derive(Debug)]
pub struct LoadedCheckpoint {
    pub checkpoint: AgentCheckpoint,
    pub encoding: CheckpointEncoding,
    /// Highest numeric checkpoint filename observed, including corrupt files.
    /// A promoted cache must use the next number so it cannot be hidden by a
    /// corrupt but higher-numbered predecessor.
    pub highest_seen_version: u64,
}

#[derive(Debug, Clone)]
struct CheckpointCandidate {
    version: u64,
    priority: u8,
    dir: SecureDir,
    name: OsString,
}

/// Checkpoint frequency policy.
#[derive(Debug, Clone)]
pub enum CheckpointPolicy {
    /// Checkpoint after every LLM response (safest).
    EveryLlmCall,
    /// Checkpoint every N messages.
    EveryNMessages(usize),
    /// Checkpoint on explicit request only.
    Manual,
    /// No checkpointing.
    None,
}

/// Durable phase of a Session-turn checkpoint transaction.
///
/// `Committing` and `Aborting` are intentionally persisted before their
/// respective file operations. A restart can therefore repeat either outcome
/// without guessing whether a partially applied filesystem mutation won.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointTransactionState {
    Pending,
    Committing,
    Aborting,
    Committed,
}

/// Canonical disposition supplied by the Session ledger during recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTransactionResolution {
    Commit,
    Abort,
}

/// One durable transaction found beneath the checkpoint store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointTransactionInfo {
    pub session_id: String,
    pub turn_id: String,
    pub state: CheckpointTransactionState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointTransactionManifest {
    format_version: u8,
    session_id: String,
    turn_id: String,
    state: CheckpointTransactionState,
}

#[derive(Debug, Clone)]
struct CheckpointTransactionScope {
    session_id: String,
    turn_id: String,
}

pub struct CheckpointStore {
    base_dir: PathBuf,
    secure_base: Option<SecureDir>,
    policy: CheckpointPolicy,
    transaction_scope: Option<CheckpointTransactionScope>,
    transaction_lock: Arc<Mutex<()>>,
}

#[path = "checkpoint_previous.rs"]
mod previous;
use previous::{AgentCheckpointPostcardV1, AgentCheckpointPostcardV2};

impl CheckpointStore {
    pub fn new(base_dir: impl Into<PathBuf>, policy: CheckpointPolicy) -> Self {
        Self {
            base_dir: base_dir.into(),
            secure_base: None,
            policy,
            transaction_scope: None,
            transaction_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Open the checkpoint store relative to an already-created data root.
    pub fn new_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
        policy: CheckpointPolicy,
    ) -> Result<Self, MemoryError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(&data_root, relative, policy)
    }

    /// Open the checkpoint store beneath the exact data-root capability owned
    /// by the process. This avoids reopening a path that may have been swapped
    /// after daemon startup.
    pub fn new_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
        policy: CheckpointPolicy,
    ) -> Result<Self, MemoryError> {
        let secure_base = data_root.child(relative)?;
        Ok(Self {
            base_dir: secure_base.path().to_path_buf(),
            secure_base: Some(secure_base),
            policy,
            transaction_scope: None,
            transaction_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Return a store view whose ordinary checkpoint writes are staged beneath
    /// one Session turn. Ordinary reads through the returned view prefer that
    /// turn's staged checkpoint and otherwise fall back to committed state.
    ///
    /// The transaction must first be created with [`Self::begin_session_turn`].
    /// An unscoped store continues to expose only committed checkpoints.
    pub fn scoped_to_session_turn(
        &self,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            base_dir: self.base_dir.clone(),
            secure_base: self.secure_base.clone(),
            policy: self.policy.clone(),
            transaction_scope: Some(CheckpointTransactionScope {
                session_id: session_id.into(),
                turn_id: turn_id.into(),
            }),
            transaction_lock: self.transaction_lock.clone(),
        }
    }

    /// Whether this store view stages ordinary checkpoint reads and writes in
    /// one Session-turn transaction instead of addressing committed state
    /// directly.
    pub fn is_session_turn_scoped(&self) -> bool {
        self.transaction_scope.is_some()
    }

    /// Create one durable pending transaction before any Session actor mutates
    /// its checkpoint. Repeating the same begin is idempotent while the
    /// transaction remains pending.
    pub async fn begin_session_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), MemoryError> {
        validate_transaction_identity("session", session_id)?;
        validate_transaction_identity("turn", turn_id)?;
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        match self.read_transaction_manifest(&base, session_id, turn_id)? {
            Some(manifest)
                if manifest.session_id == session_id
                    && manifest.turn_id == turn_id
                    && manifest.state == CheckpointTransactionState::Pending =>
            {
                Ok(())
            }
            Some(manifest) if manifest.session_id != session_id || manifest.turn_id != turn_id => {
                Err(MemoryError::Invalid(format!(
                    "checkpoint transaction identity collision for Session '{session_id}' turn '{turn_id}'"
                )))
            }
            Some(manifest) => Err(MemoryError::Invalid(format!(
                "checkpoint transaction for Session '{session_id}' turn '{turn_id}' is {:?}, not pending",
                manifest.state
            ))),
            None => self.write_transaction_manifest(
                &base,
                &CheckpointTransactionManifest {
                    format_version: SESSION_TURN_TRANSACTION_FORMAT_VERSION,
                    session_id: session_id.to_string(),
                    turn_id: turn_id.to_string(),
                    state: CheckpointTransactionState::Pending,
                },
            ),
        }
    }

    /// Stage one Agent checkpoint under an active Session turn. Staged bytes
    /// are invisible to unscoped reads until the transaction commits.
    pub async fn stage_session_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        checkpoint: &AgentCheckpoint,
    ) -> Result<(), MemoryError> {
        validate_transaction_identity("session", session_id)?;
        validate_transaction_identity("turn", turn_id)?;
        validate_transaction_identity("agent", &checkpoint.agent_id)?;
        let bytes = encode_current(checkpoint)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(MemoryError::Serialization(format!(
                "checkpoint is {} bytes; limit is {MAX_CHECKPOINT_BYTES}",
                bytes.len()
            )));
        }

        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        let manifest = self
            .read_transaction_manifest(&base, session_id, turn_id)?
            .ok_or_else(|| {
                MemoryError::NotFound(format!(
                    "checkpoint transaction for Session '{session_id}' turn '{turn_id}'"
                ))
            })?;
        require_manifest_identity(&manifest, session_id, turn_id)?;
        if manifest.state != CheckpointTransactionState::Pending {
            return Err(MemoryError::Invalid(format!(
                "cannot stage checkpoint while Session '{session_id}' turn '{turn_id}' is {:?}",
                manifest.state
            )));
        }

        let agent_dir =
            self.transaction_agent_dir(&base, session_id, turn_id, &checkpoint.agent_id, true)?;
        self.ensure_transaction_agent_identity(&agent_dir, &checkpoint.agent_id)?;

        let committed_highest = self
            .committed_checkpoint_candidates(&base, &AgentId::new(&checkpoint.agent_id))?
            .into_iter()
            .map(|candidate| candidate.version)
            .max()
            .unwrap_or_default();
        let staged_highest = checkpoint_candidates(&agent_dir, 2)?
            .into_iter()
            .map(|candidate| candidate.version)
            .max()
            .unwrap_or_default();
        let highest = committed_highest.max(staged_highest);
        let name = Self::checkpoint_name(checkpoint.version);
        if agent_dir.is_file(&name)? {
            let existing = agent_dir.read_limited(&name, MAX_CHECKPOINT_BYTES)?;
            if existing == bytes {
                return Ok(());
            }
            return Err(MemoryError::Invalid(format!(
                "checkpoint version {} for Agent '{}' already contains different staged bytes",
                checkpoint.version, checkpoint.agent_id
            )));
        }
        if checkpoint.version <= highest {
            return Err(MemoryError::Invalid(format!(
                "staged checkpoint version {} for Agent '{}' must be greater than existing version {highest}",
                checkpoint.version, checkpoint.agent_id
            )));
        }

        agent_dir.atomic_write(name, &bytes)?;
        self.prune_old(&agent_dir, 3)?;
        tracing::debug!(
            session = %session_id,
            turn = %turn_id,
            agent = %checkpoint.agent_id,
            version = checkpoint.version,
            bytes = bytes.len(),
            "Session-turn checkpoint staged"
        );
        Ok(())
    }

    /// Promote every staged Agent checkpoint after the matching Session turn's
    /// completed terminal transition is durable. The operation is idempotent:
    /// `Committing` is persisted first and a restart can repeat every copy.
    pub async fn commit_session_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), MemoryError> {
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        self.commit_session_turn_locked(&base, session_id, turn_id)
    }

    /// Discard every staged checkpoint while retaining the prior committed
    /// baseline. This is the terminal path for failed, cancelled, or
    /// interrupted Session turns.
    pub async fn abort_session_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), MemoryError> {
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        self.abort_session_turn_locked(&base, session_id, turn_id)
    }

    /// Resolve an unfinished filesystem transaction from the canonical Session
    /// ledger. Both outcomes are safe to repeat after any process death.
    pub async fn reconcile_session_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        resolution: CheckpointTransactionResolution,
    ) -> Result<(), MemoryError> {
        match resolution {
            CheckpointTransactionResolution::Commit => {
                self.commit_session_turn(session_id, turn_id).await
            }
            CheckpointTransactionResolution::Abort => {
                self.abort_session_turn(session_id, turn_id).await
            }
        }
    }

    /// Enumerate durable transactions so bootstrap can resolve each one against
    /// its canonical Session-turn lifecycle before serving requests.
    pub fn list_session_turn_transactions(
        &self,
    ) -> Result<Vec<CheckpointTransactionInfo>, MemoryError> {
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        let root = match base.existing_child(Self::transaction_root()) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut transactions = Vec::new();
        for session_entry in root.entries()? {
            if session_entry.file_type != axocoatl_core::secure_fs::SecureEntryType::Directory {
                return Err(MemoryError::Invalid(format!(
                    "checkpoint transaction root contains non-directory entry {:?}",
                    session_entry.name
                )));
            }
            let session_dir = root.existing_child(Path::new(&session_entry.name))?;
            for turn_entry in session_dir.entries()? {
                if turn_entry.file_type != axocoatl_core::secure_fs::SecureEntryType::Directory {
                    return Err(MemoryError::Invalid(format!(
                        "checkpoint transaction Session directory contains non-directory entry {:?}",
                        turn_entry.name
                    )));
                }
                let turn_dir = session_dir.existing_child(Path::new(&turn_entry.name))?;
                let Some(manifest) = read_manifest_from_dir(&turn_dir)? else {
                    if turn_dir.entries()?.is_empty() {
                        continue;
                    }
                    return Err(MemoryError::Invalid(format!(
                        "checkpoint transaction at '{}' has no manifest",
                        turn_dir.path().display()
                    )));
                };
                require_manifest_storage_keys(&manifest, &session_entry.name, &turn_entry.name)?;
                transactions.push(CheckpointTransactionInfo {
                    session_id: manifest.session_id,
                    turn_id: manifest.turn_id,
                    state: manifest.state,
                });
            }
        }
        transactions.sort_by(|left, right| {
            left.session_id
                .cmp(&right.session_id)
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        Ok(transactions)
    }

    /// One-time adoption for checkpoints created before Session-turn
    /// transactions existed. Every committed Agent whose logical identity has
    /// the exact `{session_id}:` prefix receives an accounting-only successor:
    /// cumulative provider usage is retained, while transcript and private
    /// behavior state are cleared. Repeating this operation is idempotent.
    ///
    /// The caller owns the durable per-Session adoption marker and must write
    /// it only after this operation returns successfully.
    pub async fn sanitize_committed_session_prefix(
        &self,
        session_id: &str,
    ) -> Result<usize, MemoryError> {
        validate_transaction_identity("session", session_id)?;
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        let prefix = format!("{session_id}:");
        let agent_ids = discover_committed_agent_identities(&base)?;
        let mut sanitized = 0;
        for agent_id in agent_ids
            .into_iter()
            .filter(|agent_id| agent_id.starts_with(&prefix))
        {
            let agent = AgentId::new(&agent_id);
            let candidates = self.committed_checkpoint_candidates(&base, &agent)?;
            let Some(loaded) = load_latest_from_candidates(&agent, candidates)? else {
                continue;
            };
            if loaded.checkpoint.session_messages.is_empty()
                && loaded.checkpoint.behavior_state.is_none()
            {
                continue;
            }
            let version = loaded.highest_seen_version.checked_add(1).ok_or_else(|| {
                MemoryError::Invalid(format!(
                    "cannot sanitize checkpoint for Agent '{agent_id}' because its version is exhausted"
                ))
            })?;
            self.save_committed_locked(
                &base,
                &AgentCheckpoint {
                    version,
                    agent_id: agent_id.clone(),
                    checkpoint_time: loaded.checkpoint.checkpoint_time,
                    session_messages: Vec::new(),
                    cumulative_token_usage: loaded.checkpoint.cumulative_token_usage,
                    cumulative_token_usage_known: loaded.checkpoint.cumulative_token_usage_known,
                    behavior_state: None,
                },
            )?;
            sanitized += 1;
        }
        Ok(sanitized)
    }

    fn transaction_guard(&self) -> Result<std::sync::MutexGuard<'_, ()>, MemoryError> {
        self.transaction_lock.lock().map_err(|_| {
            MemoryError::Invalid("checkpoint transaction lock is poisoned".to_string())
        })
    }

    fn transaction_root() -> PathBuf {
        Path::new("session-turn-transactions").join("v1")
    }

    fn transaction_relative(session_id: &str, turn_id: &str) -> PathBuf {
        Self::transaction_root()
            .join(storage_key(session_id))
            .join(storage_key(turn_id))
    }

    fn transaction_dir(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
        create: bool,
    ) -> Result<SecureDir, MemoryError> {
        let relative = Self::transaction_relative(session_id, turn_id);
        if create {
            base.child(relative).map_err(Into::into)
        } else {
            base.existing_child(relative).map_err(Into::into)
        }
    }

    fn transaction_agent_dir(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
        agent_id: &str,
        create: bool,
    ) -> Result<SecureDir, MemoryError> {
        let relative = Self::transaction_relative(session_id, turn_id)
            .join("agents")
            .join(storage_key(agent_id));
        if create {
            base.child(relative).map_err(Into::into)
        } else {
            base.existing_child(relative).map_err(Into::into)
        }
    }

    fn read_transaction_manifest(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<CheckpointTransactionManifest>, MemoryError> {
        let dir = match self.transaction_dir(base, session_id, turn_id, false) {
            Ok(dir) => dir,
            Err(MemoryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        read_manifest_from_dir(&dir)
    }

    fn write_transaction_manifest(
        &self,
        base: &SecureDir,
        manifest: &CheckpointTransactionManifest,
    ) -> Result<(), MemoryError> {
        let bytes = serde_json::to_vec(manifest)?;
        if bytes.len() > SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES {
            return Err(MemoryError::Serialization(format!(
                "checkpoint transaction manifest is {} bytes; limit is {SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES}",
                bytes.len()
            )));
        }
        self.transaction_dir(base, &manifest.session_id, &manifest.turn_id, true)?
            .atomic_write(SESSION_TURN_TRANSACTION_MANIFEST, &bytes)?;
        Ok(())
    }

    fn ensure_transaction_agent_identity(
        &self,
        agent_dir: &SecureDir,
        agent_id: &str,
    ) -> Result<(), MemoryError> {
        let bytes = serde_json::to_vec(agent_id)?;
        let has_identity = validate_transaction_agent_entries(agent_dir)?;
        if has_identity {
            let existing = agent_dir.read_limited(
                SESSION_TURN_AGENT_IDENTITY,
                SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES,
            )?;
            let decoded: String = serde_json::from_slice(&existing)?;
            if decoded != agent_id {
                return Err(MemoryError::Invalid(format!(
                    "staged checkpoint directory belongs to Agent '{decoded}', not '{agent_id}'"
                )));
            }
            return Ok(());
        }
        if !agent_dir.entries()?.is_empty() {
            return Err(MemoryError::Invalid(format!(
                "staged Agent directory '{}' contains checkpoints without a durable Agent identity",
                agent_dir.path().display()
            )));
        }
        agent_dir.atomic_write(SESSION_TURN_AGENT_IDENTITY, &bytes)?;
        Ok(())
    }

    fn committed_checkpoint_candidates(
        &self,
        base: &SecureDir,
        agent_id: &AgentId,
    ) -> Result<Vec<CheckpointCandidate>, MemoryError> {
        let current_key = storage_key(&agent_id.0);
        let mut candidates = Vec::new();
        match base.existing_child(Path::new("v1").join(&current_key)) {
            Ok(dir) => candidates.extend(checkpoint_candidates(&dir, 1)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(legacy) = legacy_storage_component(&agent_id.0) {
            if base.has_exact_directory(legacy)? {
                candidates.extend(checkpoint_candidates(&base.existing_child(legacy)?, 0)?);
            }
        }
        Ok(candidates)
    }

    fn save_committed_locked(
        &self,
        base: &SecureDir,
        checkpoint: &AgentCheckpoint,
    ) -> Result<(), MemoryError> {
        let dir = base.child(Path::new("v1").join(storage_key(&checkpoint.agent_id)))?;
        let bytes = encode_current(checkpoint)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(MemoryError::Serialization(format!(
                "checkpoint is {} bytes; limit is {MAX_CHECKPOINT_BYTES}",
                bytes.len()
            )));
        }
        let name = Self::checkpoint_name(checkpoint.version);
        if dir.is_file(&name)? {
            let existing = dir.read_limited(&name, MAX_CHECKPOINT_BYTES)?;
            if existing != bytes {
                return Err(MemoryError::Invalid(format!(
                    "committed checkpoint version {} for Agent '{}' already contains different bytes",
                    checkpoint.version, checkpoint.agent_id
                )));
            }
        } else {
            // Checkpoints hold full message + tool I/O verbatim. SecureDir
            // fsyncs the unpredictable temporary before atomic replacement.
            dir.atomic_write(&name, &bytes)?;
        }
        self.prune_old(&dir, 3)?;
        tracing::debug!(
            agent = %checkpoint.agent_id,
            version = checkpoint.version,
            bytes = bytes.len(),
            "Checkpoint committed"
        );
        Ok(())
    }

    fn transaction_checkpoints(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Vec<LoadedCheckpoint>, MemoryError> {
        let mut checkpoints = Vec::new();
        for (agent_id, agent_dir) in self.transaction_agents(base, session_id, turn_id, false)? {
            if let Some(loaded) = load_latest_from_candidates(
                &AgentId::new(&agent_id),
                checkpoint_candidates(&agent_dir, 2)?,
            )? {
                checkpoints.push(loaded);
            }
        }
        Ok(checkpoints)
    }

    fn commit_session_turn_locked(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), MemoryError> {
        validate_transaction_identity("session", session_id)?;
        validate_transaction_identity("turn", turn_id)?;
        let Some(mut manifest) = self.read_transaction_manifest(base, session_id, turn_id)? else {
            return Ok(());
        };
        require_manifest_identity(&manifest, session_id, turn_id)?;
        let checkpoints = match manifest.state {
            CheckpointTransactionState::Pending => {
                // Validate and decode the complete staged set before publishing
                // Committing. A malformed or noncanonical filename therefore
                // fails while the transaction is still safely retryable.
                let checkpoints =
                    self.transaction_checkpoints(base, session_id, turn_id)?;
                manifest.state = CheckpointTransactionState::Committing;
                self.write_transaction_manifest(base, &manifest)?;
                checkpoints
            }
            CheckpointTransactionState::Committing => {
                self.transaction_checkpoints(base, session_id, turn_id)?
            }
            CheckpointTransactionState::Committed => {
                self.cleanup_transaction(base, session_id, turn_id)?;
                return Ok(());
            }
            CheckpointTransactionState::Aborting => {
                return Err(MemoryError::Invalid(format!(
                    "cannot commit checkpoint transaction for Session '{session_id}' turn '{turn_id}' after abort began"
                )))
            }
        };

        for loaded in checkpoints {
            self.save_committed_locked(base, &loaded.checkpoint)?;
        }
        manifest.state = CheckpointTransactionState::Committed;
        self.write_transaction_manifest(base, &manifest)?;
        self.cleanup_transaction(base, session_id, turn_id)?;
        Ok(())
    }

    fn abort_session_turn_locked(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), MemoryError> {
        validate_transaction_identity("session", session_id)?;
        validate_transaction_identity("turn", turn_id)?;
        let Some(mut manifest) = self.read_transaction_manifest(base, session_id, turn_id)? else {
            return Ok(());
        };
        require_manifest_identity(&manifest, session_id, turn_id)?;
        match manifest.state {
            CheckpointTransactionState::Pending => {
                manifest.state = CheckpointTransactionState::Aborting;
                self.write_transaction_manifest(base, &manifest)?;
            }
            CheckpointTransactionState::Aborting => {}
            CheckpointTransactionState::Committed | CheckpointTransactionState::Committing => {
                return Err(MemoryError::Invalid(format!(
                    "cannot abort checkpoint transaction for Session '{session_id}' turn '{turn_id}' while it is {:?}",
                    manifest.state
                )))
            }
        }

        // A failed/cancelled/interrupted turn must not retain its transcript or
        // private behavior state. Provider usage was nevertheless incurred, so
        // publish an accounting-only checkpoint over the last committed
        // baseline before discarding the pending tree.
        for (agent_id, agent_dir) in self.transaction_agents(base, session_id, turn_id, true)? {
            let agent = AgentId::new(&agent_id);
            let Some(pending) =
                load_latest_from_candidates(&agent, checkpoint_candidates(&agent_dir, 2)?)?
            else {
                continue;
            };
            let committed_candidates = self.committed_checkpoint_candidates(base, &agent)?;
            let committed = load_latest_from_candidates(&agent, committed_candidates.clone())?;
            let baseline = load_latest_from_candidates(
                &agent,
                committed_candidates
                    .iter()
                    .filter(|candidate| candidate.version <= pending.checkpoint.version)
                    .cloned()
                    .collect(),
            )?;
            if let Some(existing) = committed
                .as_ref()
                .filter(|existing| existing.checkpoint.version > pending.checkpoint.version)
            {
                let baseline_messages = baseline
                    .as_ref()
                    .map(|loaded| loaded.checkpoint.session_messages.as_slice())
                    .unwrap_or_default();
                if existing.checkpoint.cumulative_token_usage
                    == pending.checkpoint.cumulative_token_usage
                    && existing.checkpoint.cumulative_token_usage_known
                        == pending.checkpoint.cumulative_token_usage_known
                    && existing.checkpoint.behavior_state.is_none()
                    && checkpoint_messages_equal(
                        &existing.checkpoint.session_messages,
                        baseline_messages,
                    )?
                {
                    continue;
                }
                return Err(MemoryError::Invalid(format!(
                    "committed checkpoint for Agent '{agent_id}' advanced past an aborting Session turn with different accounting"
                )));
            }
            let committed_highest = committed_candidates
                .iter()
                .map(|candidate| candidate.version)
                .max()
                .unwrap_or_default();
            let next_version = committed_highest
                .max(pending.highest_seen_version)
                .saturating_add(1);
            let accounting = AgentCheckpoint {
                version: next_version,
                agent_id: agent_id.clone(),
                checkpoint_time: pending.checkpoint.checkpoint_time,
                session_messages: baseline
                    .as_ref()
                    .map(|loaded| loaded.checkpoint.session_messages.clone())
                    .unwrap_or_default(),
                cumulative_token_usage: pending.checkpoint.cumulative_token_usage.clone(),
                cumulative_token_usage_known: pending.checkpoint.cumulative_token_usage_known,
                behavior_state: None,
            };
            self.save_committed_locked(base, &accounting)?;
        }
        self.cleanup_transaction(base, session_id, turn_id)?;
        Ok(())
    }

    fn transaction_agents(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
        allow_incomplete_identity: bool,
    ) -> Result<Vec<(String, SecureDir)>, MemoryError> {
        let turn_dir = self.transaction_dir(base, session_id, turn_id, false)?;
        let agents = match turn_dir.existing_child("agents") {
            Ok(agents) => agents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut result = Vec::new();
        for entry in agents.entries()? {
            if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::Directory {
                return Err(MemoryError::Invalid(format!(
                    "checkpoint transaction agents directory contains non-directory entry {:?}",
                    entry.name
                )));
            }
            let dir = agents.existing_child(Path::new(&entry.name))?;
            let has_identity = validate_transaction_agent_entries(&dir)?;
            if !has_identity {
                if allow_incomplete_identity && dir.entries()?.is_empty() {
                    drop(dir);
                    agents.remove_empty_dir(Path::new(&entry.name))?;
                    continue;
                }
                return Err(MemoryError::Invalid(format!(
                    "staged Agent directory '{}' has no durable Agent identity",
                    dir.path().display()
                )));
            }
            let identity = dir.read_limited(
                SESSION_TURN_AGENT_IDENTITY,
                SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES,
            )?;
            let agent_id: String = serde_json::from_slice(&identity)?;
            if OsString::from(storage_key(&agent_id)) != entry.name {
                return Err(MemoryError::Invalid(format!(
                    "staged Agent identity '{agent_id}' does not match its storage key"
                )));
            }
            result.push((agent_id, dir));
        }
        result.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(result)
    }

    fn cleanup_transaction(
        &self,
        base: &SecureDir,
        session_id: &str,
        turn_id: &str,
    ) -> Result<(), MemoryError> {
        let relative = Self::transaction_relative(session_id, turn_id);
        let turn_dir = match base.existing_child(&relative) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        // Keep the manifest present while recursively deleting staged data. If
        // the process dies during that deletion, startup still sees the durable
        // disposition and can repeat it. Once only the manifest remains, its
        // fsynced unlink makes an empty leftover directory equivalent to fully
        // cleaned state.
        match turn_dir.remove_dir_all("agents") {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        remove_atomic_write_temps(&turn_dir)?;
        for entry in turn_dir.entries()? {
            if entry.name != std::ffi::OsStr::new(SESSION_TURN_TRANSACTION_MANIFEST)
                || entry.file_type != axocoatl_core::secure_fs::SecureEntryType::File
            {
                return Err(MemoryError::Invalid(format!(
                    "checkpoint transaction at '{}' contains unexpected entry {:?} after resolution",
                    turn_dir.path().display(),
                    entry.name
                )));
            }
        }
        if turn_dir.is_file(SESSION_TURN_TRANSACTION_MANIFEST)? {
            turn_dir.remove_leaf(SESSION_TURN_TRANSACTION_MANIFEST)?;
        }
        match base.remove_empty_dir(&relative) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let session_relative = Self::transaction_root().join(storage_key(session_id));
        if let Ok(session_dir) = base.existing_child(&session_relative) {
            if session_dir.entries()?.is_empty() {
                let _ = base.remove_empty_dir(session_relative);
            }
        }
        Ok(())
    }

    /// Whether an automatic checkpoint should be written now, given the
    /// session's current message count. Honors the configured
    /// [`CheckpointPolicy`]: `EveryLlmCall` always checkpoints,
    /// `EveryNMessages(n)` every `n` messages, and `Manual`/`None` never
    /// auto-checkpoint (an explicit [`CheckpointStore::save`] still works).
    pub fn should_checkpoint(&self, message_count: usize) -> bool {
        match &self.policy {
            CheckpointPolicy::EveryLlmCall => true,
            CheckpointPolicy::EveryNMessages(n) => *n > 0 && message_count.is_multiple_of(*n),
            CheckpointPolicy::Manual | CheckpointPolicy::None => false,
        }
    }

    /// Save a versioned Postcard checkpoint using an atomic replacement.
    pub async fn save(&self, checkpoint: &AgentCheckpoint) -> Result<(), MemoryError> {
        if let Some(scope) = &self.transaction_scope {
            return self
                .stage_session_turn(&scope.session_id, &scope.turn_id, checkpoint)
                .await;
        }
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        self.save_committed_locked(&base, checkpoint)
    }

    /// Remove one exact checkpoint version. Used only to roll back a prepared
    /// cache projection when the canonical transaction it accompanies cannot
    /// commit. Missing versions are already equivalent to rolled back.
    pub async fn remove_version(&self, agent_id: &str, version: u64) -> Result<(), MemoryError> {
        let base = self.secure_base()?;
        let dir = match base.existing_child(Path::new("v1").join(storage_key(agent_id))) {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let name = Self::checkpoint_name(version);
        if dir.is_file(&name)? {
            dir.remove_file(name)?;
        }
        Ok(())
    }

    /// Load the most recent valid checkpoint for an agent.
    pub async fn load_latest(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentCheckpoint>, MemoryError> {
        Ok(self
            .load_latest_with_encoding(agent_id)
            .await?
            .map(|loaded| loaded.checkpoint))
    }

    /// Load the newest valid checkpoint and report its on-disk encoding.
    ///
    /// Candidates are tried in descending filename order. A corrupt newest
    /// cache cannot hide an older valid transcript. Legacy Bincode is decoded
    /// only through the private, size-limited 0.1.x compatibility module; this
    /// function never rewrites or removes the sole legacy source.
    pub async fn load_latest_with_encoding(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<LoadedCheckpoint>, MemoryError> {
        let _guard = self.transaction_guard()?;
        let base = self.secure_base()?;
        let mut candidates = self.committed_checkpoint_candidates(&base, agent_id)?;
        if let Some(scope) = &self.transaction_scope {
            let manifest = self
                .read_transaction_manifest(&base, &scope.session_id, &scope.turn_id)?
                .ok_or_else(|| {
                    MemoryError::NotFound(format!(
                        "checkpoint transaction for Session '{}' turn '{}'",
                        scope.session_id, scope.turn_id
                    ))
                })?;
            require_manifest_identity(&manifest, &scope.session_id, &scope.turn_id)?;
            if manifest.state != CheckpointTransactionState::Pending {
                return Err(MemoryError::Invalid(format!(
                    "cannot read scoped checkpoint while Session '{}' turn '{}' is {:?}",
                    scope.session_id, scope.turn_id, manifest.state
                )));
            }
            match self.transaction_agent_dir(
                &base,
                &scope.session_id,
                &scope.turn_id,
                &agent_id.0,
                false,
            ) {
                Ok(agent_dir) => {
                    verify_transaction_agent_identity(&agent_dir, &agent_id.0)?;
                    candidates.extend(checkpoint_candidates(&agent_dir, 2)?);
                }
                Err(MemoryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        load_latest_from_candidates(agent_id, candidates)
    }

    fn checkpoint_name(version: u64) -> String {
        format!("{version:016}.ckpt")
    }

    fn prune_old(&self, dir: &SecureDir, keep: usize) -> Result<(), MemoryError> {
        let mut versions: Vec<(u64, OsString)> = vec![];
        for entry in dir.entries()? {
            if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::File {
                continue;
            }
            let path = Path::new(&entry.name);
            if let Some(version) = checkpoint_filename_version(path) {
                versions.push((version, entry.name));
            }
        }

        versions.sort_by_key(|(v, _)| *v);
        if versions.len() > keep {
            for (_, name) in versions.iter().take(versions.len() - keep) {
                dir.remove_file(name).ok();
            }
        }
        Ok(())
    }

    fn secure_base(&self) -> Result<SecureDir, MemoryError> {
        self.secure_base.clone().map(Ok).unwrap_or_else(|| {
            SecureDir::open_or_create_all(&self.base_dir).map_err(MemoryError::from)
        })
    }
}

fn validate_transaction_identity(label: &str, value: &str) -> Result<(), MemoryError> {
    if value.is_empty() {
        return Err(MemoryError::Invalid(format!(
            "checkpoint transaction {label} identity is empty"
        )));
    }
    if value.len() > 4096 {
        return Err(MemoryError::Invalid(format!(
            "checkpoint transaction {label} identity is too long"
        )));
    }
    Ok(())
}

fn checkpoint_messages_equal(
    left: &[StoredMessage],
    right: &[StoredMessage],
) -> Result<bool, MemoryError> {
    let left =
        postcard::to_stdvec(left).map_err(|error| MemoryError::Serialization(error.to_string()))?;
    let right = postcard::to_stdvec(right)
        .map_err(|error| MemoryError::Serialization(error.to_string()))?;
    Ok(left == right)
}

fn discover_committed_agent_identities(base: &SecureDir) -> Result<BTreeSet<String>, MemoryError> {
    let mut identities = BTreeSet::new();

    match base.existing_child("v1") {
        Ok(current_root) => {
            for entry in current_root.entries()? {
                if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::Directory {
                    continue;
                }
                let dir = current_root.existing_child(Path::new(&entry.name))?;
                let Some(agent_id) = discover_checkpoint_identity(&dir, &entry.name)? else {
                    continue;
                };
                if OsString::from(storage_key(&agent_id)) != entry.name {
                    return Err(MemoryError::Invalid(format!(
                        "checkpoint for Agent '{agent_id}' is stored beneath a different Agent key"
                    )));
                }
                identities.insert(agent_id);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // On POSIX, pre-1.0 scoped identities were literal one-component
    // directories. Verify their payload identity through the normal legacy
    // reader before exposing them to prefix adoption.
    for entry in base.entries()? {
        if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::Directory {
            continue;
        }
        let Some(agent_id) = entry.name.to_str() else {
            continue;
        };
        if agent_id == "v1" || agent_id == "session-turn-transactions" {
            continue;
        }
        if legacy_storage_component(agent_id) != Some(agent_id) {
            continue;
        }
        let dir = base.existing_child(Path::new(&entry.name))?;
        let agent = AgentId::new(agent_id);
        if load_latest_from_candidates(&agent, checkpoint_candidates(&dir, 0)?)?.is_some() {
            identities.insert(agent_id.to_string());
        }
    }

    Ok(identities)
}

fn discover_checkpoint_identity(
    dir: &SecureDir,
    expected_storage_key: &std::ffi::OsStr,
) -> Result<Option<String>, MemoryError> {
    let mut candidates = checkpoint_candidates(dir, 1)?;
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.version));
    for candidate in candidates {
        let path = candidate.dir.path().join(&candidate.name);
        let bytes = match candidate
            .dir
            .read_limited(&candidate.name, MAX_CHECKPOINT_BYTES)
        {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Could not read checkpoint while discovering legacy Session ownership"
                );
                continue;
            }
        };
        let decoded = if bytes.starts_with(CHECKPOINT_MAGIC) {
            vec![decode_current(&bytes)]
        } else {
            vec![
                legacy_checkpoint::decode(&bytes).map(|(checkpoint, _)| checkpoint),
                decode_postcard_payload(&bytes),
            ]
        };
        let mut decoded_version = false;
        let mut errors = Vec::new();
        for decoded in decoded {
            match decoded {
                Ok(checkpoint) if checkpoint.version == candidate.version => {
                    decoded_version = true;
                    if OsString::from(storage_key(&checkpoint.agent_id)) == expected_storage_key {
                        return Ok(Some(checkpoint.agent_id));
                    }
                }
                Ok(checkpoint) => errors.push(format!(
                    "payload version {} does not match filename version {}",
                    checkpoint.version, candidate.version
                )),
                Err(error) => errors.push(error),
            }
        }
        if decoded_version {
            return Err(MemoryError::Invalid(format!(
                "checkpoint at '{}' contains an Agent identity that does not match its storage key",
                path.display()
            )));
        }
        tracing::warn!(
            path = %path.display(),
            errors = ?errors,
            "Checkpoint could not be decoded while discovering legacy Session ownership"
        );
    }
    Ok(None)
}

fn read_manifest_from_dir(
    dir: &SecureDir,
) -> Result<Option<CheckpointTransactionManifest>, MemoryError> {
    remove_atomic_write_temps(dir)?;
    for entry in dir.entries()? {
        let is_manifest = entry.name == std::ffi::OsStr::new(SESSION_TURN_TRANSACTION_MANIFEST)
            && entry.file_type == axocoatl_core::secure_fs::SecureEntryType::File;
        let is_agents = entry.name == std::ffi::OsStr::new("agents")
            && entry.file_type == axocoatl_core::secure_fs::SecureEntryType::Directory;
        if !is_manifest && !is_agents {
            return Err(MemoryError::Invalid(format!(
                "checkpoint transaction at '{}' contains unexpected entry {:?}",
                dir.path().display(),
                entry.name
            )));
        }
    }
    if !dir.is_file(SESSION_TURN_TRANSACTION_MANIFEST)? {
        return Ok(None);
    }
    let bytes = dir.read_limited(
        SESSION_TURN_TRANSACTION_MANIFEST,
        SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES,
    )?;
    let manifest: CheckpointTransactionManifest = serde_json::from_slice(&bytes)?;
    if manifest.format_version != SESSION_TURN_TRANSACTION_FORMAT_VERSION {
        return Err(MemoryError::Invalid(format!(
            "unsupported checkpoint transaction format version {}",
            manifest.format_version
        )));
    }
    validate_transaction_identity("session", &manifest.session_id)?;
    validate_transaction_identity("turn", &manifest.turn_id)?;
    Ok(Some(manifest))
}

fn require_manifest_identity(
    manifest: &CheckpointTransactionManifest,
    session_id: &str,
    turn_id: &str,
) -> Result<(), MemoryError> {
    if manifest.session_id != session_id || manifest.turn_id != turn_id {
        return Err(MemoryError::Invalid(format!(
            "checkpoint transaction manifest identifies Session '{}' turn '{}', not Session '{session_id}' turn '{turn_id}'",
            manifest.session_id, manifest.turn_id
        )));
    }
    Ok(())
}

fn require_manifest_storage_keys(
    manifest: &CheckpointTransactionManifest,
    session_key: &std::ffi::OsStr,
    turn_key: &std::ffi::OsStr,
) -> Result<(), MemoryError> {
    if OsString::from(storage_key(&manifest.session_id)) != session_key
        || OsString::from(storage_key(&manifest.turn_id)) != turn_key
    {
        return Err(MemoryError::Invalid(format!(
            "checkpoint transaction manifest identity does not match its storage path for Session '{}' turn '{}'",
            manifest.session_id, manifest.turn_id
        )));
    }
    Ok(())
}

fn verify_transaction_agent_identity(
    agent_dir: &SecureDir,
    expected_agent_id: &str,
) -> Result<(), MemoryError> {
    if !validate_transaction_agent_entries(agent_dir)? {
        return Err(MemoryError::Invalid(format!(
            "staged Agent directory '{}' has no durable Agent identity",
            agent_dir.path().display()
        )));
    }
    let bytes = agent_dir.read_limited(
        SESSION_TURN_AGENT_IDENTITY,
        SESSION_TURN_TRANSACTION_MANIFEST_MAX_BYTES,
    )?;
    let found: String = serde_json::from_slice(&bytes)?;
    if found != expected_agent_id
        || OsString::from(storage_key(&found))
            != agent_dir.path().file_name().ok_or_else(|| {
                MemoryError::Invalid("staged Agent directory has no name".to_string())
            })?
    {
        return Err(MemoryError::Invalid(format!(
            "staged checkpoint Agent identity '{found}' does not match expected Agent '{expected_agent_id}' or its storage path"
        )));
    }
    Ok(())
}

fn validate_transaction_agent_entries(agent_dir: &SecureDir) -> Result<bool, MemoryError> {
    remove_atomic_write_temps(agent_dir)?;
    let mut has_identity = false;
    for entry in agent_dir.entries()? {
        let is_identity = entry.name == std::ffi::OsStr::new(SESSION_TURN_AGENT_IDENTITY)
            && entry.file_type == axocoatl_core::secure_fs::SecureEntryType::File;
        let is_checkpoint = entry.file_type == axocoatl_core::secure_fs::SecureEntryType::File
            && checkpoint_filename_version(Path::new(&entry.name)).is_some();
        if !is_identity && !is_checkpoint {
            return Err(MemoryError::Invalid(format!(
                "staged Agent directory '{}' contains unexpected entry {:?}",
                agent_dir.path().display(),
                entry.name
            )));
        }
        has_identity |= is_identity;
    }
    Ok(has_identity)
}

fn remove_atomic_write_temps(dir: &SecureDir) -> Result<usize, MemoryError> {
    let mut removed = 0;
    for entry in dir.entries()? {
        if !is_atomic_write_temp_name(&entry.name) {
            continue;
        }
        if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::File {
            return Err(MemoryError::Invalid(format!(
                "atomic-write temporary entry {:?} in '{}' is not a regular file",
                entry.name,
                dir.path().display()
            )));
        }
        dir.remove_leaf(Path::new(&entry.name))?;
        removed += 1;
    }
    Ok(removed)
}

fn is_atomic_write_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(value) = name
        .strip_prefix(".axocoatl-")
        .and_then(|value| value.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Ok(id) = uuid::Uuid::parse_str(value) else {
        return false;
    };
    id.get_version_num() == 4 && id.hyphenated().to_string() == value
}

fn checkpoint_filename_version(path: &Path) -> Option<u64> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("ckpt") {
        return None;
    }
    let version = path
        .file_stem()
        .and_then(|stem| stem.to_str())?
        .parse::<u64>()
        .ok()?;
    let canonical = CheckpointStore::checkpoint_name(version);
    (path.file_name() == Some(std::ffi::OsStr::new(&canonical))).then_some(version)
}

fn checkpoint_candidates(
    dir: &SecureDir,
    priority: u8,
) -> Result<Vec<CheckpointCandidate>, MemoryError> {
    let mut candidates = Vec::new();
    for entry in dir.entries()? {
        if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::File {
            continue;
        }
        let path = Path::new(&entry.name);
        let Some(version) = checkpoint_filename_version(path) else {
            continue;
        };
        candidates.push(CheckpointCandidate {
            version,
            priority,
            dir: dir.clone(),
            name: entry.name,
        });
    }
    Ok(candidates)
}

fn load_latest_from_candidates(
    agent_id: &AgentId,
    mut candidates: Vec<CheckpointCandidate>,
) -> Result<Option<LoadedCheckpoint>, MemoryError> {
    // Prefer staged over current over legacy when the same version somehow
    // exists in multiple locations, then fall back by numeric version.
    candidates.sort_by(|left, right| {
        right
            .version
            .cmp(&left.version)
            .then_with(|| right.priority.cmp(&left.priority))
    });
    let Some(highest_seen_version) = candidates.first().map(|candidate| candidate.version) else {
        return Ok(None);
    };

    for candidate in candidates {
        let path = candidate.dir.path().join(&candidate.name);
        let bytes_len = match candidate.dir.file_len(&candidate.name) {
            Ok(length) => length,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Could not inspect checkpoint candidate; trying an older version"
                );
                continue;
            }
        };
        if bytes_len > MAX_CHECKPOINT_BYTES as u64 {
            tracing::warn!(
                path = %path.display(),
                bytes = bytes_len,
                limit = MAX_CHECKPOINT_BYTES,
                "Checkpoint candidate exceeds the decode limit; trying an older version"
            );
            continue;
        }
        let bytes = match candidate.dir.read(&candidate.name) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Could not read checkpoint candidate; trying an older version"
                );
                continue;
            }
        };
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            tracing::warn!(
                path = %path.display(),
                bytes = bytes.len(),
                limit = MAX_CHECKPOINT_BYTES,
                "Checkpoint candidate exceeds the decode limit; trying an older version"
            );
            continue;
        }

        let decoded = if bytes.starts_with(CHECKPOINT_MAGIC) {
            decode_current(&bytes).map(|checkpoint| (checkpoint, CheckpointEncoding::PostcardV1))
        } else {
            decode_unframed(&bytes, agent_id, candidate.version)
        };
        match decoded.and_then(|(checkpoint, encoding)| {
            validate_identity(&checkpoint, agent_id, candidate.version)?;
            Ok((checkpoint, encoding))
        }) {
            Ok((checkpoint, encoding)) => {
                return Ok(Some(LoadedCheckpoint {
                    checkpoint,
                    encoding,
                    highest_seen_version,
                }));
            }
            Err(error) => {
                // A checkpoint is a rebuildable execution cache. Do not brick
                // the agent or hide an older valid transcript because one
                // candidate is corrupt.
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Checkpoint candidate failed validation; trying an older version"
                );
            }
        }
    }
    Ok(None)
}

pub(crate) fn encode_current(checkpoint: &AgentCheckpoint) -> Result<Vec<u8>, MemoryError> {
    let payload = postcard::to_stdvec(checkpoint)
        .map_err(|error| MemoryError::Serialization(error.to_string()))?;
    let mut bytes = Vec::with_capacity(CHECKPOINT_MAGIC.len() + 1 + payload.len());
    bytes.extend_from_slice(CHECKPOINT_MAGIC);
    bytes.push(CHECKPOINT_FORMAT_VERSION);
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

pub(crate) fn decode_current(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
    let envelope = bytes
        .get(CHECKPOINT_MAGIC.len()..)
        .ok_or_else(|| "checkpoint envelope is truncated".to_string())?;
    let Some((&format_version, payload)) = envelope.split_first() else {
        return Err("checkpoint envelope is missing its format version".to_string());
    };
    match format_version {
        CHECKPOINT_FORMAT_VERSION_V1 => decode_postcard_payload_v1(payload),
        CHECKPOINT_FORMAT_VERSION_V2 => decode_postcard_payload_v2(payload),
        CHECKPOINT_FORMAT_VERSION => decode_postcard_payload_v3(payload),
        _ => Err(format!(
            "unsupported checkpoint format version {format_version}"
        )),
    }
}

fn decode_postcard_payload_v3(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
    let (checkpoint, remainder) = postcard::take_from_bytes::<AgentCheckpoint>(bytes)
        .map_err(|error| format!("Postcard decode failed: {error}"))?;
    if !remainder.is_empty() {
        return Err(format!(
            "Postcard checkpoint has {} trailing bytes",
            remainder.len()
        ));
    }
    let canonical = postcard::to_stdvec(&checkpoint)
        .map_err(|error| format!("Postcard canonical re-encode failed: {error}"))?;
    if canonical != bytes {
        return Err("Postcard checkpoint is not canonically encoded".to_string());
    }
    Ok(checkpoint)
}

fn decode_postcard_payload_v2(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
    let (checkpoint, remainder) = postcard::take_from_bytes::<AgentCheckpointPostcardV2>(bytes)
        .map_err(|error| format!("Postcard v2 decode failed: {error}"))?;
    if !remainder.is_empty() {
        return Err("Postcard v2 checkpoint has trailing bytes".into());
    }
    let canonical = postcard::to_stdvec(&checkpoint)
        .map_err(|error| format!("Postcard v2 canonical re-encode failed: {error}"))?;
    if canonical != bytes {
        return Err("Postcard v2 checkpoint is not canonically encoded".into());
    }
    Ok(checkpoint.into())
}

fn decode_postcard_payload_v1(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
    let (checkpoint, remainder) = postcard::take_from_bytes::<AgentCheckpointPostcardV1>(bytes)
        .map_err(|error| format!("Postcard v1 decode failed: {error}"))?;
    if !remainder.is_empty() {
        return Err(format!(
            "Postcard v1 checkpoint has {} trailing bytes",
            remainder.len()
        ));
    }
    let canonical = postcard::to_stdvec(&checkpoint)
        .map_err(|error| format!("Postcard v1 canonical re-encode failed: {error}"))?;
    if canonical != bytes {
        return Err("Postcard v1 checkpoint is not canonically encoded".to_string());
    }
    Ok(checkpoint.into())
}

fn decode_postcard_payload(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
    match decode_postcard_payload_v3(bytes).or_else(|_| decode_postcard_payload_v2(bytes)) {
        Ok(checkpoint) => Ok(checkpoint),
        Err(v2_error) => decode_postcard_payload_v1(bytes).map_err(|v1_error| {
            format!("not a supported Postcard checkpoint ({v2_error}; {v1_error})")
        }),
    }
}

fn decode_unframed(
    bytes: &[u8],
    expected_agent_id: &AgentId,
    filename_version: u64,
) -> Result<(AgentCheckpoint, CheckpointEncoding), String> {
    let legacy = legacy_checkpoint::decode(bytes).and_then(|(checkpoint, schema)| {
        validate_identity(&checkpoint, expected_agent_id, filename_version)?;
        let encoding = match schema {
            LegacyCheckpointSchema::V0_1_0 => CheckpointEncoding::LegacyBincodeV0_1_0,
            LegacyCheckpointSchema::V0_1_1ThroughV0_1_4 => {
                CheckpointEncoding::LegacyBincodeV0_1_1ThroughV0_1_4
            }
        };
        Ok((checkpoint, encoding))
    });
    let postcard = decode_postcard_payload(bytes).and_then(|checkpoint| {
        validate_identity(&checkpoint, expected_agent_id, filename_version)?;
        Ok((checkpoint, CheckpointEncoding::UnframedPostcard))
    });

    match (legacy, postcard) {
        // Bincode and Postcard's canonical markerless byte languages overlap.
        // Some ordinary 0.1.x Bincode timestamps are also valid Postcard
        // varints with a different value, so no semantic heuristic can
        // distinguish every dual-valid file. Preserve the released 0.1.x
        // contract deterministically; raw Postcard was an unshipped launch
        // candidate and is used only when the legacy reader does not match.
        (Ok(legacy), Ok(_)) => Ok(legacy),
        (Ok(legacy), Err(_)) => Ok(legacy),
        (Err(_), Ok(postcard)) => Ok(postcard),
        (Err(legacy_error), Err(postcard_error)) => Err(format!(
            "not a supported unframed checkpoint ({legacy_error}; {postcard_error})"
        )),
    }
}

fn validate_identity(
    checkpoint: &AgentCheckpoint,
    expected_agent_id: &AgentId,
    filename_version: u64,
) -> Result<(), String> {
    if checkpoint.agent_id != expected_agent_id.0 {
        return Err(format!(
            "checkpoint belongs to agent {}, expected {}",
            checkpoint.agent_id, expected_agent_id
        ));
    }
    if checkpoint.version != filename_version {
        return Err(format!(
            "checkpoint payload version {} does not match filename version {filename_version}",
            checkpoint.version
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionMemory, StoredToolCall};
    use axocoatl_core::{MessageRole, ProviderMetadata};

    fn fixture_bytes(hex: &str) -> Vec<u8> {
        let compact = hex.trim();
        assert!(compact.len().is_multiple_of(2));
        compact
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect()
    }

    fn v0_1_0_fixture() -> Vec<u8> {
        // Produced once by the exact structs at tag v0.1.0 using
        // bincode 2.0.1 + config::standard; the test never re-encodes it.
        fixture_bytes(include_str!("../tests/fixtures/checkpoint-v0.1.0.hex"))
    }

    fn v0_1_4_fixture() -> Vec<u8> {
        // Produced once by the exact structs at tag v0.1.4 using
        // bincode 2.0.1 + config::standard; the test never re-encodes it.
        fixture_bytes(include_str!("../tests/fixtures/checkpoint-v0.1.4.hex"))
    }

    fn test_checkpoint(agent_id: &str, version: u64) -> AgentCheckpoint {
        AgentCheckpoint {
            version,
            agent_id: agent_id.to_string(),
            checkpoint_time: 1234567890,
            session_messages: vec![StoredMessage {
                content_parts: None,
                role: MessageRole::User,
                content: format!("message v{version}"),
                timestamp: 1234567890,
                token_count: 10,
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }],
            cumulative_token_usage: TokenUsageStats::new(100, 50),
            cumulative_token_usage_known: true,
            behavior_state: None,
        }
    }

    #[tokio::test]
    async fn save_and_load_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);

        let ckpt = test_checkpoint("agent-1", 1);
        store.save(&ckpt).await.unwrap();

        let bytes =
            tokio::fs::read(storage_path(tmp.path(), "agent-1").join("0000000000000001.ckpt"))
                .await
                .unwrap();
        assert_eq!(&bytes[..CHECKPOINT_MAGIC.len()], CHECKPOINT_MAGIC);
        assert_eq!(bytes[CHECKPOINT_MAGIC.len()], CHECKPOINT_FORMAT_VERSION);

        let loaded = store
            .load_latest(&AgentId::new("agent-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.session_messages.len(), 1);
        assert_eq!(loaded.session_messages[0].content, "message v1");
    }

    #[tokio::test]
    async fn normal_load_ignores_a_noncanonical_numeric_filename_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.save(&test_checkpoint("agent-1", 1)).await.unwrap();

        let mut alias = test_checkpoint("agent-1", 1);
        alias.session_messages[0].content = "noncanonical alias".to_string();
        tokio::fs::write(
            storage_path(tmp.path(), "agent-1").join("1.ckpt"),
            encode_current(&alias).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(checkpoint_filename_version(Path::new("1.ckpt")), None);
        assert_eq!(
            checkpoint_filename_version(Path::new("0000000000000001.ckpt")),
            Some(1)
        );
        let loaded = store
            .load_latest(&AgentId::new("agent-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.session_messages[0].content, "message v1");
    }

    #[test]
    fn framed_postcard_v1_decodes_with_conservative_usage_completeness() {
        let checkpoint = test_checkpoint("agent-1", 1);
        let legacy_payload = AgentCheckpointPostcardV1 {
            version: checkpoint.version,
            agent_id: checkpoint.agent_id,
            checkpoint_time: checkpoint.checkpoint_time,
            session_messages: checkpoint
                .session_messages
                .into_iter()
                .map(Into::into)
                .collect(),
            cumulative_token_usage: checkpoint.cumulative_token_usage,
            behavior_state: checkpoint.behavior_state,
        };
        let payload = postcard::to_stdvec(&legacy_payload).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CHECKPOINT_MAGIC);
        bytes.push(CHECKPOINT_FORMAT_VERSION_V1);
        bytes.extend_from_slice(&payload);

        let decoded = decode_current(&bytes).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.agent_id, "agent-1");
        assert!(!decoded.cumulative_token_usage_known);
        assert_eq!(
            decoded.cumulative_token_usage,
            TokenUsageStats::new(100, 50)
        );
    }

    #[test]
    fn checkpoint_v3_preserves_images_and_decodes_exact_preceding_v2() {
        use axocoatl_core::{ChatMessage, ContentPart, ImageDetail, MessageContent};
        let old = test_checkpoint("agent-1", 1);
        let old = AgentCheckpointPostcardV2 {
            version: old.version,
            agent_id: old.agent_id,
            checkpoint_time: old.checkpoint_time,
            session_messages: old.session_messages.into_iter().map(Into::into).collect(),
            cumulative_token_usage: old.cumulative_token_usage,
            cumulative_token_usage_known: old.cumulative_token_usage_known,
            behavior_state: old.behavior_state,
        };
        let mut old_bytes = CHECKPOINT_MAGIC.to_vec();
        old_bytes.push(CHECKPOINT_FORMAT_VERSION_V2);
        old_bytes.extend(postcard::to_stdvec(&old).unwrap());
        let mut decoded = decode_current(&old_bytes).unwrap();
        assert!(decoded.session_messages[0].content_parts.is_none());
        assert_eq!(decoded.session_messages[0].content, "message v1");
        assert_eq!(
            decoded.cumulative_token_usage_known,
            old.cumulative_token_usage_known
        );

        let mut message = ChatMessage::user("inspect this");
        message.content = MessageContent::Parts(vec![
            ContentPart::Text("inspect this".into()),
            ContentPart::Image {
                url: "data:image/png;base64,iVBORw==".into(),
                detail: ImageDetail::Auto,
            },
        ]);
        let mut memory = SessionMemory::new();
        memory.replace_with_chat_messages(std::slice::from_ref(&message), str::len);
        decoded.session_messages = memory.messages().to_vec();
        let new_bytes = encode_current(&decoded).unwrap();
        assert_eq!(new_bytes[CHECKPOINT_MAGIC.len()], 3);
        let restored = decode_current(&new_bytes).unwrap();
        memory.restore(restored.session_messages);
        assert_eq!(
            serde_json::to_value(&memory.as_chat_messages()[0]).unwrap(),
            serde_json::to_value(message).unwrap()
        );
        // The framed old decoder must not guess the new message shape.
        let mut wrong_version = new_bytes;
        wrong_version[CHECKPOINT_MAGIC.len()] = CHECKPOINT_FORMAT_VERSION_V2;
        assert!(decode_current(&wrong_version).is_err());
    }

    #[tokio::test]
    async fn versioned_checkpoint_preserves_exact_provider_tool_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let provider_metadata = ProviderMetadata::from([
            ("axocoatl.route.slot".to_string(), "fallback".to_string()),
            (
                "gemini.thought_signature".to_string(),
                "exact-gemini-signature".to_string(),
            ),
            (
                "anthropic.assistant_content_blocks".to_string(),
                r#"[{"type":"thinking","thinking":"plan","signature":"exact-anthropic-signature"},{"type":"tool_use","id":"call-1","name":"search","input":{"q":"rust"}}]"#.to_string(),
            ),
        ]);
        let mut checkpoint = test_checkpoint("agent-1", 1);
        checkpoint.session_messages = vec![StoredMessage {
            content_parts: None,
            role: MessageRole::Assistant,
            content: "I will search.".to_string(),
            timestamp: 1234567890,
            token_count: 10,
            name: None,
            tool_calls: vec![StoredToolCall {
                id: "call-1".to_string(),
                name: "search".to_string(),
                arguments_json: r#"{"q":"rust"}"#.to_string(),
                provider_metadata: provider_metadata.clone(),
            }],
            tool_call_id: None,
        }];

        store.save(&checkpoint).await.unwrap();
        let loaded = store
            .load_latest(&AgentId::new("agent-1"))
            .await
            .unwrap()
            .unwrap();
        let mut session = SessionMemory::new();
        session.restore(loaded.session_messages);
        let messages = session.as_chat_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(
            messages[0].tool_calls[0].provider_metadata,
            provider_metadata
        );
    }

    #[test]
    fn structurally_valid_checkpoint_does_not_reject_unusual_timestamps() {
        let mut checkpoint = test_checkpoint("agent-1", 1);
        checkpoint.checkpoint_time = u64::MAX;
        checkpoint.session_messages[0].timestamp = u64::MAX;

        let decoded = decode_current(&encode_current(&checkpoint).unwrap()).unwrap();
        assert_eq!(decoded.checkpoint_time, u64::MAX);
        assert_eq!(decoded.session_messages[0].timestamp, u64::MAX);
    }

    #[tokio::test]
    async fn load_latest_picks_highest_version() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);

        store.save(&test_checkpoint("agent-1", 1)).await.unwrap();
        store.save(&test_checkpoint("agent-1", 3)).await.unwrap();
        store.save(&test_checkpoint("agent-1", 2)).await.unwrap();

        let loaded = store
            .load_latest(&AgentId::new("agent-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 3);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);

        let result = store.load_latest(&AgentId::new("ghost")).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn older_or_corrupt_checkpoint_cache_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let dir = tmp.path().join("agent-1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("0000000000000001.ckpt"), b"pre-1.0-cache")
            .await
            .unwrap();

        let result = store.load_latest(&AgentId::new("agent-1")).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn real_v0_1_0_bincode_checkpoint_decodes_without_reencoding() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let dir = tmp.path().join("legacy-session:coder");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("0000000000000007.ckpt"), v0_1_0_fixture())
            .await
            .unwrap();

        let loaded = store
            .load_latest_with_encoding(&AgentId::new("legacy-session:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.encoding, CheckpointEncoding::LegacyBincodeV0_1_0);
        assert_eq!(loaded.checkpoint.version, 7);
        assert_eq!(loaded.checkpoint.session_messages.len(), 5);
        assert_eq!(
            loaded.checkpoint.session_messages[1].content,
            "First legacy request"
        );
        assert!(loaded.checkpoint.session_messages[1].tool_calls.is_empty());
        assert_eq!(
            loaded.checkpoint.cumulative_token_usage.reasoning_tokens,
            Some(3)
        );
    }

    #[tokio::test]
    async fn real_v0_1_4_bincode_checkpoint_preserves_tool_records() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let dir = tmp.path().join("legacy-session:coder");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let fixture = v0_1_4_fixture();
        assert!(
            legacy_checkpoint::decode(&fixture).is_ok(),
            "fixture must decode: {:?}",
            legacy_checkpoint::decode(&fixture)
        );
        tokio::fs::write(dir.join("0000000000000012.ckpt"), fixture)
            .await
            .unwrap();

        let loaded = store
            .load_latest_with_encoding(&AgentId::new("legacy-session:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.encoding,
            CheckpointEncoding::LegacyBincodeV0_1_1ThroughV0_1_4
        );
        assert_eq!(loaded.checkpoint.version, 12);
        assert_eq!(loaded.checkpoint.session_messages.len(), 5);
        let assistant_tool_call = &loaded.checkpoint.session_messages[2];
        assert_eq!(assistant_tool_call.tool_calls.len(), 1);
        assert_eq!(assistant_tool_call.tool_calls[0].id, "call-1");
        let tool_result = &loaded.checkpoint.session_messages[3];
        assert_eq!(tool_result.name.as_deref(), Some("read_file"));
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn shipped_bincode_markerless_bytes_are_not_reclassified_as_postcard() {
        let bytes = v0_1_4_fixture();
        let (legacy, _) = legacy_checkpoint::decode(&bytes).unwrap();

        let (decoded, encoding) = decode_unframed(
            &bytes,
            &AgentId::new("legacy-session:coder"),
            legacy.version,
        )
        .unwrap();
        assert_eq!(
            encoding,
            CheckpointEncoding::LegacyBincodeV0_1_1ThroughV0_1_4
        );
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(legacy).unwrap()
        );
    }

    #[test]
    fn temporary_unframed_postcard_v1_remains_loadable_as_unknown_usage() {
        let checkpoint = test_checkpoint("launch-session:coder", 9);
        let v1 = AgentCheckpointPostcardV1 {
            version: checkpoint.version,
            agent_id: checkpoint.agent_id,
            checkpoint_time: checkpoint.checkpoint_time,
            session_messages: checkpoint
                .session_messages
                .into_iter()
                .map(Into::into)
                .collect(),
            cumulative_token_usage: checkpoint.cumulative_token_usage,
            behavior_state: checkpoint.behavior_state,
        };
        let bytes = postcard::to_stdvec(&v1).unwrap();

        let (decoded, encoding) =
            decode_unframed(&bytes, &AgentId::new("launch-session:coder"), v1.version).unwrap();
        assert_eq!(encoding, CheckpointEncoding::UnframedPostcard);
        assert!(!decoded.cumulative_token_usage_known);
        assert_eq!(decoded.cumulative_token_usage, v1.cumulative_token_usage);
    }

    #[tokio::test]
    async fn temporary_unframed_postcard_checkpoint_remains_loadable() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let checkpoint = test_checkpoint("launch-session:coder", 9);
        let bytes = postcard::to_stdvec(&checkpoint).unwrap();
        let dir = tmp.path().join("launch-session:coder");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("0000000000000009.ckpt"), bytes)
            .await
            .unwrap();

        let loaded = store
            .load_latest_with_encoding(&AgentId::new("launch-session:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.encoding, CheckpointEncoding::UnframedPostcard);
        assert_eq!(loaded.checkpoint.version, 9);
        assert_eq!(loaded.checkpoint.session_messages[0].content, "message v9");
    }

    #[tokio::test]
    async fn corrupt_newest_candidate_falls_back_without_mutating_either_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let dir = tmp.path().join("legacy-session:coder");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let legacy = v0_1_4_fixture();
        let corrupt = b"not-a-checkpoint".to_vec();
        let legacy_path = dir.join("0000000000000012.ckpt");
        let corrupt_path = dir.join("0000000000000013.ckpt");
        tokio::fs::write(&legacy_path, &legacy).await.unwrap();
        tokio::fs::write(&corrupt_path, &corrupt).await.unwrap();

        let loaded = store
            .load_latest_with_encoding(&AgentId::new("legacy-session:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.checkpoint.version, 12);
        assert_eq!(loaded.highest_seen_version, 13);
        assert!(loaded.encoding.is_legacy());
        assert_eq!(tokio::fs::read(legacy_path).await.unwrap(), legacy);
        assert_eq!(tokio::fs::read(corrupt_path).await.unwrap(), corrupt);
    }

    #[tokio::test]
    async fn oversized_sparse_newest_candidate_is_rejected_before_read_and_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.save(&test_checkpoint("agent-1", 1)).await.unwrap();
        let oversized_path = storage_path(tmp.path(), "agent-1").join("0000000000000002.ckpt");
        let oversized = std::fs::File::create(&oversized_path).unwrap();
        oversized.set_len(MAX_CHECKPOINT_BYTES as u64 + 1).unwrap();
        drop(oversized);

        let loaded = store
            .load_latest_with_encoding(&AgentId::new("agent-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.checkpoint.version, 1);
        assert_eq!(loaded.highest_seen_version, 2);
        assert_eq!(
            std::fs::metadata(oversized_path).unwrap().len(),
            MAX_CHECKPOINT_BYTES as u64 + 1
        );
    }

    #[tokio::test]
    async fn legacy_trailing_bytes_and_identity_mismatch_fail_safely() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let mut trailing = v0_1_4_fixture();
        trailing.push(0);
        let dir = tmp.path().join("legacy-session:coder");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("0000000000000012.ckpt"), &trailing)
            .await
            .unwrap();
        assert!(store
            .load_latest(&AgentId::new("legacy-session:coder"))
            .await
            .unwrap()
            .is_none());

        let mismatch_dir = tmp.path().join("different-session:coder");
        tokio::fs::create_dir_all(&mismatch_dir).await.unwrap();
        let fixture = v0_1_4_fixture();
        tokio::fs::write(mismatch_dir.join("0000000000000012.ckpt"), &fixture)
            .await
            .unwrap();
        assert!(store
            .load_latest(&AgentId::new("different-session:coder"))
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            tokio::fs::read(dir.join("0000000000000012.ckpt"))
                .await
                .unwrap(),
            trailing
        );
        assert_eq!(
            tokio::fs::read(mismatch_dir.join("0000000000000012.ckpt"))
                .await
                .unwrap(),
            fixture
        );
    }

    #[tokio::test]
    async fn removing_prepared_version_restores_previous_latest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.save(&test_checkpoint("agent-1", 1)).await.unwrap();
        store.save(&test_checkpoint("agent-1", 2)).await.unwrap();
        store.remove_version("agent-1", 2).await.unwrap();
        let loaded = store
            .load_latest(&AgentId::new("agent-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 1);
        store.remove_version("agent-1", 2).await.unwrap();
    }

    #[tokio::test]
    async fn prune_keeps_last_n() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);

        // Save 5 checkpoints — pruning keeps last 3
        for v in 1..=5 {
            store.save(&test_checkpoint("agent-1", v)).await.unwrap();
        }

        let dir = storage_path(tmp.path(), "agent-1");
        let mut count = 0;
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while entries.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 3); // Only last 3 kept
    }

    #[tokio::test]
    async fn scoped_ids_read_legacy_posix_paths_and_promote_to_portable_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let id = "ses-123:coder";
        let legacy_dir = tmp.path().join(id);
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        let legacy_path = legacy_dir.join("0000000000000001.ckpt");
        let legacy_bytes = encode_current(&test_checkpoint(id, 1)).unwrap();
        tokio::fs::write(&legacy_path, &legacy_bytes).await.unwrap();

        assert_eq!(
            store
                .load_latest(&AgentId::new(id))
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );

        store.save(&test_checkpoint(id, 2)).await.unwrap();
        let portable_dir = storage_path(tmp.path(), id);
        assert_ne!(portable_dir, legacy_dir);
        assert!(portable_dir.join("0000000000000002.ckpt").is_file());
        assert_eq!(tokio::fs::read(&legacy_path).await.unwrap(), legacy_bytes);
        assert_eq!(
            store
                .load_latest(&AgentId::new(id))
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
    }

    #[tokio::test]
    async fn unsafe_ids_cannot_escape_or_make_pruning_touch_an_outside_sentinel() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("checkpoints");
        let outside = parent.path().join("outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        let sentinel = outside.join("0000000000000000.ckpt");
        tokio::fs::write(&sentinel, b"must survive").await.unwrap();
        let absolute = parent.path().join("absolute-target");
        let malicious = vec![
            "../outside".to_string(),
            "a/../../outside".to_string(),
            absolute.display().to_string(),
            ".".to_string(),
            "..".to_string(),
            String::new(),
            "a\\b".to_string(),
            "x".repeat(300),
        ];
        let store = CheckpointStore::new(&root, CheckpointPolicy::Manual);

        for id in &malicious {
            for version in 1..=4 {
                store.save(&test_checkpoint(id, version)).await.unwrap();
            }
            let portable_dir = storage_path(&root, id);
            assert_eq!(portable_dir.parent(), Some(root.join("v1").as_path()));
            assert_eq!(
                store
                    .load_latest(&AgentId::new(id))
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                4
            );
            store.remove_version(id, 4).await.unwrap();
            assert_eq!(
                store
                    .load_latest(&AgentId::new(id))
                    .await
                    .unwrap()
                    .unwrap()
                    .version,
                3
            );
        }

        assert_eq!(tokio::fs::read(&sentinel).await.unwrap(), b"must survive");
        assert!(!absolute.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lowercase_current_checkpoint_does_not_adopt_uppercase_legacy_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let legacy_dir = tmp.path().join("Coder");
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        let legacy_bytes = encode_current(&test_checkpoint("Coder", 9)).unwrap();
        tokio::fs::write(legacy_dir.join("0000000000000009.ckpt"), &legacy_bytes)
            .await
            .unwrap();

        store.save(&test_checkpoint("coder", 1)).await.unwrap();

        let lowercase = store
            .load_latest(&AgentId::new("coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lowercase.agent_id, "coder");
        assert_eq!(lowercase.version, 1);
        let uppercase = store
            .load_latest(&AgentId::new("Coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uppercase.agent_id, "Coder");
        assert_eq!(uppercase.version, 9);
        assert!(storage_path(tmp.path(), "coder")
            .join("0000000000000001.ckpt")
            .is_file());
        assert_eq!(
            tokio::fs::read(legacy_dir.join("0000000000000009.ckpt"))
                .await
                .unwrap(),
            legacy_bytes
        );
    }

    #[tokio::test]
    async fn session_turn_staging_is_scoped_until_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store
            .save(&test_checkpoint("ses-a:coder", 1))
            .await
            .unwrap();
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        let scoped = store.scoped_to_session_turn("ses-a", "turn-a");
        scoped
            .save(&test_checkpoint("ses-a:coder", 2))
            .await
            .unwrap();

        assert_eq!(
            store
                .load_latest(&AgentId::new("ses-a:coder"))
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );
        assert_eq!(
            scoped
                .load_latest(&AgentId::new("ses-a:coder"))
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        assert!(scoped
            .load_latest(&AgentId::new("ses-a:reviewer"))
            .await
            .unwrap()
            .is_none());

        let reopened = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        assert_eq!(
            reopened
                .load_latest(&AgentId::new("ses-a:coder"))
                .await
                .unwrap()
                .unwrap()
                .version,
            1
        );
        assert_eq!(
            reopened
                .scoped_to_session_turn("ses-a", "turn-a")
                .load_latest(&AgentId::new("ses-a:coder"))
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        assert_eq!(
            reopened.list_session_turn_transactions().unwrap(),
            vec![CheckpointTransactionInfo {
                session_id: "ses-a".to_string(),
                turn_id: "turn-a".to_string(),
                state: CheckpointTransactionState::Pending,
            }]
        );

        reopened
            .commit_session_turn("ses-a", "turn-a")
            .await
            .unwrap();
        reopened
            .commit_session_turn("ses-a", "turn-a")
            .await
            .unwrap();
        assert!(reopened
            .list_session_turn_transactions()
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened
                .load_latest(&AgentId::new("ses-a:coder"))
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
    }

    #[tokio::test]
    async fn staged_alias_fails_retryably_before_commit_state_is_published() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        store
            .stage_session_turn("ses-a", "turn-a", &test_checkpoint("ses-a:coder", 1))
            .await
            .unwrap();

        let mut alias = test_checkpoint("ses-a:coder", 1);
        alias.session_messages[0].content = "noncanonical staged alias".to_string();
        let base = store.secure_base().unwrap();
        let agent_dir = store
            .transaction_agent_dir(&base, "ses-a", "turn-a", "ses-a:coder", false)
            .unwrap();
        agent_dir
            .atomic_write("1.ckpt", &encode_current(&alias).unwrap())
            .unwrap();

        for _ in 0..2 {
            let error = store
                .commit_session_turn("ses-a", "turn-a")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("unexpected entry"));
            assert_eq!(
                store.list_session_turn_transactions().unwrap(),
                vec![CheckpointTransactionInfo {
                    session_id: "ses-a".to_string(),
                    turn_id: "turn-a".to_string(),
                    state: CheckpointTransactionState::Pending,
                }]
            );
        }

        agent_dir.remove_leaf("1.ckpt").unwrap();
        store.commit_session_turn("ses-a", "turn-a").await.unwrap();
        let loaded = store
            .load_latest(&AgentId::new("ses-a:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.session_messages[0].content, "message v1");
    }

    #[tokio::test]
    async fn abort_restores_transcript_but_preserves_pending_usage_accounting() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let mut baseline = test_checkpoint("ses-a:coder", 1);
        baseline.behavior_state = Some("committed behavior".to_string());
        store.save(&baseline).await.unwrap();
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();

        let mut pending = test_checkpoint("ses-a:coder", 2);
        pending.cumulative_token_usage = TokenUsageStats::new(140, 70);
        pending.cumulative_token_usage_known = false;
        pending.behavior_state = Some("uncommitted behavior".to_string());
        store
            .scoped_to_session_turn("ses-a", "turn-a")
            .save(&pending)
            .await
            .unwrap();
        store.abort_session_turn("ses-a", "turn-a").await.unwrap();
        store.abort_session_turn("ses-a", "turn-a").await.unwrap();

        let loaded = store
            .load_latest(&AgentId::new("ses-a:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 3);
        assert_eq!(loaded.session_messages.len(), 1);
        assert_eq!(loaded.session_messages[0].content, "message v1");
        assert_eq!(loaded.cumulative_token_usage, TokenUsageStats::new(140, 70));
        assert!(!loaded.cumulative_token_usage_known);
        assert_eq!(loaded.behavior_state, None);
        assert!(store.list_session_turn_transactions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconciliation_repeats_durable_in_progress_dispositions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store
            .save(&test_checkpoint("ses-a:coder", 1))
            .await
            .unwrap();
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        store
            .scoped_to_session_turn("ses-a", "turn-a")
            .save(&test_checkpoint("ses-a:coder", 2))
            .await
            .unwrap();
        let base = store.secure_base().unwrap();
        let mut committing = store
            .read_transaction_manifest(&base, "ses-a", "turn-a")
            .unwrap()
            .unwrap();
        committing.state = CheckpointTransactionState::Committing;
        store
            .write_transaction_manifest(&base, &committing)
            .unwrap();
        drop(base);

        let reopened = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        reopened
            .reconcile_session_turn("ses-a", "turn-a", CheckpointTransactionResolution::Commit)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .load_latest(&AgentId::new("ses-a:coder"))
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );

        reopened
            .begin_session_turn("ses-a", "turn-b")
            .await
            .unwrap();
        let mut pending = test_checkpoint("ses-a:coder", 3);
        pending.cumulative_token_usage = TokenUsageStats::new(175, 90);
        reopened
            .scoped_to_session_turn("ses-a", "turn-b")
            .save(&pending)
            .await
            .unwrap();
        let base = reopened.secure_base().unwrap();
        let mut aborting = reopened
            .read_transaction_manifest(&base, "ses-a", "turn-b")
            .unwrap()
            .unwrap();
        aborting.state = CheckpointTransactionState::Aborting;
        reopened
            .write_transaction_manifest(&base, &aborting)
            .unwrap();
        drop(base);

        let restarted = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        restarted
            .reconcile_session_turn("ses-a", "turn-b", CheckpointTransactionResolution::Abort)
            .await
            .unwrap();
        let loaded = restarted
            .load_latest(&AgentId::new("ses-a:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 4);
        assert_eq!(loaded.session_messages[0].content, "message v2");
        assert_eq!(loaded.cumulative_token_usage, TokenUsageStats::new(175, 90));
        assert_eq!(loaded.behavior_state, None);
    }

    const STRANDED_ATOMIC_TEMP: &str = ".axocoatl-00000000-0000-4000-8000-000000000001.tmp";

    #[tokio::test]
    async fn startup_recovers_initial_manifest_atomic_temp_without_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let base = store.secure_base().unwrap();
        let turn_dir = store
            .transaction_dir(&base, "ses-a", "turn-a", true)
            .unwrap();
        turn_dir
            .atomic_write(STRANDED_ATOMIC_TEMP, b"partial manifest")
            .unwrap();

        assert!(store.list_session_turn_transactions().unwrap().is_empty());
        assert!(!turn_dir.is_file(STRANDED_ATOMIC_TEMP).unwrap());
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        assert_eq!(
            store.list_session_turn_transactions().unwrap()[0].state,
            CheckpointTransactionState::Pending
        );
    }

    #[tokio::test]
    async fn startup_uses_old_manifest_after_state_transition_atomic_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        let base = store.secure_base().unwrap();
        let turn_dir = store
            .transaction_dir(&base, "ses-a", "turn-a", false)
            .unwrap();
        turn_dir
            .atomic_write(STRANDED_ATOMIC_TEMP, b"partial aborting manifest")
            .unwrap();

        assert_eq!(
            store.list_session_turn_transactions().unwrap(),
            vec![CheckpointTransactionInfo {
                session_id: "ses-a".to_string(),
                turn_id: "turn-a".to_string(),
                state: CheckpointTransactionState::Pending,
            }]
        );
        assert!(!turn_dir.is_file(STRANDED_ATOMIC_TEMP).unwrap());
        drop(turn_dir);
        store.abort_session_turn("ses-a", "turn-a").await.unwrap();
        assert!(store.list_session_turn_transactions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cleanup_recovers_manifest_atomic_temp_but_rejects_lookalikes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        let base = store.secure_base().unwrap();
        let mut manifest = store
            .read_transaction_manifest(&base, "ses-a", "turn-a")
            .unwrap()
            .unwrap();
        manifest.state = CheckpointTransactionState::Committed;
        store.write_transaction_manifest(&base, &manifest).unwrap();
        let turn_dir = store
            .transaction_dir(&base, "ses-a", "turn-a", false)
            .unwrap();
        turn_dir
            .atomic_write(STRANDED_ATOMIC_TEMP, b"partial cleanup write")
            .unwrap();
        drop(turn_dir);
        store.commit_session_turn("ses-a", "turn-a").await.unwrap();
        assert!(store.list_session_turn_transactions().unwrap().is_empty());

        store.begin_session_turn("ses-a", "turn-b").await.unwrap();
        let turn_dir = store
            .transaction_dir(&base, "ses-a", "turn-b", false)
            .unwrap();
        turn_dir
            .atomic_write(".axocoatl-not-a-uuid.tmp", b"must not be discarded")
            .unwrap();
        assert!(matches!(
            store.list_session_turn_transactions(),
            Err(MemoryError::Invalid(message)) if message.contains("unexpected entry")
        ));
        assert!(turn_dir.is_file(".axocoatl-not-a-uuid.tmp").unwrap());
    }

    #[tokio::test]
    async fn abort_discards_incomplete_agent_identity_temp_but_commit_fails_closed() {
        let aborted_tmp = tempfile::tempdir().unwrap();
        let aborted = CheckpointStore::new(aborted_tmp.path(), CheckpointPolicy::Manual);
        aborted
            .begin_session_turn("ses-a", "turn-abort")
            .await
            .unwrap();
        let base = aborted.secure_base().unwrap();
        let agent_dir = aborted
            .transaction_agent_dir(&base, "ses-a", "turn-abort", "ses-a:coder", true)
            .unwrap();
        agent_dir
            .atomic_write(STRANDED_ATOMIC_TEMP, b"partial identity")
            .unwrap();
        drop(agent_dir);
        aborted
            .abort_session_turn("ses-a", "turn-abort")
            .await
            .unwrap();
        assert!(aborted.list_session_turn_transactions().unwrap().is_empty());

        let committed_tmp = tempfile::tempdir().unwrap();
        let committed = CheckpointStore::new(committed_tmp.path(), CheckpointPolicy::Manual);
        committed
            .begin_session_turn("ses-a", "turn-commit")
            .await
            .unwrap();
        let base = committed.secure_base().unwrap();
        let agent_dir = committed
            .transaction_agent_dir(&base, "ses-a", "turn-commit", "ses-a:coder", true)
            .unwrap();
        agent_dir
            .atomic_write(STRANDED_ATOMIC_TEMP, b"partial identity")
            .unwrap();
        drop(agent_dir);
        assert!(matches!(
            committed
                .commit_session_turn("ses-a", "turn-commit")
                .await,
            Err(MemoryError::Invalid(message))
                if message.contains("no durable Agent identity")
        ));
        assert_eq!(
            committed.list_session_turn_transactions().unwrap()[0].state,
            CheckpointTransactionState::Pending
        );
    }

    #[tokio::test]
    async fn commit_ignores_only_a_stranded_staged_checkpoint_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        store.begin_session_turn("ses-a", "turn-a").await.unwrap();
        let base = store.secure_base().unwrap();
        let agent_dir = store
            .transaction_agent_dir(&base, "ses-a", "turn-a", "ses-a:coder", true)
            .unwrap();
        store
            .ensure_transaction_agent_identity(&agent_dir, "ses-a:coder")
            .unwrap();
        agent_dir
            .atomic_write(STRANDED_ATOMIC_TEMP, b"partial checkpoint")
            .unwrap();
        drop(agent_dir);

        store.commit_session_turn("ses-a", "turn-a").await.unwrap();
        assert!(store
            .load_latest(&AgentId::new("ses-a:coder"))
            .await
            .unwrap()
            .is_none());
        assert!(store.list_session_turn_transactions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn legacy_adoption_sanitizes_only_the_exact_session_prefix_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        for agent_id in ["ses-a:coder", "ses-a:reviewer", "ses-ab:coder"] {
            let mut checkpoint = test_checkpoint(agent_id, 1);
            checkpoint.behavior_state = Some(format!("private {agent_id}"));
            store.save(&checkpoint).await.unwrap();
        }

        assert_eq!(
            store
                .sanitize_committed_session_prefix("ses-a")
                .await
                .unwrap(),
            2
        );
        for agent_id in ["ses-a:coder", "ses-a:reviewer"] {
            let loaded = store
                .load_latest(&AgentId::new(agent_id))
                .await
                .unwrap()
                .unwrap();
            assert!(loaded.session_messages.is_empty());
            assert_eq!(loaded.cumulative_token_usage, TokenUsageStats::new(100, 50));
            assert!(loaded.cumulative_token_usage_known);
            assert_eq!(loaded.behavior_state, None);
        }
        let unrelated = store
            .load_latest(&AgentId::new("ses-ab:coder"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unrelated.session_messages[0].content, "message v1");
        assert_eq!(
            unrelated.behavior_state.as_deref(),
            Some("private ses-ab:coder")
        );
        assert_eq!(
            store
                .sanitize_committed_session_prefix("ses-a")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn legacy_adoption_rejects_checkpoint_path_identity_confusion() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let confused_dir = tmp.path().join("v1").join(storage_key("ses-other:coder"));
        tokio::fs::create_dir_all(&confused_dir).await.unwrap();
        tokio::fs::write(
            confused_dir.join("0000000000000001.ckpt"),
            encode_current(&test_checkpoint("ses-a:coder", 1)).unwrap(),
        )
        .await
        .unwrap();

        assert!(matches!(
            store.sanitize_committed_session_prefix("ses-a").await,
            Err(MemoryError::Invalid(message))
                if message.contains("does not match its storage key")
        ));
        assert!(!storage_path(tmp.path(), "ses-a:coder").exists());
    }

    #[test]
    fn identity_discovery_ignores_noncanonical_numeric_filename_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let base = store.secure_base().unwrap();
        let agent_id = "ses-a:coder";
        let dir = base
            .child(Path::new("v1").join(storage_key(agent_id)))
            .unwrap();
        let bytes = encode_current(&test_checkpoint(agent_id, 1)).unwrap();
        dir.atomic_write("1.ckpt", &bytes).unwrap();

        assert!(discover_committed_agent_identities(&base)
            .unwrap()
            .is_empty());

        dir.atomic_write(CheckpointStore::checkpoint_name(1), &bytes)
            .unwrap();
        assert_eq!(
            discover_committed_agent_identities(&base).unwrap(),
            BTreeSet::from([agent_id.to_string()])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_adoption_includes_pre_transaction_literal_agent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        let agent_id = "ses-a:legacy";
        let legacy_dir = tmp.path().join(agent_id);
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        tokio::fs::write(
            legacy_dir.join("0000000000000001.ckpt"),
            encode_current(&test_checkpoint(agent_id, 1)).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            store
                .sanitize_committed_session_prefix("ses-a")
                .await
                .unwrap(),
            1
        );
        let loaded = store
            .load_latest(&AgentId::new(agent_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 2);
        assert!(loaded.session_messages.is_empty());
        assert_eq!(loaded.cumulative_token_usage, TokenUsageStats::new(100, 50));
    }

    #[tokio::test]
    async fn checkpoint_envelope_roundtrip() {
        let ckpt = test_checkpoint("test", 42);
        let bytes = encode_current(&ckpt).unwrap();
        assert!(bytes.starts_with(CHECKPOINT_MAGIC));
        let decoded = decode_current(&bytes).unwrap();
        assert_eq!(decoded.version, 42);
        assert_eq!(decoded.agent_id, "test");
    }

    #[test]
    fn should_checkpoint_honors_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let every = CheckpointStore::new(tmp.path(), CheckpointPolicy::EveryLlmCall);
        assert!(every.should_checkpoint(1));
        assert!(every.should_checkpoint(7));

        let every_3 = CheckpointStore::new(tmp.path(), CheckpointPolicy::EveryNMessages(3));
        assert!(!every_3.should_checkpoint(1));
        assert!(every_3.should_checkpoint(3));
        assert!(every_3.should_checkpoint(6));

        let manual = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);
        assert!(!manual.should_checkpoint(3));
        let none = CheckpointStore::new(tmp.path(), CheckpointPolicy::None);
        assert!(!none.should_checkpoint(3));
    }
}
