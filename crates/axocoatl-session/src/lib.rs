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
pub mod session_attachment;
pub mod turn_ledger;
pub mod workspace;
pub use devcontainer::{DevContainer, DevContainerError};
pub use session_attachment::{
    CreateSessionAttachmentRef, SessionAttachmentConsumedState, SessionAttachmentError,
    SessionAttachmentExtractionSnapshot, SessionAttachmentExtractionStatus, SessionAttachmentRef,
    SessionAttachmentStore,
};
pub use turn_ledger::{
    AppendTurnOutput, BeginSessionTurn, RecordTurnExecution, SessionTranscriptMessage,
    SessionTranscriptRole, SessionTurn, SessionTurnAgentOutput, SessionTurnContextReference,
    SessionTurnError, SessionTurnExecutionEvent, SessionTurnLifecycle, SessionTurnSearchHit,
    SessionTurnStore, TransitionSessionTurn, TurnContextScope, TurnSearchField,
};
pub use workspace::{Workspace, WorkspaceError, WorkspaceStore};

use axocoatl_core::{SecureDir, SecureEntryType};
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
    #[error("session has no workspace identity")]
    MissingWorkspace,
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("devcontainer error: {0}")]
    DevContainer(#[from] DevContainerError),
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

/// Whether the Session's execution environment is safe to present as usable.
///
/// This is deliberately separate from [`SessionStatus`]: `Active` describes
/// the Session's lifecycle, while this state says whether its requested image
/// and explicitly approved project setup have actually succeeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEnvironmentState {
    /// No sandbox has validated this persisted runtime configuration yet.
    #[default]
    Unprepared,
    /// A setup command was discovered or entered but has not been approved.
    AwaitingApproval,
    /// The daemon is starting the runtime and applying approved setup.
    Preparing,
    /// Runtime creation and every approved setup command completed.
    Ready,
    /// Preparation failed. `SessionEnvironment::error` contains the action.
    Failed,
}

/// Durable evidence from one explicitly approved setup command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetupResult {
    pub command: String,
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub completed_at: u64,
}

/// Durable cleanup authority for the exact runtime that last made this
/// Session Ready. The control-plane fingerprint contains no credential bytes;
/// it prevents a later config pointing at a different endpoint/account from
/// accepting a misleading 404 while the original remote VM remains alive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRuntimeIdentity {
    pub backend: String,
    pub id: String,
    /// Exact working root inside a remote runtime. This is distinct from the
    /// host Session path and must survive restart for safe reattachment.
    #[serde(default)]
    pub remote_root: Option<String>,
    #[serde(default)]
    pub control_plane: Option<String>,
    /// Exact data-plane authority returned by the provider for this runtime.
    /// Reattachment must not substitute a later daemon-global domain.
    #[serde(default)]
    pub data_plane_domain: Option<String>,
    #[serde(default)]
    pub authority_fingerprint: Option<String>,
    /// Exact random provider metadata token installed by this Session's
    /// preparation generation. Persisted E2B connect/pause/delete operations
    /// must prove the remote sandbox still carries this token and that its
    /// structured prefix names the current Session id and generation. Legacy
    /// records without this proof remain reviewable but cannot authorize a
    /// remote lifecycle action.
    #[serde(default)]
    pub ownership_token: Option<String>,
    /// True only after the exact backend reported deletion, or after the user
    /// explicitly confirmed manual cleanup. This makes a retained id a durable
    /// tombstone instead of forcing credentials to remain unchanged forever.
    #[serde(default)]
    pub cleanup_confirmed: bool,
}

/// Durable ownership marker written before a remote sandbox create request.
///
/// The provider metadata token closes the interval in which the control plane
/// may commit a sandbox but the caller loses the response before learning its
/// opaque id. Until this marker is reconciled, Axocoatl must discover by the
/// exact token rather than issue another create request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRuntimeCreationAttempt {
    pub backend: String,
    pub token: String,
    pub remote_root: String,
    pub control_plane: String,
    pub data_plane_domain: String,
    pub authority_fingerprint: String,
    /// Exact provider ids discovered by the metadata token and fsynced before
    /// any DELETE. This turns a crash after DELETE into an idempotent 404 retry
    /// instead of an unrecoverable token-with-zero-matches state.
    #[serde(default)]
    pub discovered_ids: Vec<String>,
}

/// Persisted environment contract for a Session.
///
/// The command is retained even while awaiting approval so the app can make a
/// concrete recommendation (for example `npm ci`) without executing repository
/// content. Once approved, the exact same command is used for the primary
/// Session and each isolated Way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnvironment {
    /// Monotonic plan generation used to make cancelled preparation cleanup
    /// unable to tear down a newer rebuild of the same deterministic name.
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub state: SessionEnvironmentState,
    #[serde(default)]
    pub effective_image: Option<String>,
    /// Backend-owned cleanup identity retained as a 404-safe tombstone after
    /// Close. The next durable preparation transition replaces it only after
    /// the exact prior runtime has reported successful deletion.
    #[serde(default)]
    pub runtime: Option<SessionRuntimeIdentity>,
    /// Pre-id remote ownership marker. A persisted runtime identity supersedes
    /// this marker only after the exact provider id is durably recorded.
    #[serde(default)]
    pub runtime_creation: Option<SessionRuntimeCreationAttempt>,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub setup_approved: bool,
    /// Whether a person/operator has made an explicit decision for this plan.
    /// `false` is the migration value for legacy Sessions; `true` with no
    /// command is a durable decision that this repository needs no setup.
    #[serde(default)]
    pub setup_reviewed: bool,
    #[serde(default)]
    pub setup_results: Vec<SessionSetupResult>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub prepared_at: Option<u64>,
}

/// Cleanup authority recoverable from a canonical Session file even when an
/// unrelated field (for example `mode`) no longer deserializes as a complete
/// [`Session`]. Bootstrap uses this envelope only for non-executing lifecycle
/// reconciliation; it never makes the malformed record visible as product
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRuntimeRecoveryRecord {
    pub id: String,
    /// Present only when every cleanup-relevant field decoded with its exact
    /// released type and the embedded id matched the canonical filename.
    pub environment: Option<SessionRuntimeRecoveryEnvironment>,
}

/// The minimal strongly-typed environment fields needed to pause or remove an
/// already-created runtime. Irrelevant malformed UI/config fields cannot hide
/// this authority, while a malformed cleanup identity fails closed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SessionRuntimeRecoveryEnvironment {
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub state: SessionEnvironmentState,
    #[serde(default)]
    pub effective_image: Option<String>,
    #[serde(default)]
    pub runtime: Option<SessionRuntimeIdentity>,
    #[serde(default)]
    pub runtime_creation: Option<SessionRuntimeCreationAttempt>,
}

#[derive(Deserialize)]
struct SessionRuntimeRecoveryEnvelope {
    id: String,
    #[serde(default)]
    environment: SessionRuntimeRecoveryEnvironment,
}

impl Default for SessionEnvironment {
    fn default() -> Self {
        Self {
            generation: 0,
            state: SessionEnvironmentState::Unprepared,
            effective_image: None,
            runtime: None,
            runtime_creation: None,
            setup_command: None,
            setup_approved: false,
            setup_reviewed: false,
            setup_results: Vec::new(),
            error: None,
            prepared_at: None,
        }
    }
}

impl SessionEnvironment {
    fn planned(setup_command: Option<String>, setup_approved: bool, setup_reviewed: bool) -> Self {
        let setup_command = normalize_command(setup_command);
        let state = if !setup_reviewed || (setup_command.is_some() && !setup_approved) {
            SessionEnvironmentState::AwaitingApproval
        } else {
            SessionEnvironmentState::Unprepared
        };
        Self {
            generation: 1,
            state,
            setup_command,
            setup_approved,
            setup_reviewed,
            ..Self::default()
        }
    }
}

