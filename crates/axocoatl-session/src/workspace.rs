//! Durable, user-named project directories that own Sessions.
//!
//! A Workspace is authorization and identity for one canonical directory. It
//! remains visible independently of whether it currently owns an open Session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axocoatl_core::{SecureDir, SecureEntryType};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("workspace not found: {0}")]
    NotFound(String),
    #[error("workspace directory does not exist or is not a directory: {0}")]
    BadPath(String),
    #[error("workspace name is empty")]
    EmptyName,
    #[error("more than one workspace owns canonical path: {0}")]
    DuplicatePath(String),
}

/// A durable product-level identity for one authorized project directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    /// Canonical absolute directory. Renaming the Workspace never changes it.
    pub canonical_path: PathBuf,
    pub created_at: u64,
    pub last_active: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn default_name(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn validated_name(name: Option<&str>, path: &Path) -> Result<String, WorkspaceError> {
    match name {
        Some(name) if name.trim().is_empty() => Err(WorkspaceError::EmptyName),
        Some(name) => Ok(name.trim().to_string()),
        None => Ok(default_name(path)),
    }
}

/// Persistent store of Workspaces — JSON files under `{data_dir}/workspaces/`.
pub struct WorkspaceStore {
    secure_dir: SecureDir,
    workspaces: HashMap<String, Workspace>,
    by_path: HashMap<PathBuf, String>,
    by_identity: HashMap<WorkspacePathIdentity, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WorkspacePathIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(not(unix))]
    Path(PathBuf),
}

fn existing_directory_identity(path: &Path) -> Option<WorkspacePathIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(WorkspacePathIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Some(WorkspacePathIdentity::Path(path.to_path_buf()))
    }
}

