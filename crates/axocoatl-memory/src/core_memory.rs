//! Tier-3 **core memory** — agent-editable, curated memory blocks
//! (MemGPT/Letta-style). Replaces the old shared key-value long-term store.
//!
//! Each agent owns a small set of named blocks (`persona`, `human`, `project`,
//! …) that are rendered into the system prompt every turn and edited by the
//! agent itself via tools. Blocks marked `shared` live in a process-wide
//! [`SharedBlockRegistry`] so several agents see each other's edits.
//!
//! This is the **curated top** of the memory hierarchy — small and lossy by
//! design. Nothing is ever lost here, because the lossless raw lives below: the
//! daily log (Tier 2) and the semantic store (Tier 4).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::MemoryError;

/// A single named core-memory block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBlock {
    pub label: String,
    #[serde(default)]
    pub value: String,
    /// Character budget; `0` means unlimited.
    #[serde(default)]
    pub limit: usize,
    /// What this block is for — rendered as a hint when the block is empty.
    #[serde(default)]
    pub description: Option<String>,
    /// When true this block is backed by the [`SharedBlockRegistry`], not the
    /// per-agent store. (A routing flag at config/bootstrap time; an agent's
    /// own store only ever holds local blocks.)
    #[serde(default)]
    pub shared: bool,
}

impl MemoryBlock {
    pub fn new(label: impl Into<String>, limit: usize) -> Self {
        Self {
            label: label.into(),
            value: String::new(),
            limit,
            description: None,
            shared: false,
        }
    }

    fn fits(&self, len: usize) -> bool {
        self.limit == 0 || len <= self.limit
    }

    fn over_limit(&self, attempted: usize) -> MemoryError {
        MemoryError::BlockOverLimit {
            label: self.label.clone(),
            limit: self.limit,
            attempted,
        }
    }

    /// Append `text` (on its own line if the block is non-empty). Errors if the
    /// result would exceed `limit`.
    pub fn append(&mut self, text: &str) -> Result<(), MemoryError> {
        let sep = if self.value.is_empty() { 0 } else { 1 };
        let new_len = self.value.chars().count() + sep + text.chars().count();
        if !self.fits(new_len) {
            return Err(self.over_limit(new_len));
        }
        if sep == 1 {
            self.value.push('\n');
        }
        self.value.push_str(text);
        Ok(())
    }

    /// Replace the first occurrence of `old` with `new`. Errors if `old` is not
    /// present (the model sees this and can retry) or the result exceeds `limit`.
    pub fn replace(&mut self, old: &str, new: &str) -> Result<(), MemoryError> {
        if !self.value.contains(old) {
            return Err(MemoryError::Invalid(format!(
                "text to replace was not found in block '{}'",
                self.label
            )));
        }
        let replaced = self.value.replacen(old, new, 1);
        let new_len = replaced.chars().count();
        if !self.fits(new_len) {
            return Err(self.over_limit(new_len));
        }
        self.value = replaced;
        Ok(())
    }

    /// Overwrite the whole block value. Errors if it would exceed `limit`.
    pub fn set(&mut self, value: &str) -> Result<(), MemoryError> {
        let new_len = value.chars().count();
        if !self.fits(new_len) {
            return Err(self.over_limit(new_len));
        }
        self.value = value.to_string();
        Ok(())
    }

    /// Render this block as a labeled section for the system prompt.
    pub fn render(&self) -> String {
        let body = if self.value.trim().is_empty() {
            match &self.description {
                Some(d) => format!("(empty — {d})"),
                None => "(empty)".to_string(),
            }
        } else {
            self.value.clone()
        };
        format!("### {}\n{}", self.label, body)
    }
}

/// Per-agent core memory — an ordered set of blocks persisted as JSON.
///
/// A `Vec` (not a map) keeps render order deterministic; there are only a
/// handful of blocks, so linear lookup by label is fine.
#[derive(Debug)]
pub struct CoreMemoryStore {
    agent_id: String,
    path: PathBuf,
    blocks: Vec<MemoryBlock>,
}

impl CoreMemoryStore {
    pub fn new(agent_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            agent_id: agent_id.into(),
            path: path.into(),
            blocks: Vec::new(),
        }
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Load from disk (JSON). Missing file is not an error (fresh agent).
    pub async fn load(&mut self) -> Result<(), MemoryError> {
        if self.path.exists() {
            let bytes = tokio::fs::read(&self.path).await?;
            self.blocks = serde_json::from_slice(&bytes)?;
        }
        Ok(())
    }

