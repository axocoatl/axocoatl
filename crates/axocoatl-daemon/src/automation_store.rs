//! Persistent store for [`Automation`].
//!
//! Single source of truth for the running daemon. The store lives in
//! `{data_dir}/automations.json`. On first boot (when that file doesn't
//! exist) we seed it from the legacy YAML sections — `workflows:`,
//! `schedules:`, `proactive:` — via [`Automation::from_legacy`]. After
//! that, the store is authoritative: edits made in the dashboard
//! visual editor are persisted here.
//!
//! Concurrency model: the store is held behind a single `RwLock` inside
//! the daemon. Reads (list/get) are cheap. Writes (CRUD) acquire the
//! write lock for the duration of the in-memory mutation and the JSON
//! save. The save uses temp-write + rename, so a crash mid-write leaves
//! the previous good file in place.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use axocoatl_config::{Automation, AutomationFolder, AxocoatlConfig};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("automation '{0}' not found")]
    NotFound(String),
    #[error("automation '{0}' already exists")]
    Conflict(String),
    #[error("folder '{0}' not found")]
    FolderNotFound(String),
    #[error("folder '{0}' already exists")]
    FolderConflict(String),
    #[error("invalid folder path: {0}")]
    InvalidFolderPath(String),
}

/// In-memory store of automations + the organizational folders they sit in.
/// Two JSON files share this struct: `automations.json` (the original) and
/// `automation-folders.json` (sibling, holds explicit folder entities so
/// empty folders survive across daemon restarts).
pub struct AutomationStore {
    path: PathBuf,
    folders_path: PathBuf,
    by_id: HashMap<String, Automation>,
    folders_by_path: HashMap<String, AutomationFolder>,
}

/// Validate a slash-separated folder path. Returns `Ok` for the empty string
/// (treated as "root"), otherwise checks every segment is non-empty and
/// contains no path separators. Folders only live in this logical namespace
/// — they aren't real OS directories — but we still reject ambiguous chars
/// so users can't paste in `..` or backslashes and get surprising behavior.
fn validate_folder_path(path: &str) -> Result<(), StoreError> {
    if path.is_empty() {
        return Ok(());
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err(StoreError::InvalidFolderPath(format!(
            "leading or trailing slash: '{path}'"
        )));
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(StoreError::InvalidFolderPath(format!(
                "bad segment in '{path}'"
            )));
        }
        if seg.contains('\\') {
            return Err(StoreError::InvalidFolderPath(format!(
                "backslash in '{path}'"
            )));
        }
    }
    Ok(())
}