/// A directory-scoped working session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    /// Durable owner. Missing only while a legacy record is being migrated.
    #[serde(default)]
    pub workspace_id: String,
    /// The directory agents work in — canonical, absolute.
    pub working_dir: PathBuf,
    pub mode: SessionMode,
    pub status: SessionStatus,
    /// Ids of skills this session's agents may fire as tools (the allowlist).
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Logical container ports reachable through this Session's Preview.
    /// Local isolation assigns independent loopback host ports at runtime, so
    /// sibling Sessions may use the same app port without colliding. Sensible
    /// defaults are filled in by `Session::new` if empty.
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
    /// The project's own command for deciding whether a change is good —
    /// tests, a build, a typecheck.
    ///
    /// A property of the repository, not of a run. It is the arbiter that rules
    /// attempts out, so retyping it per comparison invites a typo deciding which
    /// attempt survives, and hardcoding a default would quietly make this a
    /// JavaScript tool. Detected on create from what the project actually has;
    /// `None` means we could not tell and the user must say.
    #[serde(default)]
    pub check_command: Option<String>,
    /// Truthful, durable preparation state for this Session's sandbox.
    #[serde(default)]
    pub environment: SessionEnvironment,
    /// Unix-seconds timestamps.
    pub created_at: u64,
    pub last_active: u64,
}

/// Backfill only for records written before `exposed_ports` existed. A present
/// empty array is an explicit choice to expose no Preview ports and must stay
/// empty.
const LEGACY_DEFAULT_EXPOSED_PORTS: &[u16] = &[3000, 5000, 5173, 8000, 8765, 8888];

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn is_canonical_persisted_id(id: &str, prefix: &str) -> bool {
    let Some(uuid_text) = id.strip_prefix(prefix) else {
        return false;
    };
    uuid::Uuid::parse_str(uuid_text).is_ok_and(|uuid| uuid.hyphenated().to_string() == uuid_text)
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        workspace_id: String,
        working_dir: PathBuf,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
        post_create_commands: Vec<String>,
        check_command: Option<String>,
        setup_command: Option<String>,
        setup_approved: bool,
        setup_reviewed: bool,
    ) -> Self {
        let now = now_secs();
        Self {
            id: format!("ses-{}", uuid::Uuid::new_v4()),
            name,
            workspace_id,
            working_dir,
            mode,
            status: SessionStatus::Active,
            enabled_skills,
            exposed_ports,
            image,
            post_create_commands,
            check_command,
            environment: SessionEnvironment::planned(setup_command, setup_approved, setup_reviewed),
            created_at: now,
            last_active: now,
        }
    }
}

fn normalize_command(command: Option<String>) -> Option<String> {
    command
        .map(|command| command.trim().to_string())
        .filter(|command| !command.is_empty())
}

/// Suggest a reproducible dependency setup without executing repository code.
///
/// A root `package-lock.json` is the strongest npm signal: `npm ci` consumes
/// the committed lockfile exactly and is therefore safer and more reproducible
/// than guessing at `npm install`. The result is a proposal only; the Session
/// remains `awaiting_approval` until the user explicitly accepts it.
pub fn detect_setup_command(dir: &std::path::Path) -> Option<String> {
    let dir = SecureDir::open(dir).ok()?;
    detect_setup_command_in(&dir).ok().flatten()
}

fn detect_setup_command_in(dir: &SecureDir) -> std::io::Result<Option<String>> {
    Ok(dir
        .is_file("package-lock.json")?
        .then(|| "npm ci".to_string()))
}

/// Guess the project's check command from what it actually contains.
///
/// Ordered so the most specific signal wins: a script the project defined for
/// itself beats a language-wide convention, because someone who wrote
/// `"check"` in their package.json has already answered this question. Returns
/// `None` rather than a guess when nothing matches — an arbitrary default here
/// would rule attempts out using a command the project never runs, which is
/// worse than asking.
pub fn detect_check_command(dir: &std::path::Path) -> Option<String> {
    let dir = SecureDir::open(dir).ok()?;
    detect_check_command_in(&dir).ok().flatten()
}

const MAX_PACKAGE_JSON_BYTES: usize = 1024 * 1024;

fn detect_check_command_in(dir: &SecureDir) -> std::io::Result<Option<String>> {
    if dir.is_file("package.json")? {
        let bytes = dir.read_limited("package.json", MAX_PACKAGE_JSON_BYTES)?;
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let scripts = v.get("scripts").and_then(|scripts| scripts.as_object());
            if let Some(scripts) = scripts {
                for name in ["check", "test", "ci", "build"] {
                    if scripts.contains_key(name) {
                        return Ok(Some(format!("npm run {name}")));
                    }
                }
            }
        }
    }
    if dir.is_file("Cargo.toml")? {
        return Ok(Some("cargo test".to_string()));
    }
    if dir.is_file("go.mod")? {
        return Ok(Some("go test ./...".to_string()));
    }
    if dir.is_file("pyproject.toml")? || dir.is_file("setup.py")? || dir.is_file("pytest.ini")? {
        return Ok(Some("pytest".to_string()));
    }
    if dir.is_file("Makefile")? || dir.is_file("makefile")? {
        return Ok(Some("make test".to_string()));
    }
    Ok(None)
}

