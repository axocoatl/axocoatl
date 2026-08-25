use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use axocoatl_core::{secure_fs::SecureDir, AgentId, TokenUsageStats};

use crate::error::MemoryError;
use crate::legacy_checkpoint::{self, LegacyCheckpointSchema};
use crate::session::StoredMessage;
#[cfg(test)]
use crate::storage::storage_path;
use crate::storage::{legacy_storage_component, storage_key};

const CHECKPOINT_MAGIC: &[u8; 8] = b"AXOCKPT\0";
const CHECKPOINT_FORMAT_VERSION_V1: u8 = 1;
const CHECKPOINT_FORMAT_VERSION: u8 = 2;
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
    /// both schema revision 1 and the current revision 2.
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

pub struct CheckpointStore {
    base_dir: PathBuf,
    secure_base: Option<SecureDir>,
    policy: CheckpointPolicy,
}

/// Postcard payload written before cumulative-usage completeness became
/// durable. Postcard's sequence encoding cannot apply `serde(default)` to a
/// missing struct field, so keep the exact old shape as a decode-only wire
/// schema and map it to an unknown lower bound.
#[derive(Debug, Serialize, Deserialize)]
struct AgentCheckpointPostcardV1 {
    version: u64,
    agent_id: String,
    checkpoint_time: u64,
    session_messages: Vec<StoredMessage>,
    cumulative_token_usage: TokenUsageStats,
    behavior_state: Option<String>,
}

impl From<AgentCheckpointPostcardV1> for AgentCheckpoint {
    fn from(checkpoint: AgentCheckpointPostcardV1) -> Self {
        Self {
            version: checkpoint.version,
            agent_id: checkpoint.agent_id,
            checkpoint_time: checkpoint.checkpoint_time,
            session_messages: checkpoint.session_messages,
            cumulative_token_usage: checkpoint.cumulative_token_usage,
            cumulative_token_usage_known: false,
            behavior_state: checkpoint.behavior_state,
        }
    }
}

impl CheckpointStore {
    pub fn new(base_dir: impl Into<PathBuf>, policy: CheckpointPolicy) -> Self {
        Self {
            base_dir: base_dir.into(),
            secure_base: None,
            policy,
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
        })
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
        let dir = self
            .secure_base()?
            .child(Path::new("v1").join(storage_key(&checkpoint.agent_id)))?;
        let bytes = encode_current(checkpoint)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(MemoryError::Serialization(format!(
                "checkpoint is {} bytes; limit is {MAX_CHECKPOINT_BYTES}",
                bytes.len()
            )));
        }

        // Checkpoints hold full message + tool I/O verbatim. SecureDir creates
        // an unpredictable owner-only temp and fsyncs it before replacement.
        dir.atomic_write(Self::checkpoint_name(checkpoint.version), &bytes)?;

        self.prune_old(&dir, 3).await.ok();

        tracing::debug!(
            agent = %checkpoint.agent_id,
            version = checkpoint.version,
            bytes = bytes.len(),
            "Checkpoint saved"
        );
        Ok(())
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
        let base = self.secure_base()?;
        let current_key = storage_key(&agent_id.0);
        let mut dirs = Vec::new();
        match base.existing_child(Path::new("v1").join(&current_key)) {
            Ok(dir) => dirs.push((dir, true)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(legacy) = legacy_storage_component(&agent_id.0) {
            if base.has_exact_directory(legacy)? {
                dirs.push((base.existing_child(legacy)?, false));
            }
        }

        let mut candidates: Vec<(u64, bool, SecureDir, OsString)> = Vec::new();
        for (dir, current) in dirs {
            for entry in dir.entries()? {
                if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::File {
                    continue;
                }
                let path = Path::new(&entry.name);
                if path.extension().and_then(|extension| extension.to_str()) == Some("ckpt") {
                    if let Some(version) = path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .and_then(|stem| stem.parse::<u64>().ok())
                    {
                        candidates.push((version, current, dir.clone(), entry.name));
                    }
                }
            }
        }

        // Prefer the current portable location when an interrupted migration
        // left the same version in both places, then fall back by version.
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        let Some(highest_seen_version) = candidates.first().map(|(version, _, _, _)| *version)
        else {
            return Ok(None);
        };

        for (filename_version, _, dir, name) in candidates {
            let path = dir.path().join(&name);
            let bytes_len = match dir.file_len(&name) {
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
            let bytes = match dir.read(&name) {
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
                decode_current(&bytes)
                    .map(|checkpoint| (checkpoint, CheckpointEncoding::PostcardV1))
            } else {
                decode_unframed(&bytes, agent_id, filename_version)
            };

            match decoded.and_then(|(checkpoint, encoding)| {
                validate_identity(&checkpoint, agent_id, filename_version)?;
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
                    // A checkpoint is a rebuildable execution cache. Do not
                    // brick the agent or hide an older valid transcript because
                    // one candidate is corrupt.
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

    fn checkpoint_name(version: u64) -> String {
        format!("{version:016}.ckpt")
    }

    async fn prune_old(&self, dir: &SecureDir, keep: usize) -> Result<(), MemoryError> {
        let mut versions: Vec<(u64, OsString)> = vec![];
        for entry in dir.entries()? {
            if entry.file_type != axocoatl_core::secure_fs::SecureEntryType::File {
                continue;
            }
            let path = Path::new(&entry.name);
            if path.extension().and_then(|extension| extension.to_str()) == Some("ckpt") {
                if let Some(version) = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse().ok())
                {
                    versions.push((version, entry.name));
                }
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

fn encode_current(checkpoint: &AgentCheckpoint) -> Result<Vec<u8>, MemoryError> {
    let payload = postcard::to_stdvec(checkpoint)
        .map_err(|error| MemoryError::Serialization(error.to_string()))?;
    let mut bytes = Vec::with_capacity(CHECKPOINT_MAGIC.len() + 1 + payload.len());
    bytes.extend_from_slice(CHECKPOINT_MAGIC);
    bytes.push(CHECKPOINT_FORMAT_VERSION);
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

fn decode_current(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
    let envelope = bytes
        .get(CHECKPOINT_MAGIC.len()..)
        .ok_or_else(|| "checkpoint envelope is truncated".to_string())?;
    let Some((&format_version, payload)) = envelope.split_first() else {
        return Err("checkpoint envelope is missing its format version".to_string());
    };
    match format_version {
        CHECKPOINT_FORMAT_VERSION_V1 => decode_postcard_payload_v1(payload),
        CHECKPOINT_FORMAT_VERSION => decode_postcard_payload_v2(payload),
        _ => Err(format!(
            "unsupported checkpoint format version {format_version}"
        )),
    }
}

fn decode_postcard_payload_v2(bytes: &[u8]) -> Result<AgentCheckpoint, String> {
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
    match decode_postcard_payload_v2(bytes) {
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
            .chunks_exact(2)
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

    #[test]
    fn framed_postcard_v1_decodes_with_conservative_usage_completeness() {
        let checkpoint = test_checkpoint("agent-1", 1);
        let legacy_payload = AgentCheckpointPostcardV1 {
            version: checkpoint.version,
            agent_id: checkpoint.agent_id,
            checkpoint_time: checkpoint.checkpoint_time,
            session_messages: checkpoint.session_messages,
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
            session_messages: checkpoint.session_messages,
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
