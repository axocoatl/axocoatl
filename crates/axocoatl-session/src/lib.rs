//! Directory-scoped working **sessions** for Axocoatl.
//!
//! A session is the third run mode alongside chat and workflows: the user
//! picks a working directory, and either a single agent or the full agent
//! lattice builds in it. A session bundles a working directory, a persistent
//! conversation, and a chosen agent/lattice. Sessions persist as JSON and
//! survive daemon restarts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod devcontainer;
pub use devcontainer::{DevContainer, DevContainerError};

use serde::{Deserialize, Serialize};

/// Errors from session management.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("working directory does not exist or is not a directory: {0}")]
    BadWorkingDir(String),
}

/// Who works in a session — the per-session choice of single agent vs lattice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionMode {
    /// A single capable agent builds in the directory.
    SingleAgent { agent_id: String },
    /// The full agent lattice coordinates in the directory.
    Lattice {
        /// Workflow to run; `None` = the default stigmergic lattice cascade.
        #[serde(default)]
        workflow_id: Option<String>,
    },
    /// A user-picked subset of agents that runs as a lattice. Edges come from
    /// each agent's `depends_on` config — Custom is "Lattice mode, but only
    /// these agents". Lets a developer compose ad-hoc workflows in the UI
    /// without editing YAML.
    Custom { agents: Vec<String> },
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Open and recently used.
    Active,
    /// Open but not recently used.
    Idle,
    /// Explicitly closed by the user.
    Closed,
}

/// A directory-scoped working session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    /// The directory agents work in — canonical, absolute.
    pub working_dir: PathBuf,
    pub mode: SessionMode,
    pub status: SessionStatus,
    /// Ids of skills this session's agents may fire as tools (the allowlist).
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Container ports to publish on the host so the Browser pane (and any
    /// other tool) can reach dev servers inside the sandbox. `1:1` mapping.
    /// Sensible defaults are filled in by `Session::new` if empty.
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    /// OCI image for the session sandbox. `None` falls back to the
    /// `axocoatl-isolation` default (alpine). Populated from the user's
    /// pick in the modal, or auto-detected from `.devcontainer/devcontainer.json`.
    #[serde(default)]
    pub image: Option<String>,
    /// Shell commands run once after the sandbox container first boots —
    /// typically `pip install`, `npm ci`, etc. Sourced from
    /// `devcontainer.json`'s `postCreateCommand` when present.
    #[serde(default)]
    pub post_create_commands: Vec<String>,
    /// Unix-seconds timestamps.
    pub created_at: u64,
    pub last_active: u64,
}

/// Default dev-server ports we publish unless the user provides their own.
/// Picked to cover the common stacks: Node/Next (3000), Vite (5173),
/// Python http/Flask/Django (5000/8000), Jupyter (8888), plus 8765 as an
/// off-the-beaten-path fallback when the usual dev ports are already in use
/// on the host (e.g. the dog-runner demo's `serve.py`). We skip 8080 —
/// that's the axocoatl daemon's own port on the host.
pub const DEFAULT_EXPOSED_PORTS: &[u16] = &[3000, 5000, 5173, 8000, 8765, 8888];

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Session {
    fn new(
        name: String,
        working_dir: PathBuf,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
        post_create_commands: Vec<String>,
    ) -> Self {
        let now = now_secs();
        let ports = if exposed_ports.is_empty() {
            DEFAULT_EXPOSED_PORTS.to_vec()
        } else {
            exposed_ports
        };
        Self {
            id: format!("ses-{}", uuid::Uuid::new_v4()),
            name,
            working_dir,
            mode,
            status: SessionStatus::Active,
            enabled_skills,
            exposed_ports: ports,
            image,
            post_create_commands,
            created_at: now,
            last_active: now,
        }
    }
}