/// Persistent store of sessions — JSON files under `{data_dir}/sessions/`.
pub struct SessionStore {
    secure_dir: SecureDir,
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    /// Open the store rooted at `dir` (created if absent). Call [`load_all`]
    /// to read existing sessions back in.
    ///
    /// [`load_all`]: SessionStore::load_all
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let dir = dir.into();
        let secure_dir = SecureDir::open_or_create_all(&dir)?;
        Ok(Self {
            secure_dir,
            sessions: HashMap::new(),
        })
    }

    /// Open the Session store relative to an explicit control-plane anchor.
    pub fn new_in(
        data_root: impl AsRef<std::path::Path>,
        relative: impl AsRef<std::path::Path>,
    ) -> Result<Self, SessionError> {
        let data_root = SecureDir::open(data_root)?;
        Self::new_in_secure(&data_root, relative)
    }

    pub fn new_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<std::path::Path>,
    ) -> Result<Self, SessionError> {
        let secure_dir = data_root.child(relative)?;
        Ok(Self {
            secure_dir,
            sessions: HashMap::new(),
        })
    }

    /// Load every persisted session from disk. A malformed file is skipped
    /// (logged by the caller), never fatal.
    pub fn load_all(&mut self) -> Result<(), SessionError> {
        for entry in self.secure_dir.entries()? {
            if entry.file_type != SecureEntryType::File {
                continue;
            }
            let path = std::path::Path::new(&entry.name);
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(filename_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if let Ok(bytes) = self.secure_dir.read(&entry.name) {
                let parsed = serde_json::from_slice::<serde_json::Value>(&bytes);
                if let Ok(value) = parsed {
                    let exposed_ports_missing = value.get("exposed_ports").is_none();
                    let Ok(mut session) = serde_json::from_value::<Session>(value) else {
                        continue;
                    };
                    // The filename is the store's containment boundary. Never
                    // let a poisoned embedded id become a later persist/delete
                    // target, including during the legacy repairs below.
                    if session.id != filename_id || !is_canonical_persisted_id(&session.id, "ses-")
                    {
                        continue;
                    }
                    if exposed_ports_missing {
                        session.exposed_ports = LEGACY_DEFAULT_EXPOSED_PORTS.to_vec();
                        self.persist(&session)?;
                    }
                    // Sessions created before environment readiness existed
                    // must not claim Ready with the empty dependency volume a
                    // Node repository receives. Persist a concrete proposal,
                    // but never infer consent from the lockfile.
                    if session.environment.state == SessionEnvironmentState::Unprepared
                        && !session.environment.setup_reviewed
                    {
                        let suggestion = if session.post_create_commands.is_empty() {
                            detect_setup_command(&session.working_dir)
                        } else {
                            Some(session.post_create_commands.join(" && "))
                        };
                        // Legacy records still require a visible decision even
                        // when there is no repository setup command to run.
                        // Otherwise the first Files/Terminal/Agent request
                        // would silently promote an unreviewed runtime to Ready.
                        session.environment = SessionEnvironment::planned(suggestion, false, false);
                        self.persist(&session)?;
                    }
                    // A process cannot resume an in-flight preparation. Keep
                    // the exact approved plan, but make the interruption
                    // actionable instead of leaving a permanently spinning
                    // Session after restart.
                    if session.environment.state == SessionEnvironmentState::Preparing {
                        session.environment.state = SessionEnvironmentState::Failed;
                        session.environment.error = Some(
                            "environment preparation was interrupted; rebuild the environment"
                                .to_string(),
                        );
                        session.environment.prepared_at = Some(now_secs());
                        self.persist(&session)?;
                    }
                    self.sessions.insert(session.id.clone(), session);
                }
            }
        }
        Ok(())
    }

    /// Decode cleanup authority from canonical records that cannot be loaded
    /// as complete Sessions. The filename/embedded id pair remains the storage
    /// boundary, and fully valid records are deliberately excluded so callers
    /// cannot perform the same lifecycle action twice.
    pub fn unloaded_runtime_recovery_records(
        &self,
    ) -> Result<Vec<SessionRuntimeRecoveryRecord>, SessionError> {
        let mut records = Vec::new();
        for entry in self.secure_dir.entries()? {
            if entry.file_type != SecureEntryType::File {
                continue;
            }
            let path = std::path::Path::new(&entry.name);
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(filename_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !is_canonical_persisted_id(filename_id, "ses-") {
                continue;
            }
            let value = self
                .secure_dir
                .read(&entry.name)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let fully_loadable = value.as_ref().is_some_and(|value| {
                serde_json::from_value::<Session>(value.clone()).is_ok_and(|session| {
                    session.id == filename_id && is_canonical_persisted_id(&session.id, "ses-")
                })
            });
            if fully_loadable {
                continue;
            }
            let environment = value
                .and_then(|value| {
                    serde_json::from_value::<SessionRuntimeRecoveryEnvelope>(value).ok()
                })
                .filter(|envelope| envelope.id == filename_id)
                .map(|envelope| envelope.environment);
            records.push(SessionRuntimeRecoveryRecord {
                id: filename_id.to_string(),
                environment,
            });
        }
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    /// Move one successfully reconciled malformed record out of the active
    /// Session namespace without deleting operator evidence. The move is
    /// fd-relative and fsynced by [`SecureDir::rename_leaf_to`].
    pub fn quarantine_unloaded_runtime_recovery(&self, id: &str) -> Result<(), SessionError> {
        if !is_canonical_persisted_id(id, "ses-")
            || !self
                .unloaded_runtime_recovery_records()?
                .iter()
                .any(|record| record.id == id)
        {
            return Err(SessionError::BadWorkingDir(format!(
                "Session '{id}' is not a canonical unloaded recovery record"
            )));
        }
        let quarantine = self.secure_dir.child("quarantine-v1")?;
        let target = format!("{id}.{}.json", uuid::Uuid::new_v4());
        self.secure_dir.rename_leaf_to(
            std::path::Path::new(&format!("{id}.json")),
            &quarantine,
            std::path::Path::new(&target),
        )?;
        Ok(())
    }

    /// Hide one invalid loaded record without deleting operator data. Used by
    /// bootstrap before any repair/execution path can act on a Session whose
    /// mode no longer matches the active configuration.
    pub fn quarantine_loaded(&mut self, id: &str) -> Option<Session> {
        self.sessions.remove(id)
    }

    /// Create a new session on `working_dir`. The directory must already
    /// exist; the stored path is canonicalised (absolute, symlinks resolved).
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        name: impl Into<String>,
        workspace_id: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
    ) -> Result<Session, SessionError> {
        self.create_with_environment(
            name,
            workspace_id,
            working_dir,
            mode,
            enabled_skills,
            exposed_ports,
            image,
            None,
            false,
            false,
        )
    }

    /// Create a Session with an explicit environment/setup decision.
    ///
    /// `setup_command` is persisted as a proposal. It is executable only when
    /// `setup_approved` is true; callers cannot accidentally make a detected
    /// repository command run merely by including it in the record.
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_environment(
        &mut self,
        name: impl Into<String>,
        workspace_id: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
        setup_command: Option<String>,
        setup_approved: bool,
        setup_reviewed: bool,
    ) -> Result<Session, SessionError> {
        let workspace_id = workspace_id.into();
        if workspace_id.trim().is_empty() {
            return Err(SessionError::MissingWorkspace);
        }
        let raw = working_dir.into();
        let canon = raw
            .canonicalize()
            .map_err(|_| SessionError::BadWorkingDir(raw.display().to_string()))?;
        if !canon.is_dir() {
            return Err(SessionError::BadWorkingDir(canon.display().to_string()));
        }
        let workspace = SecureDir::open(&canon)?;
        // If the project ships a devcontainer.json, let it shape the session.
        // The user's explicit `image` from the UI still wins — devcontainer
        // is the *default*, not a lock. Same for ports: we merge.
        let (mut final_image, mut final_ports, mut post_create) =
            (image, exposed_ports, Vec::<String>::new());
        if let Some((_path, dc)) = DevContainer::load_in(&workspace)? {
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
        // Detection produces a proposal only. It never changes approval: a
        // CLI/compatibility caller that omitted setup receives an
        // AwaitingApproval Session instead of executing repository code.
        let explicit_setup_command = normalize_command(setup_command);
        let (setup_command, setup_approved, setup_reviewed) = match explicit_setup_command {
            Some(command) => (Some(command), setup_approved, setup_reviewed),
            None if setup_reviewed => (None, false, true),
            None => {
                let proposal = if post_create.is_empty() {
                    detect_setup_command_in(&workspace)?
                } else {
                    Some(post_create.join(" && "))
                };
                // Approval is bound to exact command bytes supplied in this
                // request. A bare `setup_approved: true` can never authorize a
                // command discovered later from repository content.
                match proposal {
                    Some(command) => (Some(command), false, false),
                    None => (None, false, setup_reviewed),
                }
            }
        };
        let check_command = detect_check_command_in(&workspace)?;
        workspace.verify_ambient_identity()?;
        let session = Session::new(
            name.into(),
            workspace_id,
            canon,
            mode,
            enabled_skills,
            final_ports,
            final_image,
            post_create,
            check_command,
            setup_command,
            setup_approved,
            setup_reviewed,
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

    /// Every Session owned by one Workspace, including closed history.
    pub fn list_for_workspace(&self, workspace_id: &str) -> Vec<Session> {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|session| session.workspace_id == workspace_id)
            .cloned()
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_active));
        sessions
    }

    /// Persist the durable owner for a legacy Session and converge its runtime
    /// path onto that Workspace's canonical path. Ways and peer exclusion are
    /// keyed by `workspace_id`, so leaving a case or normalization alias in
    /// `working_dir` would let one logical Workspace name two host mounts.
    pub fn assign_workspace(
        &mut self,
        session_id: &str,
        workspace: &Workspace,
    ) -> Result<bool, SessionError> {
        if workspace.id.trim().is_empty() {
            return Err(SessionError::MissingWorkspace);
        }
        if !workspace.canonical_path.is_absolute() {
            return Err(
                WorkspaceError::BadPath(workspace.canonical_path.display().to_string()).into(),
            );
        }
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
        if session.workspace_id == workspace.id && session.working_dir == workspace.canonical_path {
            return Ok(false);
        }
        session.workspace_id = workspace.id.clone();
        session.working_dir = workspace.canonical_path.clone();
        let snapshot = session.clone();
        self.persist(&snapshot)?;
        Ok(true)
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

    /// Set the project's check command. `None` or empty clears it.
    pub fn set_check_command(
        &mut self,
        id: &str,
        cmd: Option<String>,
    ) -> Result<Session, SessionError> {
        let s = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        s.check_command = cmd.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        let out = s.clone();
        self.persist(&out)?;
        Ok(out)
    }

    /// Replace the requested runtime and setup decision, invalidating prior
    /// readiness evidence. The daemon must prepare this exact configuration
    /// before it can mark the Session Ready again.
    pub fn configure_environment(
        &mut self,
        id: &str,
        image: Option<String>,
        setup_command: Option<String>,
        setup_approved: bool,
        setup_reviewed: bool,
    ) -> Result<Session, SessionError> {
        let setup_command = normalize_command(setup_command);
        let mut candidate = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        candidate.image = image
            .map(|image| image.trim().to_string())
            .filter(|image| !image.is_empty());
        let generation = candidate.environment.generation.saturating_add(1).max(1);
        candidate.environment =
            SessionEnvironment::planned(setup_command, setup_approved, setup_reviewed);
        candidate.environment.generation = generation;
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Persist a preparation transition and its bounded evidence.
    pub fn set_environment(
        &mut self,
        id: &str,
        state: SessionEnvironmentState,
        effective_image: Option<String>,
        runtime: Option<SessionRuntimeIdentity>,
        setup_results: Vec<SessionSetupResult>,
        error: Option<String>,
    ) -> Result<Session, SessionError> {
        let mut candidate = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        candidate.environment.state = state;
        candidate.environment.effective_image = effective_image;
        candidate.environment.runtime = runtime;
        if candidate.environment.runtime.is_some() {
            candidate.environment.runtime_creation = None;
        }
        candidate.environment.setup_results = setup_results;
        candidate.environment.error = error;
        candidate.environment.prepared_at = matches!(
            state,
            SessionEnvironmentState::Ready | SessionEnvironmentState::Failed
        )
        .then(now_secs);
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Persist an early-reconciliation transition without discarding a
    /// separately retained ambiguous-create marker. Released writers never
    /// intentionally produced both `runtime` and `runtime_creation`, but a
    /// legacy-exposed control-plane file may contain both. Startup must clean
    /// both provider authorities independently rather than letting an error on
    /// one silently erase the other.
    pub fn set_environment_preserving_runtime_creation(
        &mut self,
        id: &str,
        state: SessionEnvironmentState,
        effective_image: Option<String>,
        runtime: Option<SessionRuntimeIdentity>,
        setup_results: Vec<SessionSetupResult>,
        error: Option<String>,
    ) -> Result<Session, SessionError> {
        let mut candidate = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        candidate.environment.state = state;
        candidate.environment.effective_image = effective_image;
        candidate.environment.runtime = runtime;
        candidate.environment.setup_results = setup_results;
        candidate.environment.error = error;
        candidate.environment.prepared_at = matches!(
            state,
            SessionEnvironmentState::Ready | SessionEnvironmentState::Failed
        )
        .then(now_secs);
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Persist evidence only while the exact preparation generation still
    /// owns the Session. Detached remote creation may outlive its HTTP caller;
    /// this CAS prevents it from overwriting a later explicit rebuild.
    pub fn set_environment_if_preparing(
        &mut self,
        id: &str,
        generation: u64,
        effective_image: Option<String>,
        runtime: Option<SessionRuntimeIdentity>,
        setup_results: Vec<SessionSetupResult>,
    ) -> Result<Session, SessionError> {
        let current = self
            .sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if current.environment.state != SessionEnvironmentState::Preparing
            || current.environment.generation != generation
        {
            return Err(SessionError::BadWorkingDir(format!(
                "environment preparation generation {generation} no longer owns Session '{id}'"
            )));
        }
        self.set_environment(
            id,
            SessionEnvironmentState::Preparing,
            effective_image,
            runtime,
            setup_results,
            None,
        )
    }

    /// Persist a unique remote creation token before the provider POST while
    /// the exact preparation generation still owns the Session.
    pub fn set_runtime_creation_if_preparing(
        &mut self,
        id: &str,
        generation: u64,
        creation: SessionRuntimeCreationAttempt,
    ) -> Result<Session, SessionError> {
        let current = self
            .sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if current.environment.state != SessionEnvironmentState::Preparing
            || current.environment.generation != generation
        {
            return Err(SessionError::BadWorkingDir(format!(
                "environment preparation generation {generation} no longer owns Session '{id}'"
            )));
        }
        if current.environment.runtime.is_some() {
            return Err(SessionError::BadWorkingDir(format!(
                "Session '{id}' already has a persisted runtime identity"
            )));
        }
        let mut candidate = current.clone();
        candidate.environment.runtime_creation = Some(creation);
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Clear one exact creation marker after checked provider reconciliation.
    /// A stale cleanup task cannot clear a newer generation/token.
    pub fn clear_runtime_creation(
        &mut self,
        id: &str,
        generation: u64,
        expected_token: &str,
    ) -> Result<Session, SessionError> {
        let current = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if current.environment.generation != generation {
            return Ok(current);
        }
        let Some(creation) = current.environment.runtime_creation.as_ref() else {
            return Ok(current);
        };
        if creation.token != expected_token {
            return Ok(current);
        }
        let mut candidate = current;
        candidate.environment.runtime_creation = None;
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Release one ambiguous E2B creation marker only after an operator has
    /// explicitly verified provider metadata and confirmed every sandbox with
    /// this exact token was deleted. Unlike automatic reconciliation, this is
    /// intentionally strict: a stale generation or token is an error rather
    /// than a no-op, so confirmation cannot silently apply to changed state.
    pub fn confirm_runtime_creation_cleanup(
        &mut self,
        id: &str,
        expected_generation: u64,
        expected_token: &str,
    ) -> Result<Session, SessionError> {
        let current = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if current.environment.generation != expected_generation {
            return Err(SessionError::BadWorkingDir(format!(
                "runtime creation generation changed; expected {expected_generation}, current generation is {}",
                current.environment.generation
            )));
        }
        if current.environment.runtime.is_some() {
            return Err(SessionError::BadWorkingDir(
                "Session now has an exact runtime identity; confirm that runtime instead"
                    .to_string(),
            ));
        }
        let creation = current
            .environment
            .runtime_creation
            .as_ref()
            .ok_or_else(|| {
                SessionError::BadWorkingDir(
                    "Session has no retained runtime creation token".to_string(),
                )
            })?;
        if creation.backend != "e2b" {
            return Err(SessionError::BadWorkingDir(format!(
                "runtime creation token belongs to backend '{}', not E2B",
                creation.backend
            )));
        }
        if creation.token != expected_token {
            return Err(SessionError::BadWorkingDir(
                "runtime creation token changed; reload the Session before confirming cleanup"
                    .to_string(),
            ));
        }
        let mut candidate = current;
        candidate.environment.runtime_creation = None;
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Fsync every exact id returned by creation-token discovery before any
    /// destructive provider request is issued.
    pub fn set_runtime_creation_discovered_ids(
        &mut self,
        id: &str,
        generation: u64,
        expected_token: &str,
        mut discovered_ids: Vec<String>,
    ) -> Result<Session, SessionError> {
        let current = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if current.environment.generation != generation {
            return Err(SessionError::BadWorkingDir(format!(
                "environment generation {generation} no longer owns Session '{id}'"
            )));
        }
        let Some(creation) = current.environment.runtime_creation.as_ref() else {
            return Err(SessionError::BadWorkingDir(format!(
                "Session '{id}' has no retained runtime creation token"
            )));
        };
        if creation.token != expected_token {
            return Err(SessionError::BadWorkingDir(format!(
                "runtime creation token no longer matches Session '{id}'"
            )));
        }
        discovered_ids.sort();
        discovered_ids.dedup();
        if discovered_ids.is_empty() {
            return Err(SessionError::BadWorkingDir(
                "cannot persist an empty discovered runtime id set".to_string(),
            ));
        }
        let mut candidate = current;
        candidate
            .environment
            .runtime_creation
            .as_mut()
            .expect("creation checked above")
            .discovered_ids = discovered_ids;
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Mark one exact retained runtime identity as already cleaned. The id is
    /// intentionally preserved until the next environment transition, closing
    /// the crash window between backend deletion and Close/configure commit.
    pub fn confirm_environment_runtime_cleanup(
        &mut self,
        id: &str,
        expected_runtime_id: &str,
    ) -> Result<Session, SessionError> {
        let mut candidate = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        let runtime = candidate.environment.runtime.as_mut().ok_or_else(|| {
            SessionError::BadWorkingDir("Session has no retained runtime identity".to_string())
        })?;
        if runtime.id != expected_runtime_id {
            return Err(SessionError::BadWorkingDir(format!(
                "runtime identity is '{}', not '{expected_runtime_id}'",
                runtime.id
            )));
        }
        runtime.cleanup_confirmed = true;
        self.persist(&candidate)?;
        self.sessions.insert(id.to_string(), candidate.clone());
        Ok(candidate)
    }

    /// Cancellation-safe preparation rollback. A detached cleanup task may
    /// race a newer explicit rebuild; it may only fail the exact still-
    /// Preparing generation and must never overwrite a later Ready decision.
    pub fn fail_environment_if_preparing(
        &mut self,
        id: &str,
        generation: u64,
        effective_image: Option<String>,
        runtime: Option<SessionRuntimeIdentity>,
        setup_results: Vec<SessionSetupResult>,
        error: String,
    ) -> Result<Session, SessionError> {
        let current = self
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if current.environment.state != SessionEnvironmentState::Preparing
            || current.environment.generation != generation
        {
            return Ok(current);
        }
        self.set_environment(
            id,
            SessionEnvironmentState::Failed,
            effective_image,
            runtime,
            setup_results,
            Some(error),
        )
    }

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
        if !self.sessions.contains_key(id) {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let relative = format!("{id}.json");
        if self.secure_dir.is_file(&relative)? {
            self.secure_dir.remove_file(relative)?;
        }
        // Only hide the owner after durable deletion succeeds. If unlink
        // fails, callers receive an error and the in-memory/disk views agree;
        // restart cannot resurrect a Session the current process hid.
        self.sessions.remove(id);
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
        let bytes = serde_json::to_vec_pretty(session)?;
        self.secure_dir
            .atomic_write(format!("{}.json", session.id), &bytes)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceMigrationReport {
    pub created_workspaces: usize,
    pub linked_sessions: usize,
}

/// Converge legacy path-owned Sessions onto durable Workspaces. Safe to replay
/// after any partial write: canonical path identity and Session assignment are
/// both idempotent.
pub fn migrate_sessions_to_workspaces(
    sessions: &mut SessionStore,
    workspaces: &mut WorkspaceStore,
) -> Result<WorkspaceMigrationReport, SessionError> {
    let mut report = WorkspaceMigrationReport::default();
    for session in sessions.list() {
        let (workspace, created) = workspaces.ensure_for_migration(
            &session.working_dir,
            session.created_at,
            session.last_active,
        )?;
        report.created_workspaces += usize::from(created);
        report.linked_sessions += usize::from(sessions.assign_workspace(&session.id, &workspace)?);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("axo-detect-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detect_prefers_a_script_the_project_defined_for_itself() {
        let d = tmp();
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"build":"tsc","check":"tsc && node --test"}}"#,
        )
        .unwrap();
        // `check` wins over `build` — the project already answered this.
        assert_eq!(detect_check_command(&d).as_deref(), Some("npm run check"));
    }

    #[test]
    fn detect_falls_back_through_the_scripts_it_knows() {
        let d = tmp();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"build":"tsc"}}"#).unwrap();
        assert_eq!(detect_check_command(&d).as_deref(), Some("npm run build"));
    }

    #[test]
    fn detect_reads_language_conventions() {
        let d = tmp();
        std::fs::write(d.join("Cargo.toml"), "[package]\nname='x'").unwrap();
        assert_eq!(detect_check_command(&d).as_deref(), Some("cargo test"));
    }

    #[test]
    fn detect_says_nothing_rather_than_guessing() {
        // An arbitrary default would rule attempts out with a command the
        // project never runs — worse than asking the user.
        assert_eq!(detect_check_command(&tmp()), None);
    }

    #[test]
    fn detect_survives_malformed_package_json() {
        let d = tmp();
        std::fs::write(d.join("package.json"), "{not json").unwrap();
        assert_eq!(detect_check_command(&d), None);
    }

    #[cfg(unix)]
    #[test]
    fn session_creation_rejects_symlinked_package_json_without_following_it() {
        use std::os::unix::fs::symlink;

        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), r#"{"scripts":{"check":"attacker"}}"#).unwrap();
        symlink(outside.path(), work.path().join("package.json")).unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();

        let error = store
            .create(
                "unsafe metadata",
                "wsp-safe-read",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
        assert!(store.is_empty());
    }

    #[test]
    fn session_creation_rejects_oversized_package_json() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        std::fs::write(
            work.path().join("package.json"),
            vec![b' '; MAX_PACKAGE_JSON_BYTES + 1],
        )
        .unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();

        let error = store
            .create(
                "oversized metadata",
                "wsp-bounded-read",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
        assert!(store.is_empty());
    }

    #[test]
    fn package_lock_suggests_reproducible_setup_without_executing_it() {
        let work = tempdir().unwrap();
        std::fs::write(work.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(detect_setup_command(work.path()).as_deref(), Some("npm ci"));
    }

    #[test]
    fn detected_devcontainer_command_cannot_inherit_bare_approval() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        std::fs::create_dir(work.path().join(".devcontainer")).unwrap();
        std::fs::write(
            work.path().join(".devcontainer/devcontainer.json"),
            r#"{"postCreateCommand":"touch should-never-run"}"#,
        )
        .unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let session = store
            .create_with_environment(
                "Unreviewed repository command",
                "wsp-consent",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                true,
                false,
            )
            .unwrap();

        assert_eq!(
            session.environment.state,
            SessionEnvironmentState::AwaitingApproval
        );
        assert_eq!(
            session.environment.setup_command.as_deref(),
            Some("touch should-never-run")
        );
        assert!(!session.environment.setup_approved);
        assert!(!session.environment.setup_reviewed);
    }

    #[test]
    fn explicit_unapproved_command_stays_awaiting_approval() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let session = store
            .create_with_environment(
                "Review first",
                "wsp-review",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                Some("npm ci".into()),
                false,
                true,
            )
            .unwrap();

        assert_eq!(
            session.environment.state,
            SessionEnvironmentState::AwaitingApproval
        );
        assert!(!session.environment.setup_approved);
        assert!(session.environment.setup_reviewed);
    }

    #[test]
    fn absence_of_detected_setup_is_not_an_implicit_skip_decision() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let session = store
            .create_with_environment(
                "Review runtime",
                "wsp-review-runtime",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                false,
                false,
            )
            .unwrap();

        assert_eq!(
            session.environment.state,
            SessionEnvironmentState::AwaitingApproval
        );
        assert!(session.environment.setup_command.is_none());
        assert!(!session.environment.setup_reviewed);
    }

    #[test]
    fn an_explicit_empty_port_list_stays_closed_while_legacy_missing_ports_backfill() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let (explicit_id, legacy_id) = {
            let mut store = SessionStore::new(&sessions_dir).unwrap();
            let explicit = store
                .create(
                    "No Preview ports",
                    "wsp-ports",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                )
                .unwrap();
            let legacy = store
                .create(
                    "Legacy ports",
                    "wsp-ports",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    vec![5173],
                    None,
                )
                .unwrap();
            (explicit.id, legacy.id)
        };

        let legacy_path = sessions_dir.join(format!("{legacy_id}.json"));
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&legacy_path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("exposed_ports");
        std::fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        assert!(reopened.get(&explicit_id).unwrap().exposed_ports.is_empty());
        assert_eq!(
            reopened.get(&legacy_id).unwrap().exposed_ports,
            LEGACY_DEFAULT_EXPOSED_PORTS
        );
    }

    #[test]
    fn malformed_session_mode_cannot_hide_strict_runtime_recovery_authority() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let mut store = SessionStore::new(&sessions_dir).unwrap();
        let session = store
            .create_with_environment(
                "Malformed mode recovery",
                "wsp-recovery",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let generation = session.environment.generation;
        store
            .set_environment(
                &session.id,
                SessionEnvironmentState::Ready,
                Some("e2b:base".into()),
                Some(SessionRuntimeIdentity {
                    backend: "e2b".into(),
                    id: "sandbox-owned".into(),
                    remote_root: Some("/home/user/repository".into()),
                    control_plane: Some("https://api.e2b.dev".into()),
                    data_plane_domain: Some("e2b.app".into()),
                    authority_fingerprint: Some("fingerprint".into()),
                    ownership_token: Some(format!(
                        "{}:{generation}:{}",
                        session.id,
                        uuid::Uuid::new_v4()
                    )),
                    cleanup_confirmed: false,
                }),
                Vec::new(),
                None,
            )
            .unwrap();
        drop(store);

        let path = sessions_dir.join(format!("{}.json", session.id));
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        persisted["mode"] = serde_json::json!({ "single_agent": { "agent_id": 7 } });
        std::fs::write(&path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();
        let forged_name = format!("ses-{}.json", uuid::Uuid::new_v4());
        std::fs::write(
            sessions_dir.join(forged_name),
            serde_json::to_vec_pretty(&persisted).unwrap(),
        )
        .unwrap();

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        let records = reopened.unloaded_runtime_recovery_records().unwrap();
        assert_eq!(records.len(), 2);
        let owned = records
            .iter()
            .find(|record| record.id == session.id)
            .expect("the exact filename/id pair retains cleanup authority");
        let environment = owned
            .environment
            .as_ref()
            .expect("cleanup envelope stays independently decodable");
        assert_eq!(environment.generation, generation);
        assert_eq!(environment.state, SessionEnvironmentState::Ready);
        assert_eq!(
            environment
                .runtime
                .as_ref()
                .map(|runtime| runtime.id.as_str()),
            Some("sandbox-owned")
        );
        let forged = records
            .iter()
            .find(|record| record.id != session.id)
            .expect("every canonical unloaded filename remains visible to cleanup");
        assert!(
            forged.environment.is_none(),
            "a mismatched embedded id must never acquire remote authority"
        );
        reopened.load_all().unwrap();
        assert!(reopened.get(&session.id).is_none());
    }

    #[test]
    fn legacy_node_session_migrates_to_unapproved_npm_ci_proposal() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        std::fs::write(work.path().join("package-lock.json"), "{}").unwrap();
        let sessions_dir = data.path().join("sessions");
        let id = {
            let mut store = SessionStore::new(&sessions_dir).unwrap();
            store
                .create_with_environment(
                    "Legacy Node project",
                    "wsp-legacy",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    false,
                    true,
                )
                .unwrap()
                .id
        };
        let path = sessions_dir.join(format!("{id}.json"));
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        persisted.as_object_mut().unwrap().remove("environment");
        std::fs::write(&path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        let environment = reopened.get(&id).unwrap().environment;
        assert_eq!(environment.state, SessionEnvironmentState::AwaitingApproval);
        assert_eq!(environment.setup_command.as_deref(), Some("npm ci"));
        assert!(!environment.setup_approved);
        assert!(!environment.setup_reviewed);
    }

    #[test]
    fn legacy_session_without_setup_signal_still_requires_runtime_review() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let id = {
            let mut store = SessionStore::new(&sessions_dir).unwrap();
            store
                .create_with_environment(
                    "Legacy plain project",
                    "wsp-legacy-plain",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    false,
                    true,
                )
                .unwrap()
                .id
        };
        let path = sessions_dir.join(format!("{id}.json"));
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        persisted.as_object_mut().unwrap().remove("environment");
        std::fs::write(&path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        let environment = reopened.get(&id).unwrap().environment;
        assert_eq!(environment.state, SessionEnvironmentState::AwaitingApproval);
        assert!(environment.setup_command.is_none());
        assert!(!environment.setup_reviewed);
    }

    #[test]
    fn explicit_no_setup_decision_survives_restart_without_redetection() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        std::fs::write(work.path().join("package-lock.json"), "{}").unwrap();
        let sessions_dir = data.path().join("sessions");
        let id = {
            let mut store = SessionStore::new(&sessions_dir).unwrap();
            store
                .create_with_environment(
                    "No setup by decision",
                    "wsp-skip",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    false,
                    true,
                )
                .unwrap()
                .id
        };

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        let environment = reopened.get(&id).unwrap().environment;
        assert_eq!(environment.state, SessionEnvironmentState::Unprepared);
        assert!(environment.setup_command.is_none());
        assert!(environment.setup_reviewed);
    }

    #[test]
    fn stale_preparation_failure_cannot_overwrite_new_generation() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let session = store
            .create_with_environment(
                "Generation guard",
                "wsp-generation",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                false,
                true,
            )
            .unwrap();
        store
            .set_environment(
                &session.id,
                SessionEnvironmentState::Preparing,
                None,
                None,
                Vec::new(),
                None,
            )
            .unwrap();
        let old_generation = store.get(&session.id).unwrap().environment.generation;
        let new_plan = store
            .configure_environment(&session.id, None, Some("npm ci".into()), false, true)
            .unwrap();
        let observed = store
            .fail_environment_if_preparing(
                &session.id,
                old_generation,
                None,
                None,
                Vec::new(),
                "old request was cancelled".into(),
            )
            .unwrap();

        assert_eq!(observed.environment, new_plan.environment);
        assert_eq!(
            observed.environment.state,
            SessionEnvironmentState::AwaitingApproval
        );
    }

    #[test]
    fn failed_environment_persist_does_not_mutate_visible_state() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let mut store = SessionStore::new(&sessions_dir).unwrap();
        let session = store
            .create_with_environment(
                "Transactional transition",
                "wsp-transaction",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let persisted_path = sessions_dir.join(format!("{}.json", session.id));
        std::fs::remove_file(&persisted_path).unwrap();
        std::fs::create_dir(&persisted_path).unwrap();

        assert!(store
            .set_environment(
                &session.id,
                SessionEnvironmentState::Ready,
                Some("image-that-was-not-persisted".into()),
                None,
                Vec::new(),
                None,
            )
            .is_err());
        assert_eq!(
            store.get(&session.id).unwrap().environment,
            session.environment
        );
    }

    #[test]
    fn malformed_devcontainer_is_an_actionable_creation_error() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        std::fs::create_dir(work.path().join(".devcontainer")).unwrap();
        std::fs::write(
            work.path().join(".devcontainer/devcontainer.json"),
            "{not valid json",
        )
        .unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();

        let error = store
            .create(
                "Malformed config",
                "wsp-malformed",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap_err();
        assert!(matches!(error, SessionError::DevContainer(_)));
    }

    #[test]
    fn environment_approval_and_result_survive_store_reload() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let id;
        {
            let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
            let session = store
                .create_with_environment(
                    "Prepared Node project",
                    "wsp-node",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    Some("docker.io/library/node:20-slim".into()),
                    Some(" npm ci ".into()),
                    true,
                    true,
                )
                .unwrap();
            id = session.id;
            store
                .set_environment(
                    &id,
                    SessionEnvironmentState::Ready,
                    Some("docker.io/library/node:20-slim".into()),
                    Some(SessionRuntimeIdentity {
                        backend: "podman".into(),
                        id: id.clone(),
                        remote_root: None,
                        control_plane: None,
                        data_plane_domain: None,
                        authority_fingerprint: None,
                        ownership_token: None,
                        cleanup_confirmed: false,
                    }),
                    vec![SessionSetupResult {
                        command: "npm ci".into(),
                        exit_code: 0,
                        stdout: "added packages".into(),
                        stderr: String::new(),
                        completed_at: 42,
                    }],
                    None,
                )
                .unwrap();
        }

        let mut reopened = SessionStore::new(data.path().join("sessions")).unwrap();
        reopened.load_all().unwrap();
        let session = reopened.get(&id).unwrap();
        assert_eq!(session.environment.state, SessionEnvironmentState::Ready);
        assert!(session.environment.setup_approved);
        assert_eq!(session.environment.setup_command.as_deref(), Some("npm ci"));
        assert_eq!(session.environment.setup_results.len(), 1);
        assert_eq!(
            session
                .environment
                .runtime
                .as_ref()
                .map(|runtime| runtime.backend.as_str()),
            Some("podman")
        );
        assert_eq!(
            session.environment.effective_image.as_deref(),
            Some("docker.io/library/node:20-slim")
        );
    }

    #[test]
    fn exact_e2b_reattach_identity_survives_store_reload() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let id = {
            let mut store = SessionStore::new(&sessions_dir).unwrap();
            let session = store
                .create_with_environment(
                    "Durable remote workspace",
                    "wsp-e2b",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    false,
                    true,
                )
                .unwrap();
            store
                .set_environment(
                    &session.id,
                    SessionEnvironmentState::Ready,
                    Some("e2b:base".into()),
                    Some(SessionRuntimeIdentity {
                        backend: "e2b".into(),
                        id: "sandbox-exact-123".into(),
                        remote_root: Some("/home/user/axocoatl".into()),
                        control_plane: Some("https://api.e2b.dev".into()),
                        data_plane_domain: Some("e2b.app".into()),
                        authority_fingerprint: Some("fingerprint".into()),
                        ownership_token: Some(format!(
                            "{}:{}:{}",
                            session.id,
                            session.environment.generation,
                            uuid::Uuid::new_v4()
                        )),
                        cleanup_confirmed: false,
                    }),
                    Vec::new(),
                    None,
                )
                .unwrap();
            session.id
        };

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        let runtime = reopened
            .get(&id)
            .unwrap()
            .environment
            .runtime
            .expect("persisted E2B identity");
        assert_eq!(runtime.id, "sandbox-exact-123");
        assert_eq!(runtime.remote_root.as_deref(), Some("/home/user/axocoatl"));
        assert_eq!(runtime.data_plane_domain.as_deref(), Some("e2b.app"));
        assert!(!runtime.cleanup_confirmed);
    }

    #[test]
    fn e2b_creation_token_survives_interruption_until_exact_identity_supersedes_it() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let (id, generation) = {
            let mut store = SessionStore::new(&sessions_dir).unwrap();
            let session = store
                .create_with_environment(
                    "Ambiguous remote create",
                    "wsp-create-token",
                    work.path(),
                    SessionMode::SingleAgent {
                        agent_id: "coder".into(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    false,
                    true,
                )
                .unwrap();
            let preparing = store
                .set_environment(
                    &session.id,
                    SessionEnvironmentState::Preparing,
                    None,
                    None,
                    Vec::new(),
                    None,
                )
                .unwrap();
            let generation = preparing.environment.generation;
            store
                .set_runtime_creation_if_preparing(
                    &session.id,
                    generation,
                    SessionRuntimeCreationAttempt {
                        backend: "e2b".into(),
                        token: "session-generation-unique".into(),
                        remote_root: "/home/user/repository".into(),
                        control_plane: "https://api.e2b.dev".into(),
                        data_plane_domain: "e2b.app".into(),
                        authority_fingerprint: "fingerprint".into(),
                        discovered_ids: Vec::new(),
                    },
                )
                .unwrap();
            (session.id, generation)
        };

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        let interrupted = reopened.get(&id).unwrap();
        assert_eq!(
            interrupted.environment.state,
            SessionEnvironmentState::Failed
        );
        assert_eq!(
            interrupted
                .environment
                .runtime_creation
                .as_ref()
                .map(|creation| creation.token.as_str()),
            Some("session-generation-unique")
        );

        reopened
            .set_environment(
                &id,
                SessionEnvironmentState::Failed,
                Some("e2b:base".into()),
                Some(SessionRuntimeIdentity {
                    backend: "e2b".into(),
                    id: "discovered-exact-id".into(),
                    remote_root: Some("/home/user/repository".into()),
                    control_plane: Some("https://api.e2b.dev".into()),
                    data_plane_domain: Some("e2b.app".into()),
                    authority_fingerprint: Some("fingerprint".into()),
                    ownership_token: Some(format!("{}:{generation}:{}", id, uuid::Uuid::new_v4())),
                    cleanup_confirmed: false,
                }),
                Vec::new(),
                Some("checked cleanup failed".into()),
            )
            .unwrap();
        let promoted = reopened.get(&id).unwrap();
        assert_eq!(promoted.environment.generation, generation);
        assert!(promoted.environment.runtime_creation.is_none());
        assert_eq!(
            promoted
                .environment
                .runtime
                .as_ref()
                .map(|runtime| runtime.id.as_str()),
            Some("discovered-exact-id")
        );
    }

    #[test]
    fn explicit_creation_token_cleanup_confirmation_is_exact_and_durable() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let mut store = SessionStore::new(&sessions_dir).unwrap();
        let session = store
            .create_with_environment(
                "Pre-dispatch crash",
                "wsp-pre-dispatch-crash",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let preparing = store
            .set_environment(
                &session.id,
                SessionEnvironmentState::Preparing,
                Some("e2b:base".into()),
                None,
                Vec::new(),
                None,
            )
            .unwrap();
        let generation = preparing.environment.generation;
        store
            .set_runtime_creation_if_preparing(
                &session.id,
                generation,
                SessionRuntimeCreationAttempt {
                    backend: "e2b".into(),
                    token: "exact-creation-token".into(),
                    remote_root: "/home/user/repository".into(),
                    control_plane: "https://api.e2b.dev".into(),
                    data_plane_domain: "e2b.app".into(),
                    authority_fingerprint: "fingerprint".into(),
                    discovered_ids: Vec::new(),
                },
            )
            .unwrap();
        store
            .fail_environment_if_preparing(
                &session.id,
                generation,
                Some("e2b:base".into()),
                None,
                Vec::new(),
                "provider LIST remained empty after restart".into(),
            )
            .unwrap();

        assert!(store
            .confirm_runtime_creation_cleanup(&session.id, generation + 1, "exact-creation-token")
            .is_err());
        assert!(store
            .confirm_runtime_creation_cleanup(&session.id, generation, "stale-token")
            .is_err());
        let confirmed = store
            .confirm_runtime_creation_cleanup(&session.id, generation, "exact-creation-token")
            .expect("exact explicit confirmation releases only this marker");
        assert!(confirmed.environment.runtime_creation.is_none());

        drop(store);
        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        assert!(reopened
            .get(&session.id)
            .unwrap()
            .environment
            .runtime_creation
            .is_none());
    }

    #[test]
    fn legacy_runtime_identity_defaults_new_reattach_coordinates_to_missing() {
        let runtime: SessionRuntimeIdentity = serde_json::from_value(serde_json::json!({
            "backend": "e2b",
            "id": "legacy-sandbox",
            "control_plane": "https://api.e2b.dev",
            "authority_fingerprint": "legacy-fingerprint"
        }))
        .unwrap();

        assert!(runtime.remote_root.is_none());
        assert!(runtime.data_plane_domain.is_none());
        assert!(!runtime.cleanup_confirmed);
    }

    #[test]
    fn interrupted_environment_preparation_reloads_as_actionable_failure() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let session = store
            .create_with_environment(
                "Interrupted",
                "wsp-node",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
                None,
                false,
                true,
            )
            .unwrap();
        store
            .set_environment(
                &session.id,
                SessionEnvironmentState::Preparing,
                None,
                None,
                Vec::new(),
                None,
            )
            .unwrap();
        drop(store);

        let mut reopened = SessionStore::new(data.path().join("sessions")).unwrap();
        reopened.load_all().unwrap();
        let environment = reopened.get(&session.id).unwrap().environment;
        assert_eq!(environment.state, SessionEnvironmentState::Failed);
        assert!(environment
            .error
            .as_deref()
            .is_some_and(|error| error.contains("rebuild")));
    }

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_rejects_embedded_id_that_does_not_match_filename_before_repair() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let mut writer = SessionStore::new(&sessions_dir).unwrap();
        let session = writer
            .create(
                "poisoned",
                "wsp-poisoned",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let original = sessions_dir.join(format!("{}.json", session.id));
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&original).unwrap()).unwrap();
        value["id"] = serde_json::json!("../outside");
        value.as_object_mut().unwrap().remove("exposed_ports");
        std::fs::write(
            sessions_dir.join("safe.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        value["id"] = serde_json::json!("not-canonical");
        std::fs::write(
            sessions_dir.join("not-canonical.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(original).unwrap();
        let sentinel = data.path().join("outside.json");
        std::fs::write(&sentinel, b"must survive").unwrap();

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();

        assert!(reopened.is_empty());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"must survive");
    }

    #[cfg(unix)]
    #[test]
    fn load_ignores_symlinked_session_records() {
        use std::os::unix::fs::symlink;

        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let external = data.path().join("external.json");
        let session = Session::new(
            "external".into(),
            "wsp-external".into(),
            work.path().to_path_buf(),
            SessionMode::SingleAgent {
                agent_id: "coder".into(),
            },
            Vec::new(),
            Vec::new(),
            None,
            Vec::new(),
            None,
            None,
            false,
            true,
        );
        std::fs::write(&external, serde_json::to_vec_pretty(&session).unwrap()).unwrap();
        let sessions_dir = data.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        symlink(&external, sessions_dir.join(format!("{}.json", session.id))).unwrap();

        let mut reopened = SessionStore::new(sessions_dir).unwrap();
        reopened.load_all().unwrap();
        assert!(reopened.is_empty());
    }

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
                    "wsp-build",
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
            "wsp-bad",
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
                "wsp-x",
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

    #[test]
    #[cfg(unix)]
    fn failed_remove_keeps_session_visible() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let mut store = SessionStore::new(data.path().join("sessions")).unwrap();
        let session = store
            .create(
                "x",
                "wsp-x",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let path = data
            .path()
            .join("sessions")
            .join(format!("{}.json", session.id));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("child"), b"forces non-empty directory error").unwrap();

        assert!(store.remove(&session.id).is_err());
        assert!(store.get(&session.id).is_some());
    }

    #[test]
    fn migration_backfills_legacy_sessions_once_and_preserves_closed_history() {
        let data = tempdir().unwrap();
        let work = tempdir().unwrap();
        let sessions_dir = data.path().join("sessions");
        let mut sessions = SessionStore::new(&sessions_dir).unwrap();
        let session = sessions
            .create(
                "historical work",
                "temporary-owner",
                work.path(),
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        sessions.close(&session.id).unwrap();

        // Model the exact legacy JSON shape by removing workspace_id.
        let session_path = sessions_dir.join(format!("{}.json", session.id));
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("workspace_id");
        std::fs::write(&session_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let mut reopened_sessions = SessionStore::new(&sessions_dir).unwrap();
        reopened_sessions.load_all().unwrap();
        assert!(reopened_sessions
            .get(&session.id)
            .unwrap()
            .workspace_id
            .is_empty());
        let mut workspaces = WorkspaceStore::new(data.path().join("workspaces")).unwrap();

        let first =
            migrate_sessions_to_workspaces(&mut reopened_sessions, &mut workspaces).unwrap();
        assert_eq!(first.created_workspaces, 1);
        assert_eq!(first.linked_sessions, 1);
        let migrated = reopened_sessions.get(&session.id).unwrap();
        assert!(!migrated.workspace_id.is_empty());
        assert_eq!(migrated.status, SessionStatus::Closed);
        assert_eq!(migrated.working_dir, work.path().canonicalize().unwrap());
        assert_eq!(
            workspaces.list().len(),
            1,
            "closing the only Session must not hide its Workspace"
        );
        assert_eq!(
            workspaces
                .get(&migrated.workspace_id)
                .unwrap()
                .canonical_path,
            work.path().canonicalize().unwrap()
        );

        let second =
            migrate_sessions_to_workspaces(&mut reopened_sessions, &mut workspaces).unwrap();
        assert_eq!(second, WorkspaceMigrationReport::default());
        assert_eq!(
            reopened_sessions
                .list_for_workspace(&migrated.workspace_id)
                .len(),
            1,
            "closed Sessions remain owned and discoverable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_reconciles_legacy_workspace_alias_to_the_owned_canonical_path() {
        use std::os::unix::fs::symlink;

        let data = tempdir().unwrap();
        let project_parent = tempdir().unwrap();
        let project = project_parent.path().join("Project");
        let alias = project_parent.path().join("project-alias");
        std::fs::create_dir(&project).unwrap();
        symlink(&project, &alias).unwrap();

        let sessions_dir = data.path().join("sessions");
        let mut sessions = SessionStore::new(&sessions_dir).unwrap();
        let session = sessions
            .create(
                "legacy alias",
                "temporary-owner",
                &project,
                SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let session_path = sessions_dir.join(format!("{}.json", session.id));
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&session_path).unwrap()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("workspace_id");
        object.insert(
            "working_dir".into(),
            serde_json::Value::String(alias.display().to_string()),
        );
        std::fs::write(&session_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let mut reopened = SessionStore::new(&sessions_dir).unwrap();
        reopened.load_all().unwrap();
        let mut workspaces = WorkspaceStore::new(data.path().join("workspaces")).unwrap();
        let report = migrate_sessions_to_workspaces(&mut reopened, &mut workspaces).unwrap();
        assert_eq!(report.linked_sessions, 1);
        let migrated = reopened.get(&session.id).unwrap();
        let workspace = workspaces.get(&migrated.workspace_id).unwrap();
        assert_eq!(migrated.working_dir, workspace.canonical_path);
        assert_eq!(migrated.working_dir, project.canonicalize().unwrap());
        assert_eq!(
            migrate_sessions_to_workspaces(&mut reopened, &mut workspaces).unwrap(),
            WorkspaceMigrationReport::default()
        );
    }
}
