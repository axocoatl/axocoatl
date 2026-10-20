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

/// Why a folder cannot simply be opened as a Workspace.
///
/// A Session mounts its Workspace read-write into the sandbox, so the folder
/// decides what the Session's Agents can read and change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceScopeRefusal {
    /// The root of the file system, the folder that holds the home folders,
    /// the home folder, or a folder that contains one of Axocoatl's own
    /// directories. It can still be opened, but only after the person
    /// confirms that this is the folder they mean.
    Broad(String),
    /// One of Axocoatl's own directories, or a folder inside one. It is never
    /// a Workspace: no mount inside the sandbox could hide it again.
    Protected(String),
}

impl WorkspaceScopeRefusal {
    pub fn message(&self) -> &str {
        match self {
            Self::Broad(message) | Self::Protected(message) => message,
        }
    }
}

/// One of Axocoatl's own directories, named for the person.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtectedRoot {
    label: &'static str,
    path: PathBuf,
}

/// The folders a Workspace must not be: the file system root, the home
/// folder and the folders above it, and Axocoatl's own directories (its data,
/// configuration, lease and control-socket directories) together with every
/// folder inside or above them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceScope {
    home: Option<PathBuf>,
    protected: Vec<ProtectedRoot>,
}

/// The canonical form of `path`, or the path itself when it cannot be
/// resolved (a missing directory still names the same place lexically).
fn resolved(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl WorkspaceScope {
    /// A scope for the person whose home folder is `home`.
    pub fn new(home: Option<&Path>) -> Self {
        Self {
            home: home
                .filter(|home| home.is_absolute())
                .map(resolved)
                .filter(|home| home.parent().is_some()),
            protected: Vec::new(),
        }
    }

    /// Also protect `path`, one of Axocoatl's own directories, which the
    /// person knows as `label` (for example "data directory").
    pub fn protect(mut self, label: &'static str, path: &Path) -> Self {
        let path = resolved(path);
        if path.is_absolute() && !self.protected.iter().any(|root| root.path == path) {
            self.protected.push(ProtectedRoot { label, path });
        }
        self
    }

    /// Whether `path` may be opened as a Workspace as it is. `path` should be
    /// canonical; it is resolved again here so an alias cannot slip past.
    pub fn check(&self, path: &Path) -> Result<(), WorkspaceScopeRefusal> {
        let path = resolved(path);
        let shown = path.display();
        for root in &self.protected {
            if path == root.path || path.starts_with(&root.path) {
                return Err(WorkspaceScopeRefusal::Protected(format!(
                    "{shown} is inside Axocoatl's {} ({}). Axocoatl never opens its own state as \
                     a Workspace; open a project folder instead.",
                    root.label,
                    root.path.display()
                )));
            }
        }
        if path.parent().is_none() {
            return Err(WorkspaceScopeRefusal::Broad(format!(
                "{shown} is the root of the file system. A Session there could read and change \
                 every file your account can. Open a project folder instead."
            )));
        }
        if let Some(home) = &self.home {
            if &path == home {
                return Err(WorkspaceScopeRefusal::Broad(format!(
                    "{shown} is your home folder. A Session there could read and change \
                     everything in it, including ~/.ssh, cloud credentials and shell startup \
                     files. Open a project folder instead."
                )));
            }
            if home.parent() == Some(path.as_path()) {
                return Err(WorkspaceScopeRefusal::Broad(format!(
                    "{shown} holds the home folders of this computer's users, yours included. \
                     Open a project folder instead."
                )));
            }
            if home.starts_with(&path) {
                return Err(WorkspaceScopeRefusal::Broad(format!(
                    "{shown} contains your home folder ({}). Open a project folder instead.",
                    home.display()
                )));
            }
        }
        if let Some(root) = self
            .protected
            .iter()
            .find(|root| root.path.starts_with(&path))
        {
            return Err(WorkspaceScopeRefusal::Broad(format!(
                "{shown} contains Axocoatl's {} ({}). Sessions hide that directory from their \
                 Agents, but everything else in {shown} is theirs to read and change. Open a \
                 project folder instead.",
                root.label,
                root.path.display()
            )));
        }
        Ok(())
    }
}

/// What [`WorkspaceStore::register_reporting_move`] registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorkspace {
    pub workspace: Workspace,
    /// The Workspace's previous path when its directory had moved to the
    /// registered path. Its Sessions keep the previous path, which is no
    /// longer the Workspace's.
    pub moved_from: Option<PathBuf>,
}