/// Persistent store of sessions — JSON files under `{data_dir}/sessions/`.
pub struct SessionStore {
    dir: PathBuf,
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    /// Open the store rooted at `dir` (created if absent). Call [`load_all`]
    /// to read existing sessions back in.
    ///
    /// [`load_all`]: SessionStore::load_all
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            sessions: HashMap::new(),
        })
    }

    /// Load every persisted session from disk. A malformed file is skipped
    /// (logged by the caller), never fatal.
    pub fn load_all(&mut self) -> Result<(), SessionError> {
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(mut session) = serde_json::from_slice::<Session>(&bytes) {
                    // Sessions persisted before exposed_ports existed have an
                    // empty list — backfill the default set so they can run
                    // dev servers reachable from the Browser pane.
                    if session.exposed_ports.is_empty() {
                        session.exposed_ports = DEFAULT_EXPOSED_PORTS.to_vec();
                    }
                    self.sessions.insert(session.id.clone(), session);
                }
            }
        }
        Ok(())
    }

    /// Create a new session on `working_dir`. The directory must already
    /// exist; the stored path is canonicalised (absolute, symlinks resolved).
    pub fn create(
        &mut self,
        name: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
    ) -> Result<Session, SessionError> {
        let raw = working_dir.into();
        let canon = raw
            .canonicalize()
            .map_err(|_| SessionError::BadWorkingDir(raw.display().to_string()))?;
        if !canon.is_dir() {
            return Err(SessionError::BadWorkingDir(canon.display().to_string()));
        }
        // If the project ships a devcontainer.json, let it shape the session.
        // The user's explicit `image` from the UI still wins — devcontainer
        // is the *default*, not a lock. Same for ports: we merge.
        let (mut final_image, mut final_ports, mut post_create) =
            (image, exposed_ports, Vec::<String>::new());
        if let Ok(Some((_path, dc))) = DevContainer::load(&canon) {
            if final_image.is_none() {
                final_image = dc.image.clone();
            }
            let fwd = dc.forwarded_ports();
            for p in fwd {
                if !final_ports.contains(&p) {
                    final_ports.push(p);
                }
            }
            post_create = dc.post_create_scripts();
        }
        let session = Session::new(
            name.into(),
            canon,
            mode,
            enabled_skills,
            final_ports,
            final_image,
            post_create,
        );
        self.persist(&session)?;
        self.sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    /// Fetch a session by id.
    pub fn get(&self, id: &str) -> Option<Session> {
        self.sessions.get(id).cloned()
    }

    /// All sessions, newest first.
    pub fn list(&self) -> Vec<Session> {
        let mut v: Vec<Session> = self.sessions.values().cloned().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.created_at));
        v
    }

    /// Mark a session active and bump its `last_active` timestamp.
    pub fn touch(&mut self, id: &str) -> Result<(), SessionError> {
        let s = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        s.last_active = now_secs();
        s.status = SessionStatus::Active;
        let snapshot = s.clone();
        self.persist(&snapshot)
    }

    /// Mark a session closed (kept on disk for history).
    pub fn close(&mut self, id: &str) -> Result<(), SessionError> {
        let s = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        s.status = SessionStatus::Closed;
        let snapshot = s.clone();
        self.persist(&snapshot)
    }

    /// Rename a session in place — same id, same working_dir, new display name.
    pub fn rename(
        &mut self,
        id: &str,
        new_name: impl Into<String>,
    ) -> Result<Session, SessionError> {
        let name = new_name.into();
        let name = name.trim();
        if name.is_empty() {
            return Err(SessionError::BadWorkingDir("name is empty".to_string()));
        }
        let s = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        s.name = name.to_string();
        let snapshot = s.clone();
        self.persist(&snapshot)?;
        Ok(snapshot)
    }

    /// Delete a session entirely (from memory and disk).
    pub fn remove(&mut self, id: &str) -> Result<(), SessionError> {
        if self.sessions.remove(id).is_none() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let path = self.dir.join(format!("{id}.json"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Number of sessions held.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True iff there are no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Atomically write one session to `{dir}/{id}.json` (temp + rename).
    fn persist(&self, session: &Session) -> Result<(), SessionError> {
        let path = self.dir.join(format!("{}.json", session.id));
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(session)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_list_and_persistence_roundtrip() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let id;
        {
            let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
            let s = store
                .create(
                    "build the CLI",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                )
                .unwrap();
            id = s.id.clone();
            assert_eq!(store.len(), 1);
            // working_dir is canonicalised + absolute.
            assert!(s.working_dir.is_absolute());
        }
        // Reopen — the session is loaded back.
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        store.load_all().unwrap();
        assert_eq!(store.len(), 1);
        let reloaded = store.get(&id).unwrap();
        assert_eq!(reloaded.name, "build the CLI");
        assert_eq!(reloaded.status, SessionStatus::Active);
    }

    #[test]
    fn rejects_nonexistent_working_dir() {
        let data = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let err = store.create(
            "bad",
            "/no/such/axocoatl/dir",
            SessionMode::Lattice { workflow_id: None },
            Vec::new(),
            Vec::new(),
            None,
        );
        assert!(matches!(err, Err(SessionError::BadWorkingDir(_))));
    }

    #[test]
    fn close_and_remove() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let s = store
            .create(
                "x",
                work.path(),
                SessionMode::Lattice { workflow_id: None },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        store.close(&s.id).unwrap();
        assert_eq!(store.get(&s.id).unwrap().status, SessionStatus::Closed);
        store.remove(&s.id).unwrap();
        assert!(store.is_empty());
        assert!(matches!(
            store.remove(&s.id),
            Err(SessionError::NotFound(_))
        ));
    }
}
