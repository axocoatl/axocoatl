use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

/// Maximum number of entries kept before pruning evicts the weakest.
/// Long-term memory is curated and high-signal — it should stay small.
pub const MAX_ENTRIES: usize = 200;

/// Curated long-term memory — high-signal facts, decisions, user preferences.
/// Serialized to disk using bincode for fast read/write.
pub struct LongTermMemory {
    storage_path: PathBuf,
    entries: HashMap<String, MemoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub category: MemoryCategory,
    pub confidence: f32,
    pub created_at: u64,
    pub last_accessed: u64,
    pub access_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum MemoryCategory {
    UserPreference,
    ProjectContext,
    Decision,
    Fact,
    Relationship,
    Skill,
}

impl LongTermMemory {
    pub fn new(storage_path: impl Into<PathBuf>) -> Self {
        Self {
            storage_path: storage_path.into(),
            entries: HashMap::new(),
        }
    }

    /// Load from disk.
    pub async fn load(&mut self) -> Result<(), MemoryError> {
        if self.storage_path.exists() {
            let bytes = tokio::fs::read(&self.storage_path).await?;
            let (entries, _): (HashMap<String, MemoryEntry>, _) =
                bincode::decode_from_slice(&bytes, bincode::config::standard())
                    .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            self.entries = entries;
        }
        Ok(())
    }

    /// Save to disk (atomic write: temp file then rename).
    pub async fn save(&self) -> Result<(), MemoryError> {
        if let Some(parent) = self.storage_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            crate::perms::restrict_dir(parent);
        }
        let bytes = bincode::encode_to_vec(&self.entries, bincode::config::standard())
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;

        // Long-term memory can contain anything the agent chose to remember —
        // owner-only on disk.
        let tmp_path = self.storage_path.with_extension("tmp");
        tokio::fs::write(&tmp_path, &bytes).await?;
        crate::perms::restrict_file(&tmp_path);
        tokio::fs::rename(&tmp_path, &self.storage_path).await?;
        Ok(())
    }

