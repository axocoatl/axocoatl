use std::path::{Path, PathBuf};

use axocoatl_core::secure_fs::SecureDir;
use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
#[cfg(test)]
use crate::storage::storage_path;
use crate::storage::{legacy_storage_component, legacy_storage_path, storage_key};

/// Append-only daily log — survives process restarts.
/// Format: `{base_dir}/v1/{portable_agent_key}/YYYY-MM-DD.jsonl`
pub struct DailyLogMemory {
    agent_id: String,
    base_dir: PathBuf,
    legacy_base_dir: Option<PathBuf>,
    secure_base: Option<SecureDir>,
    secure_legacy_base: Option<SecureDir>,
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
        let root = base_dir.into();
        let base_dir = root.join("v1").join(storage_key(&agent_id));
        let legacy_base_dir = legacy_storage_path(&root, &agent_id);
        Self {
            agent_id,
            base_dir,
            legacy_base_dir,
            secure_base: None,
            secure_legacy_base: None,
        }
    }

    /// Open an agent log beneath an already-created control-plane data root.
    pub fn new_in(
        agent_id: impl Into<String>,
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(agent_id, &data_root, relative)
    }

    pub fn new_in_secure(
        agent_id: impl Into<String>,
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let agent_id = agent_id.into();
        let root = data_root.child(relative)?;
        let secure_base = root.child(Path::new("v1").join(storage_key(&agent_id)))?;
        let secure_legacy_base = match legacy_storage_component(&agent_id) {
            Some(legacy) if root.has_exact_directory(legacy)? => Some(root.existing_child(legacy)?),
            _ => None,
        };
        Ok(Self {
            agent_id,
            base_dir: secure_base.path().to_path_buf(),
            legacy_base_dir: secure_legacy_base
                .as_ref()
                .map(|legacy| legacy.path().to_path_buf()),
            secure_base: Some(secure_base),
            secure_legacy_base,
        })
    }

    /// Append an entry to today's log.
    pub async fn append(&self, entry: LogEntry) -> Result<(), MemoryError> {
        self.append_at(chrono::Local::now().date_naive(), entry)
            .await
    }

    /// Append an entry to a specific date's log. `append` targets today; this
    /// lets callers pin an exact date, so behavior (and tests) never depend on a
    /// wall-clock midnight crossing splitting a batch across two date files.
    pub async fn append_at(
        &self,
        date: chrono::NaiveDate,
        entry: LogEntry,
    ) -> Result<(), MemoryError> {
        let line = serde_json::to_string(&entry)? + "\n";
        self.current_dir()?
            .append(Self::log_name(date), line.as_bytes(), true)?;

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
            for dir in self.dirs_for_read()? {
                let name = Self::log_name(date);
                if dir.is_file(&name)? {
                    let content = String::from_utf8(dir.read(&name)?)
                        .map_err(|error| MemoryError::Invalid(error.to_string()))?;
                    for line in content.lines() {
                        if let Ok(entry) = serde_json::from_str::<LogEntry>(line) {
                            entries.push(entry);
                        }
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

    fn log_name(date: chrono::NaiveDate) -> String {
        format!("{}.jsonl", date.format("%Y-%m-%d"))
    }

    fn current_dir(&self) -> Result<SecureDir, MemoryError> {
        self.secure_base.clone().map(Ok).unwrap_or_else(|| {
            SecureDir::open_or_create_all(&self.base_dir).map_err(MemoryError::from)
        })
    }

    fn dirs_for_read(&self) -> Result<Vec<SecureDir>, MemoryError> {
        let mut dirs = Vec::new();
        if let Some(legacy) = &self.secure_legacy_base {
            dirs.push(legacy.clone());
        } else if let Some(path) = &self.legacy_base_dir {
            let Some(parent_path) = path.parent() else {
                return Err(MemoryError::Invalid(format!(
                    "legacy daily-log path has no parent: {}",
                    path.display()
                )));
            };
            let Some(name) = path.file_name() else {
                return Err(MemoryError::Invalid(format!(
                    "legacy daily-log path has no component: {}",
                    path.display()
                )));
            };
            match SecureDir::open(parent_path) {
                Ok(parent) if parent.has_exact_directory(name)? => {
                    dirs.push(parent.existing_child(name)?);
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        dirs.push(self.current_dir()?);
        Ok(dirs)
    }
}

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
    async fn append_and_read_a_day() {
        let tmp = tempfile::tempdir().unwrap();
        let log = DailyLogMemory::new("test-agent", tmp.path());

        // Pin an explicit date so a real midnight crossing during the test can't
        // split the batch across two date files.
        let day = chrono::NaiveDate::from_ymd_opt(2020, 1, 15).unwrap();
        log.append_at(day, test_entry("first")).await.unwrap();
        log.append_at(day, test_entry("second")).await.unwrap();

        let entries = log.read_range(day, day).await.unwrap();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_id_reads_legacy_log_and_appends_only_to_portable_path() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "ses-123:coder";
        let day = chrono::NaiveDate::from_ymd_opt(2020, 1, 15).unwrap();
        let legacy_dir = tmp.path().join(id);
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        let legacy_line = serde_json::to_string(&test_entry("legacy")).unwrap() + "\n";
        tokio::fs::write(legacy_dir.join("2020-01-15.jsonl"), legacy_line)
            .await
            .unwrap();

        let log = DailyLogMemory::new(id, tmp.path());
        log.append_at(day, test_entry("current")).await.unwrap();
        let entries = log.read_range(day, day).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content["text"], "legacy");
        assert_eq!(entries[1].content["text"], "current");
        assert!(storage_path(tmp.path(), id)
            .join("2020-01-15.jsonl")
            .is_file());
    }

    #[tokio::test]
    async fn traversal_id_appends_under_the_supplied_root() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("daily");
        let outside = parent.path().join("outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        let sentinel = outside.join("sentinel");
        tokio::fs::write(&sentinel, b"safe").await.unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2020, 1, 15).unwrap();
        let log = DailyLogMemory::new("../outside", &root);
        log.append_at(day, test_entry("contained")).await.unwrap();

        assert!(storage_path(&root, "../outside")
            .join("2020-01-15.jsonl")
            .is_file());
        assert_eq!(tokio::fs::read(sentinel).await.unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lowercase_current_log_does_not_adopt_uppercase_legacy_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2020, 1, 15).unwrap();
        let legacy_dir = tmp.path().join("Coder");
        tokio::fs::create_dir_all(&legacy_dir).await.unwrap();
        let legacy_line = serde_json::to_string(&test_entry("uppercase legacy")).unwrap() + "\n";
        tokio::fs::write(legacy_dir.join("2020-01-15.jsonl"), legacy_line)
            .await
            .unwrap();

        let lowercase = DailyLogMemory::new("coder", tmp.path());
        lowercase
            .append_at(day, test_entry("lowercase current"))
            .await
            .unwrap();
        let entries = lowercase.read_range(day, day).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content["text"], "lowercase current");

        let uppercase = DailyLogMemory::new("Coder", tmp.path());
        let entries = uppercase.read_range(day, day).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content["text"], "uppercase legacy");
        assert!(storage_path(tmp.path(), "coder")
            .join("2020-01-15.jsonl")
            .is_file());
    }
}