    /// Save to disk — atomic (temp + rename), owner-only perms.
    pub async fn save(&self) -> Result<(), MemoryError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            crate::perms::restrict_dir(parent);
        }
        let bytes = serde_json::to_vec_pretty(&self.blocks)?;
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        crate::perms::restrict_file(&tmp);
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }

    /// Seed a block if it doesn't already exist — used to apply config defaults
    /// without clobbering an agent's curated value on reload.
    pub fn ensure_block(&mut self, block: MemoryBlock) {
        if !self.blocks.iter().any(|b| b.label == block.label) {
            self.blocks.push(block);
        }
    }

    pub fn block(&self, label: &str) -> Option<&MemoryBlock> {
        self.blocks.iter().find(|b| b.label == label)
    }

    pub fn block_mut(&mut self, label: &str) -> Option<&mut MemoryBlock> {
        self.blocks.iter_mut().find(|b| b.label == label)
    }

    pub fn blocks(&self) -> &[MemoryBlock] {
        &self.blocks
    }

    /// Render the agent's local blocks under a `## Core Memory` header. Empty
    /// store → empty string. Shared blocks render separately (the behavior
    /// concatenates both under one header).
    pub fn as_context_string(&self) -> String {
        render_blocks(self.blocks.iter())
    }
}

/// Render an iterator of blocks under the `## Core Memory` header (or "" if none).
pub fn render_blocks<'a>(blocks: impl Iterator<Item = &'a MemoryBlock>) -> String {
    let sections: Vec<String> = blocks.map(|b| b.render()).collect();
    if sections.is_empty() {
        String::new()
    } else {
        format!("## Core Memory\n{}", sections.join("\n\n"))
    }
}

/// A shared block handle: the block (cross-agent `Arc<RwLock>`) plus its own
/// file path so an editor can persist it without the registry.
#[derive(Clone)]
pub struct SharedBlock {
    pub block: Arc<RwLock<MemoryBlock>>,
    path: PathBuf,
}

impl SharedBlock {
    /// Persist the current value to disk — atomic, owner-only. Call after an edit.
    pub async fn persist(&self) -> Result<(), MemoryError> {
        let snapshot = self.block.read().await.clone();
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            crate::perms::restrict_dir(parent);
        }
        let bytes = serde_json::to_vec_pretty(&snapshot)?;
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        crate::perms::restrict_file(&tmp);
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

/// Process-wide registry of shared memory blocks. Built once at bootstrap; each
/// shared label is a single `Arc<RwLock<MemoryBlock>>` cloned into every agent
/// that references it, so edits are visible across agents.
#[derive(Default)]
pub struct SharedBlockRegistry {
    dir: PathBuf,
    blocks: HashMap<String, SharedBlock>,
}

impl SharedBlockRegistry {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            blocks: HashMap::new(),
        }
    }

    fn block_path(&self, label: &str) -> PathBuf {
        self.dir.join(format!("{label}.json"))
    }

    /// Register a shared block, loading its persisted value if present, else
    /// seeding from `default`. Idempotent: the first registration of a label
    /// wins, and later calls return the same handle (the existing value is kept,
    /// so two agents declaring the same shared label share one block).
    pub async fn ensure(&mut self, default: MemoryBlock) -> SharedBlock {
        let label = default.label.clone();
        if let Some(existing) = self.blocks.get(&label) {
            return existing.clone();
        }
        let path = self.block_path(&label);
        let block = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<MemoryBlock>(&bytes).unwrap_or(default),
            Err(_) => default,
        };
        let handle = SharedBlock {
            block: Arc::new(RwLock::new(block)),
            path,
        };
        self.blocks.insert(label, handle.clone());
        handle
    }

    pub fn get(&self, label: &str) -> Option<SharedBlock> {
        self.blocks.get(label).cloned()
    }
}

impl std::fmt::Debug for SharedBlockRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBlockRegistry")
            .field("dir", &self.dir)
            .field("labels", &self.blocks.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Build the per-agent core-memory store path under a data dir.
pub fn core_store_path(data_dir: &str, agent_id: &str) -> PathBuf {
    Path::new(data_dir)
        .join("memory")
        .join("core")
        .join(format!("agent_{agent_id}.json"))
}

/// The directory holding shared block files, under a data dir.
pub fn shared_blocks_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir)
        .join("memory")
        .join("core")
        .join("shared")
}

