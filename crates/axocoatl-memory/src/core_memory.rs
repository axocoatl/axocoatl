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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axocoatl_core::secure_fs::SecureDir;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::MemoryError;
use crate::storage::{legacy_storage_component, storage_key};

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
    secure_parent: Option<SecureDir>,
    file_name: Option<OsString>,
    blocks: Vec<MemoryBlock>,
}

impl CoreMemoryStore {
    pub fn new(agent_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            agent_id: agent_id.into(),
            file_name: path.file_name().map(OsString::from),
            path,
            secure_parent: None,
            blocks: Vec::new(),
        }
    }

    /// Open one core-memory file relative to the control-plane data root.
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
        let relative = relative.as_ref();
        let file_name = relative
            .file_name()
            .ok_or_else(|| MemoryError::Invalid("core-memory path has no filename".to_string()))?
            .to_os_string();
        let parent = data_root.child(relative.parent().unwrap_or_else(|| Path::new("")))?;
        Ok(Self {
            agent_id: agent_id.into(),
            path: parent.path().join(&file_name),
            secure_parent: Some(parent),
            file_name: Some(file_name),
            blocks: Vec::new(),
        })
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Load from disk (JSON). Missing file is not an error (fresh agent).
    pub async fn load(&mut self) -> Result<(), MemoryError> {
        let (parent, name) = self.location()?;
        if parent.is_file(&name)? {
            self.blocks = serde_json::from_slice(&parent.read(name)?)?;
        }
        Ok(())
    }

    async fn load_from(&mut self, path: &Path) -> Result<bool, MemoryError> {
        let parent_path = path.parent().ok_or_else(|| {
            MemoryError::Invalid(format!(
                "core-memory path has no parent: {}",
                path.display()
            ))
        })?;
        let name = path.file_name().ok_or_else(|| {
            MemoryError::Invalid(format!(
                "core-memory path has no filename: {}",
                path.display()
            ))
        })?;
        let parent = SecureDir::open(parent_path)?;
        if parent.has_exact_file(name)? {
            self.blocks = serde_json::from_slice(&parent.read(name)?)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn load_from_secure(
        &mut self,
        dir: &SecureDir,
        name: &std::ffi::OsStr,
    ) -> Result<(), MemoryError> {
        self.blocks = serde_json::from_slice(&dir.read(name)?)?;
        Ok(())
    }

    /// Save to disk — atomic (temp + rename), owner-only perms.
    pub async fn save(&self) -> Result<(), MemoryError> {
        let bytes = serde_json::to_vec_pretty(&self.blocks)?;
        let (parent, name) = self.location()?;
        parent.atomic_write(name, &bytes)?;
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

    fn location(&self) -> Result<(SecureDir, OsString), MemoryError> {
        let name = self.file_name.clone().ok_or_else(|| {
            MemoryError::Invalid(format!(
                "core-memory path has no filename: {}",
                self.path.display()
            ))
        })?;
        let parent = match &self.secure_parent {
            Some(parent) => parent.clone(),
            None => {
                let parent = self.path.parent().ok_or_else(|| {
                    MemoryError::Invalid(format!(
                        "core-memory path has no parent: {}",
                        self.path.display()
                    ))
                })?;
                SecureDir::open_or_create_all(parent)?
            }
        };
        Ok((parent, name))
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
    secure_dir: Option<SecureDir>,
    file_name: OsString,
}

impl SharedBlock {
    /// Persist the current value to disk — atomic, owner-only. Call after an edit.
    pub async fn persist(&self) -> Result<(), MemoryError> {
        let snapshot = self.block.read().await.clone();
        let bytes = serde_json::to_vec_pretty(&snapshot)?;
        let dir = match &self.secure_dir {
            Some(dir) => dir.clone(),
            None => SecureDir::open_or_create_all(self.path.parent().ok_or_else(|| {
                MemoryError::Invalid(format!(
                    "shared block path has no parent: {}",
                    self.path.display()
                ))
            })?)?,
        };
        dir.atomic_write(&self.file_name, &bytes)?;
        Ok(())
    }
}

/// Process-wide registry of shared memory blocks. Built once at bootstrap; each
/// shared label is a single `Arc<RwLock<MemoryBlock>>` cloned into every agent
/// that references it, so edits are visible across agents.
#[derive(Default)]
pub struct SharedBlockRegistry {
    dir: PathBuf,
    legacy_dir: PathBuf,
    secure_dir: Option<SecureDir>,
    secure_legacy_dir: Option<SecureDir>,
    blocks: HashMap<String, SharedBlock>,
}

impl SharedBlockRegistry {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let legacy_dir = dir.into();
        Self {
            dir: legacy_dir.join("v1"),
            legacy_dir,
            secure_dir: None,
            secure_legacy_dir: None,
            blocks: HashMap::new(),
        }
    }

    pub fn new_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(&data_root, relative)
    }

    pub fn new_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let secure_legacy_dir = data_root.child(relative)?;
        let secure_dir = secure_legacy_dir.child("v1")?;
        Ok(Self {
            dir: secure_dir.path().to_path_buf(),
            legacy_dir: secure_legacy_dir.path().to_path_buf(),
            secure_dir: Some(secure_dir),
            secure_legacy_dir: Some(secure_legacy_dir),
            blocks: HashMap::new(),
        })
    }

    fn block_path(&self, label: &str) -> PathBuf {
        self.dir.join(format!("{}.json", storage_key(label)))
    }

    fn legacy_block_path(&self, label: &str) -> Option<PathBuf> {
        legacy_storage_component(label)
            .map(|component| self.legacy_dir.join(format!("{component}.json")))
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
        let file_name = path.file_name().unwrap_or_default().to_os_string();
        let secure_dir = self
            .secure_dir
            .clone()
            .or_else(|| SecureDir::open_or_create_all(&self.dir).ok());
        let legacy_name = self
            .legacy_block_path(&label)
            .and_then(|legacy| legacy.file_name().map(OsString::from));
        let legacy_dir = self
            .secure_legacy_dir
            .clone()
            .or_else(|| SecureDir::open_or_create_all(&self.legacy_dir).ok());
        let block = secure_dir
            .as_ref()
            .and_then(|dir| {
                if dir.is_file(&file_name).ok()? {
                    dir.read(&file_name).ok()
                } else if let (Some(legacy_dir), Some(legacy)) =
                    (legacy_dir.as_ref(), legacy_name.as_ref())
                {
                    legacy_dir
                        .has_exact_file(legacy)
                        .ok()
                        .filter(|exists| *exists)
                        .and_then(|_| legacy_dir.read(legacy).ok())
                } else {
                    None
                }
            })
            .and_then(|bytes| serde_json::from_slice::<MemoryBlock>(&bytes).ok())
            .unwrap_or(default);
        let handle = SharedBlock {
            block: Arc::new(RwLock::new(block)),
            path,
            secure_dir,
            file_name,
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
        .join("v1")
        .join(format!("agent_{}.json", storage_key(agent_id)))
}

/// Safe pre-1.0 per-agent core-memory path, used only as a migration source.
pub fn legacy_core_store_path(data_dir: &str, agent_id: &str) -> Option<PathBuf> {
    legacy_storage_component(agent_id).map(|component| {
        Path::new(data_dir)
            .join("memory")
            .join("core")
            .join(format!("agent_{component}.json"))
    })
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

/// Load a current core store, or one safe legacy raw-id file when the portable
/// location does not yet exist. A successful legacy read is immediately saved
/// to the portable path; the legacy file remains as a rollback source.
pub async fn build_store_with_legacy(
    agent_id: &str,
    path: impl Into<PathBuf>,
    legacy_path: Option<PathBuf>,
    specs: &[MemoryBlock],
) -> CoreMemoryStore {
    let path = path.into();
    let mut store = CoreMemoryStore::new(agent_id, &path);
    let mut loaded_legacy = false;
    let load_result = if path.exists() {
        store.load().await
    } else if let Some(legacy) = legacy_path {
        match store.load_from(&legacy).await {
            Ok(loaded) => {
                loaded_legacy = loaded;
                Ok(())
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    if let Err(error) = load_result {
        tracing::warn!(agent = %agent_id, error = %error, "core memory load failed — starting fresh");
    }
    for spec in specs.iter().filter(|block| !block.shared) {
        store.ensure_block(spec.clone());
    }
    if loaded_legacy {
        if let Err(error) = store.save().await {
            tracing::warn!(agent = %agent_id, error = %error, "core memory legacy promotion failed");
        }
    }
    store
}

/// Anchored variant used by the daemon for all control-plane core memory.
pub async fn build_store_with_legacy_in(
    agent_id: &str,
    data_root: impl AsRef<Path>,
    specs: &[MemoryBlock],
) -> Result<CoreMemoryStore, MemoryError> {
    let data_root = SecureDir::open(data_root)?;
    build_store_with_legacy_in_secure(agent_id, &data_root, specs).await
}

/// Same-root-capability variant used by the daemon after acquiring its data
/// directory lease.
pub async fn build_store_with_legacy_in_secure(
    agent_id: &str,
    data_root: &SecureDir,
    specs: &[MemoryBlock],
) -> Result<CoreMemoryStore, MemoryError> {
    let relative = Path::new("memory")
        .join("core")
        .join("v1")
        .join(format!("agent_{}.json", storage_key(agent_id)));
    let mut store = CoreMemoryStore::new_in_secure(agent_id, data_root, &relative)?;
    let (dir, current_name) = store.location()?;
    let current_exists = dir.is_file(&current_name)?;
    let legacy_dir = data_root.child("memory/core")?;
    let legacy_name = legacy_storage_component(agent_id)
        .map(|component| OsString::from(format!("agent_{component}.json")));
    let mut loaded_legacy = false;
    let load_result = if current_exists {
        store.load_from_secure(&dir, &current_name)
    } else if let Some(legacy) = legacy_name.as_ref() {
        if legacy_dir.has_exact_file(legacy)? {
            let result = store.load_from_secure(&legacy_dir, legacy);
            loaded_legacy = result.is_ok();
            result
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    if let Err(error) = load_result {
        tracing::warn!(agent = %agent_id, error = %error, "core memory load failed — starting fresh");
    }
    for spec in specs.iter().filter(|block| !block.shared) {
        store.ensure_block(spec.clone());
    }
    if loaded_legacy {
        store.save().await?;
    }
    Ok(store)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_core_store_promotes_safe_legacy_file_to_portable_path() {
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path().to_str().unwrap();
        let id = "ses-123:coder";
        let legacy = legacy_core_store_path(data_dir, id).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let mut block = MemoryBlock::new("project", 0);
        block.set("legacy value").unwrap();
        std::fs::write(&legacy, serde_json::to_vec(&vec![block]).unwrap()).unwrap();
        let current = core_store_path(data_dir, id);

        let store = build_store_with_legacy(id, &current, Some(legacy.clone()), &[]).await;
        assert_eq!(store.block("project").unwrap().value, "legacy value");
        assert!(current.is_file());
        assert!(legacy.is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_legacy_core_does_not_create_a_hiding_current_file() {
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path().to_str().unwrap();
        let id = "ses-123:coder";
        let legacy = legacy_core_store_path(data_dir, id).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"not json").unwrap();
        let current = core_store_path(data_dir, id);

        let _ = build_store_with_legacy(id, &current, Some(legacy), &[]).await;
        assert!(!current.exists());
    }

    #[tokio::test]
    async fn unsafe_core_and_shared_ids_stay_inside_the_memory_root() {
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path().to_str().unwrap();
        let outside = data.path().join("outside.json");
        std::fs::write(&outside, b"sentinel").unwrap();

        let core_path = core_store_path(data_dir, "../outside");
        let mut store = CoreMemoryStore::new("../outside", &core_path);
        store.ensure_block(MemoryBlock::new("project", 0));
        store.save().await.unwrap();
        assert_eq!(
            core_path.parent(),
            Some(data.path().join("memory/core/v1").as_path())
        );
        assert!(core_path.is_file());

        let shared_dir = shared_blocks_dir(data_dir);
        let mut registry = SharedBlockRegistry::new(&shared_dir);
        let shared = registry.ensure(MemoryBlock::new("../outside", 0)).await;
        shared.block.write().await.set("contained").unwrap();
        shared.persist().await.unwrap();
        assert!(shared_dir
            .join("v1")
            .join(format!("{}.json", storage_key("../outside")))
            .is_file());
        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lowercase_core_store_does_not_adopt_uppercase_legacy_file() {
        let data = tempfile::tempdir().unwrap();
        let data_root = SecureDir::open(data.path()).unwrap();
        let legacy = legacy_core_store_path(data.path().to_str().unwrap(), "Coder").unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let mut uppercase_block = MemoryBlock::new("project", 0);
        uppercase_block.set("uppercase legacy").unwrap();
        std::fs::write(&legacy, serde_json::to_vec(&vec![uppercase_block]).unwrap()).unwrap();

        let mut lowercase_default = MemoryBlock::new("project", 0);
        lowercase_default.set("lowercase default").unwrap();
        let lowercase = build_store_with_legacy_in_secure(
            "coder",
            &data_root,
            std::slice::from_ref(&lowercase_default),
        )
        .await
        .unwrap();
        assert_eq!(
            lowercase.block("project").unwrap().value,
            "lowercase default"
        );
        lowercase.save().await.unwrap();

        let uppercase = build_store_with_legacy_in_secure("Coder", &data_root, &[])
            .await
            .unwrap();
        assert_eq!(
            uppercase.block("project").unwrap().value,
            "uppercase legacy"
        );
        assert!(core_store_path(data.path().to_str().unwrap(), "coder").is_file());
        assert!(legacy.is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lowercase_shared_block_does_not_adopt_uppercase_legacy_file() {
        let data = tempfile::tempdir().unwrap();
        let data_root = SecureDir::open(data.path()).unwrap();
        let legacy_dir = data.path().join("memory/core/shared");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let mut uppercase = MemoryBlock::new("Project", 0);
        uppercase.set("uppercase legacy").unwrap();
        std::fs::write(
            legacy_dir.join("Project.json"),
            serde_json::to_vec(&uppercase).unwrap(),
        )
        .unwrap();

        let mut registry =
            SharedBlockRegistry::new_in_secure(&data_root, "memory/core/shared").unwrap();
        let mut lowercase_default = MemoryBlock::new("project", 0);
        lowercase_default.set("lowercase default").unwrap();
        let lowercase = registry.ensure(lowercase_default).await;
        assert_eq!(lowercase.block.read().await.value, "lowercase default");
        lowercase.persist().await.unwrap();

        let mut uppercase_registry =
            SharedBlockRegistry::new_in_secure(&data_root, "memory/core/shared").unwrap();
        let loaded_uppercase = uppercase_registry
            .ensure(MemoryBlock::new("Project", 0))
            .await;
        assert_eq!(
            loaded_uppercase.block.read().await.value,
            "uppercase legacy"
        );
        assert!(legacy_dir.join("v1/project.json").is_file());
        assert!(legacy_dir.join("Project.json").is_file());
    }
}