    /// Get a memory entry by key (updates access stats).
    pub fn get(&mut self, key: &str) -> Option<&MemoryEntry> {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_accessed = now_timestamp();
            entry.access_count += 1;
        }
        self.entries.get(key)
    }

    /// Set a memory entry.
    pub fn set(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
        category: MemoryCategory,
    ) {
        let key = key.into();
        let now = now_timestamp();
        self.entries.insert(
            key.clone(),
            MemoryEntry {
                key,
                value: value.into(),
                category,
                confidence: 1.0,
                created_at: now,
                last_accessed: now,
                access_count: 0,
            },
        );
        // Keep long-term memory bounded — never let it grow without limit.
        self.prune();
    }

    /// Remove a memory entry.
    pub fn remove(&mut self, key: &str) -> Option<MemoryEntry> {
        self.entries.remove(key)
    }

    /// Evict the lowest-value entries until at most `MAX_ENTRIES` remain.
    /// Value ranks by `access_count`, tie-broken by recency (`last_accessed`):
    /// rarely-used, stale facts go first. Called automatically by `set`.
    pub fn prune(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut ranked: Vec<(String, u32, u64)> = self
            .entries
            .values()
            .map(|e| (e.key.clone(), e.access_count, e.last_accessed))
            .collect();
        // Lowest access_count first, then oldest last_accessed first.
        ranked.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)));
        let evict = self.entries.len() - MAX_ENTRIES;
        for (key, _, _) in ranked.into_iter().take(evict) {
            self.entries.remove(&key);
        }
    }

    /// True once long-term memory has grown enough that an LLM consolidation
    /// pass (merging redundant facts) is worthwhile.
    pub fn needs_consolidation(&self) -> bool {
        self.entries.len() >= MAX_ENTRIES * 3 / 4
    }

    /// Snapshot of all entries — for building a consolidation prompt.
    pub fn entries(&self) -> Vec<MemoryEntry> {
        self.entries.values().cloned().collect()
    }

    /// Replace the entire entry set with a consolidated one (the LLM result
    /// from `consolidate_facts_prompt` + `parse_extracted_facts`).
    pub fn replace_all(&mut self, facts: Vec<(MemoryCategory, String, String)>) {
        self.entries.clear();
        for (category, key, value) in facts {
            self.set(key, value, category);
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Format all entries as a compact context string for LLM prompts.
    pub fn as_context_string(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let lines: Vec<String> = self
            .entries
            .values()
            .map(|e| format!("[{:?}] {}: {}", e.category, e.key, e.value))
            .collect();
        format!("## Agent Memory\n{}", lines.join("\n"))
    }
}

/// Build a prompt that asks the LLM to extract memorable facts from a conversation.
/// Returns (system_prompt, user_prompt) — the caller sends these to the LLM and parses
/// the response as one-fact-per-line in "category: key = value" format.
pub fn extract_facts_prompt(conversation_messages: &[String]) -> (String, String) {
    let system = "You are a memory extraction assistant. Your job is to identify facts, preferences, \
        decisions, and relationships from conversations that should be remembered for future sessions.\n\
        \n\
        Output one fact per line in this exact format:\n\
        CATEGORY: key = value\n\
        \n\
        Valid categories: UserPreference, ProjectContext, Decision, Fact, Relationship, Skill\n\
        \n\
        Rules:\n\
        - Only extract high-signal information worth remembering across sessions\n\
        - Prefer concise, factual statements\n\
        - Skip ephemeral details (greetings, debugging back-and-forth)\n\
        - If there is nothing worth remembering, output: NONE\n\
        \n\
        Examples:\n\
        UserPreference: preferred_language = Rust\n\
        Decision: auth_approach = JWT with refresh tokens\n\
        Fact: user_name = Alice\n\
        ProjectContext: deployment_target = AWS ECS Fargate";

    let conversation = conversation_messages.join("\n");
    let user = format!(
        "Extract memorable facts from this conversation:\n\n{conversation}\n\n\
         Remember: one fact per line, format: CATEGORY: key = value"
    );

    (system.to_string(), user)
}

/// Build a prompt asking the LLM to consolidate a set of memory entries —
/// merging duplicates, dropping superseded/contradictory facts, tightening
/// wording. The response uses the same `CATEGORY: key = value` format as
/// `extract_facts_prompt`, so `parse_extracted_facts` parses it unchanged.
pub fn consolidate_facts_prompt(entries: &[MemoryEntry]) -> (String, String) {
    let system = "You are a memory consolidation assistant. You are given a list of \
        remembered facts. Merge duplicates, drop superseded or contradictory entries \
        (keep the most recent / most specific), and tighten wording. Preserve every \
        distinct piece of high-signal information — do not lose facts.\n\
        \n\
        Output the consolidated set, one fact per line:\n\
        CATEGORY: key = value\n\
        \n\
        Valid categories: UserPreference, ProjectContext, Decision, Fact, Relationship, Skill";

    let listing: Vec<String> = entries
        .iter()
        .map(|e| format!("{:?}: {} = {}", e.category, e.key, e.value))
        .collect();
    let user = format!(
        "Consolidate these {} memory entries:\n\n{}\n\n\
         Remember: one fact per line, format: CATEGORY: key = value",
        entries.len(),
        listing.join("\n")
    );

    (system.to_string(), user)
}

/// Parse the LLM's fact extraction response into (category, key, value) tuples.
pub fn parse_extracted_facts(response: &str) -> Vec<(MemoryCategory, String, String)> {
    let mut facts = Vec::new();
    for line in response.lines() {
        let line = line.trim();
        if line.is_empty() || line == "NONE" {
            continue;
        }
        // Parse "CATEGORY: key = value"
        if let Some((cat_str, rest)) = line.split_once(':') {
            if let Some((key, value)) = rest.split_once('=') {
                let category = match cat_str.trim() {
                    "UserPreference" => MemoryCategory::UserPreference,
                    "ProjectContext" => MemoryCategory::ProjectContext,
                    "Decision" => MemoryCategory::Decision,
                    "Fact" => MemoryCategory::Fact,
                    "Relationship" => MemoryCategory::Relationship,
                    "Skill" => MemoryCategory::Skill,
                    _ => MemoryCategory::Fact, // default fallback
                };
                facts.push((category, key.trim().to_string(), value.trim().to_string()));
            }
        }
    }
    facts
}

fn now_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mem = LongTermMemory::new(tmp.path().join("memory.bin"));

        mem.set("user_name", "Alice", MemoryCategory::UserPreference);
        mem.set("project", "Axocoatl", MemoryCategory::ProjectContext);

        assert_eq!(mem.len(), 2);
        let entry = mem.get("user_name").unwrap();
        assert_eq!(entry.value, "Alice");
        assert_eq!(entry.access_count, 1);
    }

    #[tokio::test]
    async fn save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("memory.bin");

        {
            let mut mem = LongTermMemory::new(&path);
            mem.set("key1", "value1", MemoryCategory::Fact);
            mem.set("key2", "value2", MemoryCategory::Decision);
            mem.save().await.unwrap();
        }

        {
            let mut mem = LongTermMemory::new(&path);
            mem.load().await.unwrap();
            assert_eq!(mem.len(), 2);
            let entry = mem.get("key1").unwrap();
            assert_eq!(entry.value, "value1");
        }
    }

    #[tokio::test]
    async fn load_nonexistent_file() {
        let mut mem = LongTermMemory::new("/tmp/nonexistent_axocoatl_test.bin");
        // Should succeed with empty entries
        mem.load().await.unwrap();
        assert!(mem.is_empty());
    }

    #[test]
    fn remove_entry() {
        let mut mem = LongTermMemory::new("/tmp/unused.bin");
        mem.set("temp", "data", MemoryCategory::Fact);
        assert_eq!(mem.len(), 1);
        mem.remove("temp");
        assert!(mem.is_empty());
    }

    #[test]
    fn as_context_string() {
        let mut mem = LongTermMemory::new("/tmp/unused.bin");
        mem.set("name", "Alice", MemoryCategory::UserPreference);
        let ctx = mem.as_context_string();
        assert!(ctx.contains("## Agent Memory"));
        assert!(ctx.contains("Alice"));
    }

    #[test]
    fn empty_context_string() {
        let mem = LongTermMemory::new("/tmp/unused.bin");
        assert!(mem.as_context_string().is_empty());
    }

    #[test]
    fn prune_bounds_memory_and_keeps_high_value_entries() {
        let mut mem = LongTermMemory::new("/tmp/unused.bin");
        // Fill exactly to the cap — no pruning yet, every entry present.
        for i in 0..MAX_ENTRIES {
            mem.set(format!("k{i}"), format!("v{i}"), MemoryCategory::Fact);
        }
        assert_eq!(mem.len(), MAX_ENTRIES);

        // Make one entry "hot" — give it a non-zero access count.
        for _ in 0..10 {
            assert!(mem.get("k100").is_some());
        }

        // Insert past the cap — each `set` auto-prunes. Every cold entry has
        // access_count 0; the hot one has 10, so it is never in the eviction
        // prefix (which takes the lowest access_count first).
        for i in MAX_ENTRIES..(MAX_ENTRIES + 50) {
            mem.set(format!("k{i}"), format!("v{i}"), MemoryCategory::Fact);
        }
        assert_eq!(mem.len(), MAX_ENTRIES, "memory stays bounded");
        assert!(mem.get("k100").is_some(), "hot entry must survive pruning");
    }

    #[test]
    fn consolidation_prompt_and_replace_all() {
        let mut mem = LongTermMemory::new("/tmp/unused.bin");
        mem.set("name", "Alice", MemoryCategory::UserPreference);
        mem.set("lang", "Rust", MemoryCategory::UserPreference);
        let (system, user) = consolidate_facts_prompt(&mem.entries());
        assert!(system.contains("consolidation"));
        assert!(user.contains("Alice") && user.contains("Rust"));

        mem.replace_all(vec![(
            MemoryCategory::Fact,
            "merged".into(),
            "one fact".into(),
        )]);
        assert_eq!(mem.len(), 1);
        assert_eq!(mem.get("merged").unwrap().value, "one fact");
    }
}