impl From<&axocoatl_core::CoreBlockConfig> for MemoryBlock {
    fn from(c: &axocoatl_core::CoreBlockConfig) -> Self {
        Self {
            label: c.label.clone(),
            value: c.value.clone(),
            limit: c.limit,
            description: c.description.clone(),
            shared: c.shared,
        }
    }
}

/// Load (or create) a per-agent store and seed it with the LOCAL (non-shared)
/// blocks from `specs`. Shared specs are skipped — they're resolved against the
/// [`SharedBlockRegistry`] instead. Best-effort load (a failure starts fresh).
pub async fn build_store(
    agent_id: &str,
    path: impl Into<PathBuf>,
    specs: &[MemoryBlock],
) -> CoreMemoryStore {
    let mut store = CoreMemoryStore::new(agent_id, path);
    if let Err(e) = store.load().await {
        tracing::warn!(agent = %agent_id, error = %e, "core memory load failed — starting fresh");
    }
    for spec in specs.iter().filter(|b| !b.shared) {
        store.ensure_block(spec.clone());
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_set_replace_and_limits() {
        let mut b = MemoryBlock::new("human", 20);
        b.append("name: Alice").unwrap();
        assert_eq!(b.value, "name: Alice");
        b.append("rust").unwrap(); // "name: Alice\nrust" = 16 chars, fits
        assert!(b.value.contains("rust"));
        // Over limit.
        assert!(matches!(
            b.append("way too much text here"),
            Err(MemoryError::BlockOverLimit { .. })
        ));
        // Replace present / absent.
        b.replace("Alice", "Bob").unwrap();
        assert!(b.value.contains("Bob"));
        assert!(b.replace("Zzz", "x").is_err());
        // Set respects limit.
        assert!(b.set("short").is_ok());
        assert!(matches!(
            b.set(&"x".repeat(21)),
            Err(MemoryError::BlockOverLimit { .. })
        ));
        // Unlimited block (limit 0).
        let mut u = MemoryBlock::new("notes", 0);
        u.set(&"x".repeat(10_000)).unwrap();
    }

    #[test]
    fn renders_in_order_with_header() {
        let mut s = CoreMemoryStore::new("a", "/tmp/unused.json");
        s.ensure_block(MemoryBlock::new("persona", 0));
        let mut human = MemoryBlock::new("human", 0);
        human.set("name: Alice").unwrap();
        s.ensure_block(human);
        let out = s.as_context_string();
        assert!(out.starts_with("## Core Memory"));
        // persona (empty) appears before human (order preserved).
        let p = out.find("### persona").unwrap();
        let h = out.find("### human").unwrap();
        assert!(p < h);
        assert!(out.contains("name: Alice"));
        // Empty store → empty string.
        assert_eq!(CoreMemoryStore::new("a", "/x.json").as_context_string(), "");
    }

    #[tokio::test]
    async fn store_round_trips_and_ensure_block_no_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent_a.json");
        let mut s = CoreMemoryStore::new("a", &path);
        let mut human = MemoryBlock::new("human", 0);
        human.set("name: Alice").unwrap();
        s.ensure_block(human);
        s.save().await.unwrap();

        let mut reloaded = CoreMemoryStore::new("a", &path);
        reloaded.load().await.unwrap();
        assert_eq!(reloaded.block("human").unwrap().value, "name: Alice");
        // ensure_block must NOT clobber the curated value with the config default.
        reloaded.ensure_block(MemoryBlock::new("human", 0));
        assert_eq!(reloaded.block("human").unwrap().value, "name: Alice");
    }

    #[tokio::test]
    async fn shared_block_clones_share_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SharedBlockRegistry::new(dir.path().join("shared"));
        let h1 = reg.ensure(MemoryBlock::new("team", 0)).await;
        // A second declaration of the same label returns the SAME block.
        let h2 = reg.ensure(MemoryBlock::new("team", 0)).await;
        h1.block.write().await.append("shared fact").unwrap();
        h1.persist().await.unwrap();
        // The other handle sees the write (one Arc<RwLock> per label).
        assert_eq!(h2.block.read().await.value, "shared fact");

        // Persisted to disk + reloadable.
        let mut reg2 = SharedBlockRegistry::new(dir.path().join("shared"));
        let h3 = reg2.ensure(MemoryBlock::new("team", 0)).await;
        assert_eq!(h3.block.read().await.value, "shared fact");
    }
}