impl WorkspaceStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let dir = dir.into();
        let secure_dir = SecureDir::open_or_create_all(&dir)?;
        Ok(Self {
            secure_dir,
            workspaces: HashMap::new(),
            by_path: HashMap::new(),
            by_identity: HashMap::new(),
        })
    }

    pub fn new_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(&data_root, relative)
    }

    pub fn new_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, WorkspaceError> {
        let secure_dir = data_root.child(relative)?;
        Ok(Self {
            secure_dir,
            workspaces: HashMap::new(),
            by_path: HashMap::new(),
            by_identity: HashMap::new(),
        })
    }

    /// Load all valid Workspace records. Malformed files are ignored in the
    /// same way as legacy Session records, while duplicate canonical ownership
    /// is fatal because silently choosing one id would corrupt Session links.
    pub fn load_all(&mut self) -> Result<(), WorkspaceError> {
        let mut loaded = Vec::new();
        for entry in self.secure_dir.entries()? {
            if entry.file_type != SecureEntryType::File {
                continue;
            }
            let path = Path::new(&entry.name);
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(filename_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(bytes) = self.secure_dir.read(&entry.name) else {
                continue;
            };
            let Ok(workspace) = serde_json::from_slice::<Workspace>(&bytes) else {
                continue;
            };
            if workspace.id != filename_id
                || !crate::is_canonical_persisted_id(&workspace.id, "wsp-")
            {
                continue;
            }
            if !workspace.canonical_path.is_absolute() {
                return Err(WorkspaceError::BadPath(
                    workspace.canonical_path.display().to_string(),
                ));
            }
            loaded.push(workspace);
        }
        loaded.sort_by(|left, right| left.id.cmp(&right.id));
        for workspace in loaded {
            if let Some(existing) = self.by_path.get(&workspace.canonical_path) {
                if existing != &workspace.id {
                    return Err(WorkspaceError::DuplicatePath(
                        workspace.canonical_path.display().to_string(),
                    ));
                }
            }
            if let Some(identity) = existing_directory_identity(&workspace.canonical_path) {
                if let Some(existing) = self.by_identity.get(&identity) {
                    if existing != &workspace.id {
                        return Err(WorkspaceError::DuplicatePath(
                            workspace.canonical_path.display().to_string(),
                        ));
                    }
                }
                self.by_identity.insert(identity, workspace.id.clone());
            }
            self.by_path
                .insert(workspace.canonical_path.clone(), workspace.id.clone());
            self.workspaces.insert(workspace.id.clone(), workspace);
        }
        Ok(())
    }

    /// Register a directory explicitly. Canonical path is the unique key, so
    /// replaying Open Workspace returns the same durable identity. Supplying a
    /// name for an existing path deliberately updates only its display label.
    pub fn register(
        &mut self,
        path: impl AsRef<Path>,
        name: Option<&str>,
    ) -> Result<Workspace, WorkspaceError> {
        let raw = path.as_ref();
        let canonical_path = raw
            .canonicalize()
            .map_err(|_| WorkspaceError::BadPath(raw.display().to_string()))?;
        if !canonical_path.is_dir() {
            return Err(WorkspaceError::BadPath(
                canonical_path.display().to_string(),
            ));
        }
        let identity = existing_directory_identity(&canonical_path)
            .ok_or_else(|| WorkspaceError::BadPath(canonical_path.display().to_string()))?;
        // A legacy Workspace may have been loaded while its drive was absent,
        // so it has only a lexical key. Re-stat every retained path before the
        // fast return: two formerly unavailable aliases can later resolve to
        // one inode, and allowing both durable ids to survive would split Ways
        // locks and Session ownership until the next daemon restart.
        let mut matching_ids = self
            .workspaces
            .values()
            .filter(|workspace| {
                existing_directory_identity(&workspace.canonical_path).as_ref() == Some(&identity)
            })
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        if let Some(id) = self.by_identity.get(&identity) {
            matching_ids.push(id.clone());
        }
        if let Some(id) = self.by_path.get(&canonical_path) {
            matching_ids.push(id.clone());
        }
        matching_ids.sort();
        matching_ids.dedup();
        if matching_ids.len() > 1 {
            return Err(WorkspaceError::DuplicatePath(
                canonical_path.display().to_string(),
            ));
        }
        let existing = matching_ids.pop();
        if let Some(id) = existing {
            self.by_identity.insert(identity, id.clone());
            self.by_path.insert(canonical_path, id.clone());
            if let Some(name) = name {
                return self.rename(&id, name);
            }
            return self
                .workspaces
                .get(&id)
                .cloned()
                .ok_or(WorkspaceError::NotFound(id));
        }

        let now = now_secs();
        let workspace = Workspace {
            id: format!("wsp-{}", uuid::Uuid::new_v4()),
            name: validated_name(name, &canonical_path)?,
            canonical_path,
            created_at: now,
            last_active: now,
        };
        self.persist(&workspace)?;
        self.by_identity.insert(identity, workspace.id.clone());
        self.by_path
            .insert(workspace.canonical_path.clone(), workspace.id.clone());
        self.workspaces
            .insert(workspace.id.clone(), workspace.clone());
        Ok(workspace)
    }

    /// Find or create the Workspace for a legacy Session path. Missing paths
    /// are retained when they were already absolute: historical Sessions must
    /// survive even if a drive is temporarily unavailable at startup.
    pub fn ensure_for_migration(
        &mut self,
        session_path: &Path,
        session_created_at: u64,
        session_last_active: u64,
    ) -> Result<(Workspace, bool), WorkspaceError> {
        let canonical_path = match session_path.canonicalize() {
            Ok(path) if path.is_dir() => path,
            _ if session_path.is_absolute() => session_path.to_path_buf(),
            _ => return Err(WorkspaceError::BadPath(session_path.display().to_string())),
        };
        let existing = existing_directory_identity(&canonical_path)
            .and_then(|identity| self.by_identity.get(&identity).cloned())
            .or_else(|| self.by_path.get(&canonical_path).cloned());
        if let Some(id) = existing {
            let workspace = self
                .workspaces
                .get_mut(&id)
                .ok_or_else(|| WorkspaceError::NotFound(id.clone()))?;
            let original = workspace.clone();
            workspace.created_at = workspace.created_at.min(session_created_at);
            workspace.last_active = workspace.last_active.max(session_last_active);
            let snapshot = workspace.clone();
            if snapshot != original {
                self.persist(&snapshot)?;
            }
            return Ok((snapshot, false));
        }

        let workspace = Workspace {
            id: format!("wsp-{}", uuid::Uuid::new_v4()),
            name: default_name(&canonical_path),
            canonical_path,
            created_at: session_created_at,
            last_active: session_last_active,
        };
        self.persist(&workspace)?;
        if let Some(identity) = existing_directory_identity(&workspace.canonical_path) {
            self.by_identity.insert(identity, workspace.id.clone());
        }
        self.by_path
            .insert(workspace.canonical_path.clone(), workspace.id.clone());
        self.workspaces
            .insert(workspace.id.clone(), workspace.clone());
        Ok((workspace, true))
    }

    pub fn get(&self, id: &str) -> Option<Workspace> {
        self.workspaces.get(id).cloned()
    }

    pub fn get_by_path(&self, canonical_path: &Path) -> Option<Workspace> {
        existing_directory_identity(canonical_path)
            .and_then(|identity| self.by_identity.get(&identity))
            .or_else(|| self.by_path.get(canonical_path))
            .and_then(|id| self.workspaces.get(id))
            .cloned()
    }

    pub fn list(&self) -> Vec<Workspace> {
        let mut workspaces: Vec<_> = self.workspaces.values().cloned().collect();
        workspaces.sort_by(|left, right| {
            right
                .last_active
                .cmp(&left.last_active)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        workspaces
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<Workspace, WorkspaceError> {
        let name = validated_name(Some(name), Path::new(""))?;
        let workspace = self
            .workspaces
            .get_mut(id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        workspace.name = name;
        let snapshot = workspace.clone();
        self.persist(&snapshot)?;
        Ok(snapshot)
    }

    pub fn touch(&mut self, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self
            .workspaces
            .get_mut(id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        workspace.last_active = now_secs();
        let snapshot = workspace.clone();
        self.persist(&snapshot)?;
        Ok(snapshot)
    }

    pub fn len(&self) -> usize {
        self.workspaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    fn persist(&self, workspace: &Workspace) -> Result<(), WorkspaceError> {
        let bytes = serde_json::to_vec_pretty(workspace)?;
        self.secure_dir
            .atomic_write(format!("{}.json", workspace.id), &bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn registration_is_idempotent_by_canonical_path_and_rename_keeps_path() {
        let data = tempdir().unwrap();
        let project = tempdir().unwrap();
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();

        let first = store.register(project.path(), Some("First name")).unwrap();
        let second = store.register(project.path(), Some("Better name")).unwrap();
        let reopened_without_name = store.register(project.path(), None).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.name, "Better name");
        assert_eq!(reopened_without_name.name, "Better name");
        assert_eq!(
            second.canonical_path,
            project.path().canonicalize().unwrap()
        );
        assert_eq!(store.len(), 1);

        let mut reopened = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        reopened.load_all().unwrap();
        assert_eq!(reopened.get(&first.id).unwrap(), second);
    }

    #[test]
    fn migration_retains_temporarily_missing_absolute_session_path() {
        let data = tempdir().unwrap();
        let missing = data.path().join("detached-project");
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();

        let (workspace, created) = store
            .ensure_for_migration(&missing, 10, 20)
            .expect("historical workspace remains discoverable");

        assert!(created);
        assert_eq!(workspace.name, "detached-project");
        assert_eq!(workspace.canonical_path, missing);
        assert_eq!(workspace.created_at, 10);
        assert_eq!(workspace.last_active, 20);
    }

    #[cfg(unix)]
    #[test]
    fn live_store_rejects_legacy_missing_aliases_that_later_share_an_inode() {
        use std::os::unix::fs::symlink;

        let data = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let target = parent.path().join("project");
        let first_alias = parent.path().join("detached-a");
        let second_alias = parent.path().join("detached-b");
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();

        let (first, _) = store.ensure_for_migration(&first_alias, 10, 20).unwrap();
        let (second, _) = store.ensure_for_migration(&second_alias, 11, 21).unwrap();
        assert_ne!(first.id, second.id);

        std::fs::create_dir(&target).unwrap();
        symlink(&target, &first_alias).unwrap();
        symlink(&target, &second_alias).unwrap();

        assert!(matches!(
            store.register(&first_alias, None),
            Err(WorkspaceError::DuplicatePath(_))
        ));
        assert_eq!(store.len(), 2, "registration must not create a third id");
    }

    #[test]
    fn load_rejects_two_ids_for_one_canonical_path() {
        let data = tempdir().unwrap();
        let project = tempdir().unwrap();
        let store_dir = data.path().join("workspaces");
        std::fs::create_dir_all(&store_dir).unwrap();
        for id in [
            "wsp-00000000-0000-4000-8000-000000000001",
            "wsp-00000000-0000-4000-8000-000000000002",
        ] {
            let workspace = Workspace {
                id: id.to_string(),
                name: id.to_string(),
                canonical_path: project.path().canonicalize().unwrap(),
                created_at: 1,
                last_active: 1,
            };
            std::fs::write(
                store_dir.join(format!("{id}.json")),
                serde_json::to_vec_pretty(&workspace).unwrap(),
            )
            .unwrap();
        }

        let mut reopened = WorkspaceStore::new(&store_dir).unwrap();
        assert!(matches!(
            reopened.load_all(),
            Err(WorkspaceError::DuplicatePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_two_lexical_paths_for_one_directory_inode() {
        use std::os::unix::fs::symlink;

        let data = tempdir().unwrap();
        let project = tempdir().unwrap();
        let alias = data.path().join("project-alias");
        symlink(project.path(), &alias).unwrap();
        let store_dir = data.path().join("workspaces");
        std::fs::create_dir_all(&store_dir).unwrap();
        for (id, path) in [
            (
                "wsp-00000000-0000-4000-8000-000000000011",
                project.path().to_path_buf(),
            ),
            ("wsp-00000000-0000-4000-8000-000000000012", alias),
        ] {
            let workspace = Workspace {
                id: id.to_string(),
                name: id.to_string(),
                canonical_path: path,
                created_at: 1,
                last_active: 1,
            };
            std::fs::write(
                store_dir.join(format!("{id}.json")),
                serde_json::to_vec_pretty(&workspace).unwrap(),
            )
            .unwrap();
        }

        let mut reopened = WorkspaceStore::new(&store_dir).unwrap();
        assert!(matches!(
            reopened.load_all(),
            Err(WorkspaceError::DuplicatePath(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn case_and_unicode_aliases_reuse_one_workspace_on_default_apfs() {
        let data = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();

        let cased = parent.path().join("Project");
        std::fs::create_dir(&cased).unwrap();
        let lowercase = parent.path().join("project");
        if lowercase.is_dir() {
            let first = store.register(&cased, None).unwrap();
            let alias = store.register(&lowercase, None).unwrap();
            assert_eq!(first.id, alias.id);
        }

        let composed = parent.path().join("Caf\u{e9}");
        std::fs::create_dir(&composed).unwrap();
        let decomposed = parent.path().join("Cafe\u{301}");
        if decomposed.is_dir() {
            let first = store.register(&composed, None).unwrap();
            let alias = store.register(&decomposed, None).unwrap();
            assert_eq!(first.id, alias.id);
        }
    }

    #[test]
    fn load_rejects_embedded_workspace_id_that_does_not_match_filename() {
        let data = tempdir().unwrap();
        let project = tempdir().unwrap();
        let store_dir = data.path().join("workspaces");
        std::fs::create_dir_all(&store_dir).unwrap();
        let workspace = Workspace {
            id: "../outside".to_string(),
            name: "poisoned".to_string(),
            canonical_path: project.path().canonicalize().unwrap(),
            created_at: 1,
            last_active: 1,
        };
        std::fs::write(
            store_dir.join("safe.json"),
            serde_json::to_vec_pretty(&workspace).unwrap(),
        )
        .unwrap();
        let mut noncanonical = workspace;
        noncanonical.id = "not-canonical".to_string();
        std::fs::write(
            store_dir.join("not-canonical.json"),
            serde_json::to_vec_pretty(&noncanonical).unwrap(),
        )
        .unwrap();

        let mut reopened = WorkspaceStore::new(store_dir).unwrap();
        reopened.load_all().unwrap();
        assert!(reopened.is_empty());
    }
}