/// Persistent store of Workspaces — JSON files under `{data_dir}/workspaces/`.
pub struct WorkspaceStore {
    secure_dir: SecureDir,
    workspaces: HashMap<String, Workspace>,
    by_path: HashMap<PathBuf, String>,
    by_identity: HashMap<WorkspacePathIdentity, String>,
}

/// One directory, however it is named. On Unix the birth time, where the
/// file system records one, tells a moved directory (same inode, same birth
/// time) from a new directory that reuses a deleted one's inode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WorkspacePathIdentity {
    #[cfg(unix)]
    Unix {
        device: u64,
        inode: u64,
        born: Option<SystemTime>,
    },
    #[cfg(not(unix))]
    Path(PathBuf),
}

/// Whether `path` is a directory itself, not a symbolic link to one.
fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
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
            born: metadata.created().ok(),
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
    ///
    /// A Workspace whose directory was moved here keeps its id and moves with
    /// it: see [`Self::register_reporting_move`].
    pub fn register(
        &mut self,
        path: impl AsRef<Path>,
        name: Option<&str>,
    ) -> Result<Workspace, WorkspaceError> {
        self.register_reporting_move(path, name)
            .map(|registered| registered.workspace)
    }

    /// [`Self::register`], saying when the Workspace's directory had moved.
    ///
    /// A directory belongs to the Workspace whose retained path is that
    /// directory now. When no retained path is, the Workspace this directory
    /// was last seen as (by device and inode) owns it only if that
    /// Workspace's own path is no longer a real directory: the directory was
    /// moved here (`mv A B`), and the Workspace's path becomes this one,
    /// durably. When the old path is a real directory again (another
    /// directory made at A), this directory is a new Workspace, never the old
    /// one. A retained path that is now a symbolic link to this directory is
    /// likewise moved to this real path, so no Workspace is left on a link.
    pub fn register_reporting_move(
        &mut self,
        path: impl AsRef<Path>,
        name: Option<&str>,
    ) -> Result<RegisteredWorkspace, WorkspaceError> {
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
        matching_ids.sort();
        matching_ids.dedup();
        if matching_ids.len() > 1 {
            return Err(WorkspaceError::DuplicatePath(
                canonical_path.display().to_string(),
            ));
        }
        if let Some(id) = matching_ids.pop() {
            let retained = self
                .workspaces
                .get(&id)
                .map(|workspace| workspace.canonical_path.clone())
                .ok_or_else(|| WorkspaceError::NotFound(id.clone()))?;
            if retained != canonical_path && !is_real_directory(&retained) {
                // The retained path is a link to this directory now.
                return self.relocate(&id, canonical_path, identity, name);
            }
            self.by_identity.insert(identity, id.clone());
            self.by_path.insert(canonical_path, id.clone());
            let workspace = match name {
                Some(name) => self.rename(&id, name)?,
                None => self
                    .workspaces
                    .get(&id)
                    .cloned()
                    .ok_or(WorkspaceError::NotFound(id))?,
            };
            return Ok(RegisteredWorkspace {
                workspace,
                moved_from: None,
            });
        }
        // No retained path is this directory. The Workspace it was last seen
        // as moved here only if that Workspace's own path is gone; a real
        // directory there now is another directory, and this one is new.
        if let Some(id) = self.by_identity.get(&identity).cloned() {
            if let Some(retained) = self
                .workspaces
                .get(&id)
                .map(|workspace| workspace.canonical_path.clone())
            {
                if !is_real_directory(&retained) {
                    return self.relocate(&id, canonical_path, identity, name);
                }
            }
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
        Ok(RegisteredWorkspace {
            workspace,
            moved_from: None,
        })
    }

    /// Durably move Workspace `id` to `canonical_path`, its directory's new
    /// path. The old path and every alias of it stop naming this Workspace.
    fn relocate(
        &mut self,
        id: &str,
        canonical_path: PathBuf,
        identity: WorkspacePathIdentity,
        name: Option<&str>,
    ) -> Result<RegisteredWorkspace, WorkspaceError> {
        let mut workspace = self
            .workspaces
            .get(id)
            .cloned()
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        let moved_from = std::mem::replace(&mut workspace.canonical_path, canonical_path);
        if let Some(name) = name {
            workspace.name = validated_name(Some(name), &workspace.canonical_path)?;
        } else if workspace.name == default_name(&moved_from) {
            // A name nobody chose follows the folder's.
            workspace.name = default_name(&workspace.canonical_path);
        }
        self.persist(&workspace)?;
        self.by_path.retain(|_, owner| owner != id);
        self.by_identity.retain(|_, owner| owner != id);
        self.by_path
            .insert(workspace.canonical_path.clone(), id.to_string());
        self.by_identity.insert(identity, id.to_string());
        self.workspaces.insert(id.to_string(), workspace.clone());
        Ok(RegisteredWorkspace {
            workspace,
            moved_from: Some(moved_from),
        })
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

    /// Forget Workspace `id`: its record is deleted and its folder is no
    /// longer a Workspace. The folder itself is never touched. The caller
    /// decides whether the Workspace's Sessions allow this.
    pub fn remove(&mut self, id: &str) -> Result<Workspace, WorkspaceError> {
        let workspace = self
            .workspaces
            .get(id)
            .cloned()
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        match self.secure_dir.remove_file(format!("{id}.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.workspaces.remove(id);
        self.by_path.retain(|_, owner| owner != id);
        self.by_identity.retain(|_, owner| owner != id);
        Ok(workspace)
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

    /// `mv A B`, then Open B: the Workspace moves with its directory,
    /// durably, and A no longer names it.
    #[test]
    fn a_moved_directory_moves_its_workspace() {
        let data = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let parent = parent.path().canonicalize().unwrap();
        let (a, b) = (parent.join("a"), parent.join("b"));
        std::fs::create_dir(&a).unwrap();
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        let first = store.register(&a, None).unwrap();

        std::fs::rename(&a, &b).unwrap();
        let moved = store.register_reporting_move(&b, None).unwrap();
        assert_eq!(moved.workspace.id, first.id);
        assert_eq!(moved.workspace.canonical_path, b);
        assert_eq!(moved.moved_from.as_deref(), Some(a.as_path()));
        // The name nobody chose follows the folder.
        assert_eq!(moved.workspace.name, "b");
        // Registering B again is no move.
        let again = store.register_reporting_move(&b, None).unwrap();
        assert_eq!(again.workspace.id, first.id);
        assert_eq!(again.moved_from, None);

        // A new directory at A is a new Workspace.
        std::fs::create_dir(&a).unwrap();
        let other = store.register(&a, None).unwrap();
        assert_ne!(other.id, first.id);
        assert_eq!(other.canonical_path, a);
        assert_eq!(store.get(&first.id).unwrap().canonical_path, b);

        let mut reopened = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        reopened.load_all().unwrap();
        assert_eq!(reopened.get(&first.id).unwrap().canonical_path, b);
        assert_eq!(reopened.register(&b, None).unwrap().id, first.id);
        assert_eq!(reopened.register(&a, None).unwrap().id, other.id);
    }

    /// A chosen name stays when the directory moves.
    #[test]
    fn a_moved_workspace_keeps_a_chosen_name() {
        let data = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let (a, b) = (parent.path().join("a"), parent.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        let first = store.register(&a, Some("Shop")).unwrap();
        std::fs::rename(&a, &b).unwrap();
        let moved = store.register(&b, None).unwrap();
        assert_eq!(moved.id, first.id);
        assert_eq!(moved.name, "Shop");
    }

    /// `mv A B; mkdir A`, then Open B: the old Workspace keeps A, which is a
    /// directory again, and B is never given it.
    #[test]
    fn a_directory_moved_away_from_a_path_made_again_is_a_new_workspace() {
        let data = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let parent = parent.path().canonicalize().unwrap();
        let (a, b) = (parent.join("a"), parent.join("b"));
        std::fs::create_dir(&a).unwrap();
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        let first = store.register(&a, None).unwrap();

        std::fs::rename(&a, &b).unwrap();
        std::fs::create_dir(&a).unwrap();
        let moved = store.register_reporting_move(&b, None).unwrap();
        assert_ne!(moved.workspace.id, first.id);
        assert_eq!(moved.workspace.canonical_path, b);
        assert_eq!(moved.moved_from, None);
        assert_eq!(store.get(&first.id).unwrap().canonical_path, a);
        assert_eq!(store.register(&a, None).unwrap().id, first.id);
        assert_eq!(store.register(&b, None).unwrap().id, moved.workspace.id);
        assert_eq!(store.len(), 2);
    }

    /// `mv A B` and a file or a link left at A: the Workspace moves to B,
    /// the real directory, and never stays on the link.
    #[cfg(unix)]
    #[test]
    fn a_retained_path_replaced_by_a_file_or_a_link_moves_to_the_directory() {
        use std::os::unix::fs::symlink;

        for leave in ["file", "link to the moved directory", "link elsewhere"] {
            let data = tempdir().unwrap();
            let parent = tempdir().unwrap();
            let parent = parent.path().canonicalize().unwrap();
            let (a, b, elsewhere) = (parent.join("a"), parent.join("b"), parent.join("c"));
            std::fs::create_dir(&a).unwrap();
            std::fs::create_dir(&elsewhere).unwrap();
            let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
            let first = store.register(&a, None).unwrap();

            std::fs::rename(&a, &b).unwrap();
            match leave {
                "file" => std::fs::write(&a, b"not a directory").unwrap(),
                "link to the moved directory" => symlink(&b, &a).unwrap(),
                _ => symlink(&elsewhere, &a).unwrap(),
            }
            let moved = store.register_reporting_move(&b, None).unwrap();
            assert_eq!(moved.workspace.id, first.id, "{leave}");
            assert_eq!(moved.workspace.canonical_path, b, "{leave}");
            assert_eq!(moved.moved_from.as_deref(), Some(a.as_path()), "{leave}");
        }
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

    #[test]
    fn removing_a_workspace_forgets_it_durably_and_keeps_its_folder() {
        let data = tempdir().unwrap();
        let project = tempdir().unwrap();
        std::fs::write(project.path().join("keep.txt"), "kept").unwrap();
        let mut store = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        let workspace = store.register(project.path(), Some("Mine")).unwrap();

        let removed = store.remove(&workspace.id).unwrap();
        assert_eq!(removed, workspace);
        assert!(store.get(&workspace.id).is_none());
        assert!(store.get_by_path(&workspace.canonical_path).is_none());
        assert!(matches!(
            store.remove(&workspace.id),
            Err(WorkspaceError::NotFound(_))
        ));
        assert_eq!(
            std::fs::read_to_string(project.path().join("keep.txt")).unwrap(),
            "kept"
        );

        let mut reopened = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        reopened.load_all().unwrap();
        assert!(reopened.is_empty());
        // Opening the folder again makes a new Workspace.
        let again = reopened.register(project.path(), None).unwrap();
        assert_ne!(again.id, workspace.id);
    }

    #[test]
    fn scope_refuses_the_root_the_home_folder_and_the_folders_above_it() {
        let root = tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let users = root.join("Users");
        let home = users.join("alex");
        let project = home.join("src").join("app");
        std::fs::create_dir_all(&project).unwrap();
        let scope = WorkspaceScope::new(Some(&home));

        let broad = |path: &Path| match scope.check(path) {
            Err(WorkspaceScopeRefusal::Broad(message)) => message,
            other => panic!("{} should be broad, was {other:?}", path.display()),
        };
        assert!(broad(Path::new("/")).contains("root of the file system"));
        assert!(broad(&home).contains("is your home folder"));
        assert!(broad(&users).contains("home folders of this computer's users"));
        assert!(broad(&root).contains("contains your home folder"));
        assert_eq!(scope.check(&project), Ok(()));
        assert_eq!(scope.check(&home.join("src")), Ok(()));
    }

    #[test]
    fn scope_protects_axocoatl_directories_and_refuses_folders_that_contain_them() {
        let home = tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let support = home.join("Library").join("Application Support");
        let config = support.join("Axocoatl");
        let data = config.join("data");
        let ipc = home.join(".axocoatl").join("run");
        let project = home.join("project");
        for directory in [&data, &ipc, &project] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let scope = WorkspaceScope::new(Some(&home))
            .protect("data directory", &data)
            .protect("configuration directory", &config)
            .protect("control socket directory", &ipc);

        for inside in [&data, &data.join("sessions"), &config, &ipc] {
            match scope.check(inside) {
                Err(WorkspaceScopeRefusal::Protected(message)) => {
                    assert!(message.contains("never opens its own state"), "{message}")
                }
                other => panic!("{} should be protected, was {other:?}", inside.display()),
            }
        }
        match scope.check(&support) {
            Err(WorkspaceScopeRefusal::Broad(message)) => {
                assert!(message.contains("contains Axocoatl's"), "{message}");
            }
            other => panic!("Application Support should be broad, was {other:?}"),
        }
        assert!(matches!(
            scope.check(&home.join(".axocoatl")),
            Err(WorkspaceScopeRefusal::Broad(_))
        ));
        assert_eq!(scope.check(&project), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn scope_resolves_links_before_it_decides() {
        let home = tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let elsewhere = tempdir().unwrap();
        let link = elsewhere.path().join("home-link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let scope = WorkspaceScope::new(Some(&home));
        assert!(matches!(
            scope.check(&link),
            Err(WorkspaceScopeRefusal::Broad(_))
        ));
    }
}
