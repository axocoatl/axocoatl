use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use axocoatl_core::{AgentId, TokenUsageStats};

use crate::error::MemoryError;
use crate::session::StoredMessage;

/// Complete serializable snapshot of agent state.
#[derive(Debug, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct AgentCheckpoint {
    /// Monotonically increasing version.
    pub version: u64,
    pub agent_id: String,
    pub checkpoint_time: u64,
    /// All session messages (Tier 1).
    pub session_messages: Vec<StoredMessage>,
    /// Cumulative token usage.
    pub cumulative_token_usage: TokenUsageStats,
    /// Agent-specific state (behavior-defined, stored as JSON).
    pub behavior_state: Option<String>,
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
    _policy: CheckpointPolicy,
}

impl CheckpointStore {
    pub fn new(base_dir: impl Into<PathBuf>, policy: CheckpointPolicy) -> Self {
        Self {
            base_dir: base_dir.into(),
            _policy: policy,
        }
    }

    /// Save checkpoint (bincode, atomic write).
    pub async fn save(&self, checkpoint: &AgentCheckpoint) -> Result<(), MemoryError> {
        let dir = self.base_dir.join(&checkpoint.agent_id);
        tokio::fs::create_dir_all(&dir).await?;
        crate::perms::restrict_dir(&dir);

        let path = Self::checkpoint_path(&dir, checkpoint.version);
        let bytes = bincode::encode_to_vec(checkpoint, bincode::config::standard())
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;

        // Checkpoints hold full message + tool I/O verbatim — keep them
        // owner-only. Restrict the temp file before the rename so the final
        // file is never briefly world-readable.
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        crate::perms::restrict_file(&tmp);
        tokio::fs::rename(&tmp, &path).await?;

        self.prune_old(&dir, 3).await.ok();

        tracing::debug!(
            agent = %checkpoint.agent_id,
            version = checkpoint.version,
            bytes = bytes.len(),
            "Checkpoint saved"
        );
        Ok(())
    }

    /// Load the most recent valid checkpoint for an agent.
    pub async fn load_latest(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentCheckpoint>, MemoryError> {
        let dir = self.base_dir.join(&agent_id.0);
        if !dir.exists() {
            return Ok(None);
        }

        let mut latest: Option<(u64, PathBuf)> = None;
        let mut entries = tokio::fs::read_dir(&dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("ckpt") {
                if let Some(version) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    match &latest {
                        None => latest = Some((version, path)),
                        Some((v, _)) if version > *v => latest = Some((version, path)),
                        _ => {}
                    }
                }
            }
        }

        match latest {
            None => Ok(None),
            Some((_, path)) => {
                let bytes = tokio::fs::read(&path).await?;
                // A checkpoint is a local, regenerable cache of session state —
                // never a source of truth. If the bytes don't decode (corruption,
                // or a schema change across an Axocoatl upgrade), we must NOT
                // brick the agent: log and start fresh instead of propagating a
                // fatal deserialization error up through `on_start`.
                match bincode::decode_from_slice::<AgentCheckpoint, _>(
                    &bytes,
                    bincode::config::standard(),
                ) {
                    Ok((checkpoint, _)) => Ok(Some(checkpoint)),
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "Checkpoint failed to decode (corrupt or from an older \
                             Axocoatl version) — discarding it and starting fresh"
                        );
                        Ok(None)
                    }
                }
            }
        }
    }

    fn checkpoint_path(dir: &std::path::Path, version: u64) -> PathBuf {
        dir.join(format!("{:016}.ckpt", version))
    }

    async fn prune_old(&self, dir: &std::path::Path, keep: usize) -> Result<(), MemoryError> {
        let mut versions: Vec<(u64, PathBuf)> = vec![];
        let mut entries = tokio::fs::read_dir(dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("ckpt") {
                if let Some(v) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse().ok())
                {
                    versions.push((v, path));
                }
            }
        }

        versions.sort_by_key(|(v, _)| *v);
        if versions.len() > keep {
            for (_, path) in versions.iter().take(versions.len() - keep) {
                tokio::fs::remove_file(path).await.ok();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::MessageRole;

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
            behavior_state: None,
        }
    }

    #[tokio::test]
    async fn save_and_load_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);

        let ckpt = test_checkpoint("agent-1", 1);
        store.save(&ckpt).await.unwrap();

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
    async fn prune_keeps_last_n() {
        let tmp = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(tmp.path(), CheckpointPolicy::Manual);

        // Save 5 checkpoints — pruning keeps last 3
        for v in 1..=5 {
            store.save(&test_checkpoint("agent-1", v)).await.unwrap();
        }

        let dir = tmp.path().join("agent-1");
        let mut count = 0;
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while entries.next_entry().await.unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 3); // Only last 3 kept
    }

    #[tokio::test]
    async fn checkpoint_serde_roundtrip() {
        let ckpt = test_checkpoint("test", 42);
        let bytes = bincode::encode_to_vec(&ckpt, bincode::config::standard()).unwrap();
        let (decoded, _): (AgentCheckpoint, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(decoded.version, 42);
        assert_eq!(decoded.agent_id, "test");
    }
}
