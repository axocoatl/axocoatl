//! JSONL session transcripts for persistence and observability.
//! Fire-and-forget writes via background tokio task.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use axocoatl_core::MessageRole;

/// A single entry in the session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub session_id: String,
    pub timestamp: u64,
    pub entry_type: TranscriptEntryType,
}

/// Types of transcript entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TranscriptEntryType {
    /// A chat message.
    Message {
        role: MessageRole,
        content: String,
        token_count: usize,
    },
    /// A tool call.
    ToolCall {
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// A tool result.
    ToolResult {
        tool_name: String,
        result: serde_json::Value,
        token_count: usize,
    },
    /// Session started.
    SessionStart { agent_id: String, model: String },
    /// Session ended.
    SessionEnd { total_tokens: usize },
    /// Compression event.
    Compression {
        stage: String,
        tokens_before: usize,
        tokens_after: usize,
    },
}

/// Background JSONL writer for session transcripts.
/// Uses an mpsc channel for fire-and-forget writes.
pub struct TranscriptWriter {
    tx: mpsc::UnboundedSender<TranscriptEntry>,
}

impl TranscriptWriter {
    /// Create a new transcript writer that writes to the given directory.
    /// Spawns a background task that handles writes.
    pub fn new(dir: impl AsRef<Path>, session_id: &str) -> Self {
        let file_path = dir.as_ref().join(format!("{session_id}.jsonl"));
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(Self::writer_loop(file_path, rx));

        Self { tx }
    }

    /// Fire-and-forget: write a transcript entry.
    pub fn write(&self, entry: TranscriptEntry) {
        let _ = self.tx.send(entry);
    }

    /// Write a message entry.
    pub fn write_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
        token_count: usize,
    ) {
        self.write(TranscriptEntry {
            session_id: session_id.to_string(),
            timestamp: now(),
            entry_type: TranscriptEntryType::Message {
                role,
                content: content.to_string(),
                token_count,
            },
        });
    }

    /// Write a tool call entry.
    pub fn write_tool_call(&self, session_id: &str, tool_name: &str, arguments: serde_json::Value) {
        self.write(TranscriptEntry {
            session_id: session_id.to_string(),
            timestamp: now(),
            entry_type: TranscriptEntryType::ToolCall {
                tool_name: tool_name.to_string(),
                arguments,
            },
        });
    }

    /// Write a tool result entry.
    pub fn write_tool_result(
        &self,
        session_id: &str,
        tool_name: &str,
        result: serde_json::Value,
        token_count: usize,
    ) {
        self.write(TranscriptEntry {
            session_id: session_id.to_string(),
            timestamp: now(),
            entry_type: TranscriptEntryType::ToolResult {
                tool_name: tool_name.to_string(),
                result,
                token_count,
            },
        });
    }

    /// Background writer loop — receives entries and appends to JSONL file.
    async fn writer_loop(file_path: PathBuf, mut rx: mpsc::UnboundedReceiver<TranscriptEntry>) {
        use tokio::io::AsyncWriteExt;

        // Ensure parent directory exists
        if let Some(parent) = file_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
            crate::perms::restrict_dir(parent);
        }

        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(path = %file_path.display(), error = %e, "Failed to open transcript file");
                return;
            }
        };
        // Transcripts persist every message + tool args/results verbatim —
        // owner-only on disk.
        crate::perms::restrict_file(&file_path);

        while let Some(entry) = rx.recv().await {
            match serde_json::to_string(&entry) {
                Ok(json) => {
                    let line = format!("{json}\n");
                    if let Err(e) = file.write_all(line.as_bytes()).await {
                        tracing::warn!(error = %e, "Failed to write transcript entry");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to serialize transcript entry");
                }
            }
        }

        // Flush on channel close
        let _ = file.flush().await;
    }
}

/// Read a session transcript from a JSONL file.
pub async fn read_transcript(
    path: impl AsRef<Path>,
) -> Result<Vec<TranscriptEntry>, crate::error::MemoryError> {
    let content = tokio::fs::read_to_string(path.as_ref())
        .await
        .map_err(|e| crate::error::MemoryError::Serialization(e.to_string()))?;

    let entries: Vec<TranscriptEntry> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<TranscriptEntry>(line).ok())
        .collect();

    Ok(entries)
}

/// List available session IDs in a transcript directory.
pub async fn list_sessions(
    dir: impl AsRef<Path>,
) -> Result<Vec<String>, crate::error::MemoryError> {
    let mut sessions = Vec::new();
    let mut dir_entries = tokio::fs::read_dir(dir.as_ref())
        .await
        .map_err(|e| crate::error::MemoryError::Serialization(e.to_string()))?;

    while let Ok(Some(entry)) = dir_entries.next_entry().await {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            if let Some(stem) = path.file_stem() {
                sessions.push(stem.to_string_lossy().to_string());
            }
        }
    }

    Ok(sessions)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_and_read_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "test-session";

        let writer = TranscriptWriter::new(tmp.path(), session_id);

        writer.write_message(session_id, MessageRole::User, "hello", 2);
        writer.write_message(session_id, MessageRole::Assistant, "hi there", 3);
        writer.write_tool_call(session_id, "echo", serde_json::json!({"text": "test"}));
        writer.write_tool_result(session_id, "echo", serde_json::json!({"text": "test"}), 5);

        // Drop writer to flush
        drop(writer);

        // Small delay to let background task finish
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let entries = read_transcript(tmp.path().join(format!("{session_id}.jsonl")))
            .await
            .unwrap();

        assert_eq!(entries.len(), 4);
        assert!(matches!(
            entries[0].entry_type,
            TranscriptEntryType::Message { .. }
        ));
    }

    #[tokio::test]
    async fn list_sessions_finds_files() {
        let tmp = tempfile::tempdir().unwrap();

        // Create some transcript files
        tokio::fs::write(tmp.path().join("session-1.jsonl"), "")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("session-2.jsonl"), "")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("other.txt"), "")
            .await
            .unwrap();

        let sessions = list_sessions(tmp.path()).await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn transcript_entry_serde() {
        let entry = TranscriptEntry {
            session_id: "test".to_string(),
            timestamp: 1234567890,
            entry_type: TranscriptEntryType::Message {
                role: MessageRole::User,
                content: "hello".to_string(),
                token_count: 2,
            },
        };

        let json = serde_json::to_string(&entry).unwrap();
        let back: TranscriptEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "test");
    }
}
