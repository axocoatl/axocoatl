use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

/// Append-only daily log — survives process restarts.
/// Format: `{base_dir}/{agent_id}/YYYY-MM-DD.jsonl`
pub struct DailyLogMemory {
    agent_id: String,
    base_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: u64,
    pub entry_type: LogEntryType,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogEntryType {
    Conversation,
    ToolCall,
    Decision,
    Error,
    Note,
}

impl DailyLogMemory {
    pub fn new(agent_id: impl Into<String>, base_dir: impl Into<PathBuf>) -> Self {
        let agent_id = agent_id.into();
        let base_dir = base_dir.into().join(&agent_id);
        Self { agent_id, base_dir }
    }

    /// Append an entry to today's log.
    pub async fn append(&self, entry: LogEntry) -> Result<(), MemoryError> {
        tokio::fs::create_dir_all(&self.base_dir).await?;
        let path = self.log_path_today();

        let line = serde_json::to_string(&entry)? + "\n";
        tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await?
            .write_all_buf(&mut line.as_bytes())
            .await
            .map_err(MemoryError::Io)?;

        Ok(())
    }

    /// Read entries for a given date range (inclusive).
    pub async fn read_range(
        &self,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
    ) -> Result<Vec<LogEntry>, MemoryError> {
        let mut entries = Vec::new();
        let mut date = from;

        while date <= to {
            let path = self.log_path(date);
            if path.exists() {
                let content = tokio::fs::read_to_string(&path).await?;
                for line in content.lines() {
                    if let Ok(entry) = serde_json::from_str::<LogEntry>(line) {
                        entries.push(entry);
                    }
                }
            }
            date = date.succ_opt().unwrap_or(date);
            if date == from {
                break; // succ_opt returned same date (shouldn't happen but prevent infinite loop)
            }
        }

        Ok(entries)
    }

    /// Get the agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    fn log_path_today(&self) -> PathBuf {
        let today = chrono::Local::now().date_naive();
        self.log_path(today)
    }

    fn log_path(&self, date: chrono::NaiveDate) -> PathBuf {
        self.base_dir
            .join(format!("{}.jsonl", date.format("%Y-%m-%d")))
    }
}

// Need AsyncWriteExt for write_all_buf
use tokio::io::AsyncWriteExt;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(content: &str) -> LogEntry {
        LogEntry {
            timestamp: 1234567890,
            entry_type: LogEntryType::Note,
            content: serde_json::json!({"text": content}),
        }
    }

    #[tokio::test]
    async fn append_and_read_today() {
        let tmp = tempfile::tempdir().unwrap();
        let log = DailyLogMemory::new("test-agent", tmp.path());

        log.append(test_entry("first")).await.unwrap();
        log.append(test_entry("second")).await.unwrap();

        let today = chrono::Local::now().date_naive();
        let entries = log.read_range(today, today).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content["text"], "first");
        assert_eq!(entries[1].content["text"], "second");
    }

    #[tokio::test]
    async fn read_empty_range() {
        let tmp = tempfile::tempdir().unwrap();
        let log = DailyLogMemory::new("test-agent", tmp.path());

        let today = chrono::Local::now().date_naive();
        let entries = log.read_range(today, today).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn log_entry_serde_roundtrip() {
        let entry = LogEntry {
            timestamp: 999,
            entry_type: LogEntryType::ToolCall,
            content: serde_json::json!({"tool": "web_search", "result": "found"}),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.timestamp, 999);
    }
}