impl AutomationStore {
    /// Open the store. Loads existing automations from disk if the file
    /// is present. Empty starting state otherwise.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        // Sibling file in the same directory. We pick a sibling so the user's
        // single-file backup story stays intact (copy automations.json, get
        // its folders alongside).
        let folders_path = path.with_file_name("automation-folders.json");
        let mut store = Self {
            path,
            folders_path,
            by_id: HashMap::new(),
            folders_by_path: HashMap::new(),
        };
        if store.path.exists() {
            let bytes = std::fs::read(&store.path)?;
            if !bytes.is_empty() {
                let list: Vec<Automation> = serde_json::from_slice(&bytes)?;
                for a in list {
                    store.by_id.insert(a.id.clone(), a);
                }
            }
        }
        if store.folders_path.exists() {
            let bytes = std::fs::read(&store.folders_path)?;
            if !bytes.is_empty() {
                let list: Vec<AutomationFolder> = serde_json::from_slice(&bytes)?;
                for f in list {
                    store.folders_by_path.insert(f.path.clone(), f);
                }
            }
        }
        Ok(store)
    }

    /// One-time seed from the legacy YAML sections. Idempotent — if any
    /// automation already exists in the store we leave the store alone
    /// (the user has been editing in the UI; YAML is no longer truth).
    pub fn seed_from_legacy_if_empty(&mut self, cfg: &AxocoatlConfig) -> Result<bool, StoreError> {
        if !self.by_id.is_empty() {
            return Ok(false);
        }
        let deps = |aid: &str| -> Vec<String> {
            cfg.agents
                .iter()
                .find(|a| a.id == aid)
                .map(|a| a.depends_on.clone())
                .unwrap_or_default()
        };
        let automations =
            Automation::from_legacy(&cfg.workflows, &cfg.schedules, &cfg.proactive, &deps);
        for a in automations {
            self.by_id.insert(a.id.clone(), a);
        }
        self.persist()?;
        Ok(true)
    }

    /// Atomic-write the store's JSON to disk.
    fn persist(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut list: Vec<&Automation> = self.by_id.values().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        let bytes = serde_json::to_vec_pretty(&list)?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Persist the folder list to its sibling file. Same temp+rename pattern
    /// as `persist()`. Called after every folder mutation.
    fn persist_folders(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.folders_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.folders_path.with_extension("json.tmp");
        let mut list: Vec<&AutomationFolder> = self.folders_by_path.values().collect();
        list.sort_by(|a, b| a.path.cmp(&b.path));
        let bytes = serde_json::to_vec_pretty(&list)?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.folders_path)?;
        Ok(())
    }

    // ── Folder CRUD ────────────────────────────────────────────────
    // Folders are organizational paths. They auto-create ancestors on demand
    // (creating "client/spec-reviews" creates "client" too if missing), and
    // renaming/deleting a folder cascades to its sub-folders + automations
    // sitting under it.

    /// All folders, sorted by path so callers get a stable tree-friendly order.
    pub fn list_folders(&self) -> Vec<AutomationFolder> {
        let mut v: Vec<AutomationFolder> = self.folders_by_path.values().cloned().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    /// Create a folder. Idempotent — if the path already exists with the
    /// same name, this is a no-op. Auto-creates missing ancestor folders so
    /// the user can drop a deep path and not have to scaffold the chain.
    pub fn create_folder(
        &mut self,
        path: &str,
        name: Option<String>,
    ) -> Result<AutomationFolder, StoreError> {
        validate_folder_path(path)?;
        if path.is_empty() {
            return Err(StoreError::InvalidFolderPath("cannot create root".into()));
        }
        if let Some(existing) = self.folders_by_path.get(path) {
            return Ok(existing.clone());
        }
        // Auto-create ancestors.
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            let ancestor = parts[..i].join("/");
            if !self.folders_by_path.contains_key(&ancestor) {
                self.folders_by_path.insert(
                    ancestor.clone(),
                    AutomationFolder {
                        path: ancestor,
                        name: None,
                    },
                );
            }
        }
        let f = AutomationFolder {
            path: path.to_string(),
            name,
        };
        self.folders_by_path.insert(path.to_string(), f.clone());
        self.persist_folders()?;
        Ok(f)
    }

    /// Rename / re-path a folder. If `new_path` differs from `old_path`:
    /// every sub-folder is re-pathed (`old_path/child` → `new_path/child`)
    /// and every Automation sitting under it gets its `folder` field updated.
    pub fn rename_folder(
        &mut self,
        old_path: &str,
        new_path: &str,
        new_name: Option<String>,
    ) -> Result<AutomationFolder, StoreError> {
        validate_folder_path(new_path)?;
        if new_path.is_empty() {
            return Err(StoreError::InvalidFolderPath(
                "cannot rename to root".into(),
            ));
        }
        if !self.folders_by_path.contains_key(old_path) {
            return Err(StoreError::FolderNotFound(old_path.into()));
        }
        if old_path != new_path && self.folders_by_path.contains_key(new_path) {
            return Err(StoreError::FolderConflict(new_path.into()));
        }
        // Drain every folder under `old_path` and re-insert under `new_path`.
        if old_path != new_path {
            let drained: Vec<(String, AutomationFolder)> = self.folders_by_path.drain().collect();
            for (key, mut f) in drained {
                if key == old_path {
                    f.path = new_path.to_string();
                } else if let Some(rest) = key.strip_prefix(&format!("{old_path}/")) {
                    f.path = format!("{new_path}/{rest}");
                }
                self.folders_by_path.insert(f.path.clone(), f);
            }
            // Migrate automations: anything whose folder was under old_path
            // now lives under new_path.
            for a in self.by_id.values_mut() {
                if let Some(folder) = a.folder.clone() {
                    if folder == old_path {
                        a.folder = Some(new_path.to_string());
                    } else if let Some(rest) = folder.strip_prefix(&format!("{old_path}/")) {
                        a.folder = Some(format!("{new_path}/{rest}"));
                    }
                }
            }
            self.persist()?;
        }
        // Apply name override.
        let folder = self.folders_by_path.get_mut(new_path).unwrap();
        if let Some(n) = new_name {
            folder.name = if n.trim().is_empty() { None } else { Some(n) };
        }
        let snap = folder.clone();
        self.persist_folders()?;
        Ok(snap)
    }

    /// Delete a folder. `keep_contents = true` migrates contents up to the
    /// parent folder (preserves automations); `false` deletes recursively
    /// (folders + every automation under it disappears).
    pub fn delete_folder(&mut self, path: &str, keep_contents: bool) -> Result<usize, StoreError> {
        if !self.folders_by_path.contains_key(path) {
            return Err(StoreError::FolderNotFound(path.into()));
        }
        let parent = path.rsplit_once('/').map(|(p, _)| p.to_string());

        // Find affected folders + automations.
        let affected_folders: Vec<String> = self
            .folders_by_path
            .keys()
            .filter(|k| *k == path || k.starts_with(&format!("{path}/")))
            .cloned()
            .collect();
        let affected_autos: Vec<String> = self
            .by_id
            .iter()
            .filter(|(_, a)| match &a.folder {
                Some(f) => f == path || f.starts_with(&format!("{path}/")),
                None => false,
            })
            .map(|(id, _)| id.clone())
            .collect();

        if keep_contents {
            // Re-parent automations to the deleted folder's parent (None = root).
            for id in &affected_autos {
                if let Some(a) = self.by_id.get_mut(id) {
                    a.folder = parent.clone();
                }
            }
            for k in &affected_folders {
                self.folders_by_path.remove(k);
            }
        } else {
            for k in &affected_folders {
                self.folders_by_path.remove(k);
            }
            for id in &affected_autos {
                self.by_id.remove(id);
            }
        }
        self.persist()?;
        self.persist_folders()?;
        Ok(affected_autos.len())
    }

    pub fn list(&self) -> Vec<Automation> {
        let mut v: Vec<Automation> = self.by_id.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn get(&self, id: &str) -> Option<Automation> {
        self.by_id.get(id).cloned()
    }

    pub fn upsert(&mut self, a: Automation) -> Result<Automation, StoreError> {
        let id = a.id.clone();
        self.by_id.insert(id.clone(), a.clone());
        self.persist()?;
        Ok(a)
    }

    pub fn create(&mut self, a: Automation) -> Result<Automation, StoreError> {
        if self.by_id.contains_key(&a.id) {
            return Err(StoreError::Conflict(a.id));
        }
        self.upsert(a)
    }

    pub fn delete(&mut self, id: &str) -> Result<(), StoreError> {
        if self.by_id.remove(id).is_none() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        self.persist()?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::{AutomationNode, AutomationNodeKind, AutomationTrigger, NodeInput};
    use std::path::PathBuf;

    fn auto(id: &str) -> Automation {
        Automation {
            id: id.into(),
            name: id.into(),
            description: None,
            nodes: vec![AutomationNode {
                id: "n1".into(),
                kind: AutomationNodeKind::Agent {
                    agent_id: "coder".into(),
                    input: NodeInput::FromTrigger,
                },
                position: None,
            }],
            edges: vec![],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        }
    }

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "axo-store-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn open_empty_then_create_then_reload() {
        let dir = tmpdir();
        let p = dir.join("automations.json");

        let mut s = AutomationStore::open(&p).unwrap();
        assert!(s.is_empty());

        s.create(auto("hello")).unwrap();
        assert_eq!(s.len(), 1);

        // Reopen — data should round-trip
        let s2 = AutomationStore::open(&p).unwrap();
        assert_eq!(s2.len(), 1);
        assert_eq!(s2.get("hello").unwrap().name, "hello");
    }

    #[test]
    fn create_rejects_duplicate() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create(auto("x")).unwrap();
        assert!(matches!(s.create(auto("x")), Err(StoreError::Conflict(_))));
    }

    #[test]
    fn upsert_replaces_existing() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create(auto("x")).unwrap();
        let mut updated = auto("x");
        updated.name = "renamed".into();
        s.upsert(updated).unwrap();
        assert_eq!(s.get("x").unwrap().name, "renamed");
    }

    #[test]
    fn delete_actually_persists() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create(auto("a")).unwrap();
        s.create(auto("b")).unwrap();
        s.delete("a").unwrap();
        let s2 = AutomationStore::open(&p).unwrap();
        assert_eq!(s2.len(), 1);
        assert!(s2.get("a").is_none());
        assert!(s2.get("b").is_some());
    }

    #[test]
    fn folders_create_and_auto_create_ancestors() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create_folder("client/spec-reviews/v2", None).unwrap();
        // Auto-created: "client", "client/spec-reviews", plus the leaf.
        let paths: Vec<String> = s.list_folders().into_iter().map(|f| f.path).collect();
        assert!(paths.contains(&"client".to_string()));
        assert!(paths.contains(&"client/spec-reviews".to_string()));
        assert!(paths.contains(&"client/spec-reviews/v2".to_string()));
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn folders_survive_reopen() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        {
            let mut s = AutomationStore::open(&p).unwrap();
            s.create_folder("empty/scaffold", None).unwrap();
        }
        let reopen = AutomationStore::open(&p).unwrap();
        assert_eq!(reopen.list_folders().len(), 2);
    }

    #[test]
    fn rename_folder_migrates_automations_and_subfolders() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create_folder("client/v1", None).unwrap();
        let mut a = auto("rev-1");
        a.folder = Some("client/v1".into());
        s.create(a).unwrap();
        s.rename_folder("client", "customer", None).unwrap();
        // Both the sub-folder and the automation pointed at the new path.
        assert!(s.list_folders().iter().any(|f| f.path == "customer"));
        assert!(s.list_folders().iter().any(|f| f.path == "customer/v1"));
        assert_eq!(
            s.get("rev-1").unwrap().folder.as_deref(),
            Some("customer/v1")
        );
    }

    #[test]
    fn delete_folder_keep_contents_reparents() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create_folder("a/b", None).unwrap();
        let mut au = auto("x");
        au.folder = Some("a/b".into());
        s.create(au).unwrap();
        s.delete_folder("a/b", true).unwrap();
        // Folder gone; automation reparented to "a".
        assert!(!s.list_folders().iter().any(|f| f.path == "a/b"));
        assert_eq!(s.get("x").unwrap().folder.as_deref(), Some("a"));
    }

    #[test]
    fn delete_folder_recursive_removes_everything() {
        let dir = tmpdir();
        let p = dir.join("automations.json");
        let mut s = AutomationStore::open(&p).unwrap();
        s.create_folder("doomed/sub", None).unwrap();
        let mut au = auto("y");
        au.folder = Some("doomed/sub".into());
        s.create(au).unwrap();
        let count = s.delete_folder("doomed", false).unwrap();
        assert_eq!(count, 1);
        assert!(s.get("y").is_none());
        assert_eq!(s.list_folders().len(), 0);
    }
}
