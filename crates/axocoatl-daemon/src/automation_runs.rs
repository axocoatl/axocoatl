//! Per-automation run history + checkpoints — the foundation for
//! LangGraph-style **time travel**.
//!
//! Every execution of an automation gets a `run_id` and a `Run` record in the
//! collision-safe current namespace under `{data_dir}/automation/runs-v1`.
//! Released 0.1.x records under `{data_dir}/runs` remain read-only migration
//! sources and are promoted only after their embedded identity matches the
//! exact requested logical run. As the executor advances, we append a
//! `Checkpoint` after each node completes (or after a key state transition
//! like interrupt-parked). The Run holds the ordered list of checkpoints plus
//! run metadata.
//!
//! The persisted record supports two honest replay/recovery boundaries:
//!
//! * **List & inspect** — the dashboard "Runs" panel reads back the
//!   history per automation.
//! * **Run again** — start from the beginning with the source run's trigger
//!   and TextInput values while retaining durable ancestry. Arbitrary
//!   checkpoint forks are not exposed; checkpoint continuation is reserved
//!   for a run parked at an Interrupt.
//!
//! Storage is plain JSON files (atomic write via temp+rename), not SQLite,
//! because: (a) runs are append-only, (b) writes are infrequent (once
//! per node), and (c) the dashboard reads them cold via the API.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axocoatl_config::Automation;
use axocoatl_core::{AgentOutput, SecureDir, SecureEntryType, TokenUsageStats};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum RunStoreError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("run '{0}' not found")]
    NotFound(String),
}

/// One executed automation run. Lives on disk; fully serializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub run_id: String,
    pub automation_id: String,
    pub trigger_input: String,
    pub status: RunStatus,
    pub started_at_unix: u64,
    pub finished_at_unix: Option<u64>,
    /// Durable explanation for a status transition that is not self-evident
    /// from the checkpoint timeline (for example, bootstrap abandoning a run
    /// whose process disappeared before reaching a terminal state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    pub checkpoints: Vec<Checkpoint>,
    /// When this run was forked from another run, the source coordinates.
    /// Lets the UI render the run tree.
    pub forked_from: Option<ForkSource>,
    /// Exact graph executed by this run. Older run files predate this field;
    /// recovery may use the current Automation only after validating the
    /// parked Interrupt still exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_snapshot: Option<Automation>,
    /// Explicit TextInput values supplied when the run started. Completed
    /// inputs also live in checkpoints, but retaining the original form makes
    /// a not-yet-executed input deterministic after restart.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub text_inputs: HashMap<String, String>,
    /// Deterministic output produced by the executed runtime sink nodes.
    /// Older run records predate result persistence, so absence is distinct
    /// from a completed run whose legitimate result is an empty string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_content: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    /// Run is paused at an Interrupt node; will move to Running on resume.
    Interrupted,
    /// Forked from. Future runs continue under a different run_id.
    Forked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSource {
    pub source_run_id: String,
    /// `true` means the new run restarted from the beginning. Older records
    /// used only `from_step` for an experimental checkpoint-fork shape.
    #[serde(default)]
    pub from_start: bool,
    /// Retained for backward-compatible decoding of older run records. It is
    /// zero for a whole-run rerun and must not be presented as checkpoint 0
    /// when `from_start` is true.
    #[serde(default)]
    pub from_step: usize,
}

/// Snapshot written after a node completes (or after interrupt-park). The
/// pair `(outputs, active_edges)` is enough to resume execution from this
/// point — both are HashMap-like in-memory state of the executor's loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub step_idx: usize,
    pub node_id: String,
    pub event: CheckpointEvent,
    /// Diagnosis attached to a failed node. Older records predate this field,
    /// so absence means only that no durable detail was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    /// Every node-output known at this point. Keys are node ids.
    pub outputs: HashMap<String, String>,
    /// Every edge that's been activated so far, as `from→to` strings (we
    /// flatten the (String,String) for simpler JSON).
    pub active_edges: HashSet<String>,
    /// Compatibility outputs plus stable activation identities accumulated
    /// before this boundary. Older checkpoints did not retain them.
    #[serde(default)]
    pub agent_outputs: Vec<(String, AgentOutput)>,
    #[serde(default)]
    pub agent_activations: Vec<crate::workflow::AgentActivationOutput>,
    #[serde(default)]
    pub completed_agents: Vec<String>,
    #[serde(default)]
    pub failed_agents: Vec<(String, String)>,
    /// Known provider usage through this boundary. Missing legacy accounting
    /// is conservatively incomplete rather than asserted to be free.
    #[serde(default)]
    pub total_token_usage: TokenUsageStats,
    #[serde(default)]
    pub token_usage_known: bool,
    pub at_unix: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointEvent {
    NodeCompleted,
    NodeFailed,
    NodeSkipped,
    InterruptParked,
    InterruptResumed,
}

/// In-memory cache + on-disk persistence. The cache is mostly for
/// `list_runs` to avoid scanning the directory on every call.
pub struct AutomationRunStore {
    #[cfg_attr(not(test), allow(dead_code))]
    root: PathBuf,
    secure_root: SecureDir,
    #[cfg_attr(not(test), allow(dead_code))]
    legacy_root: PathBuf,
    secure_legacy_root: SecureDir,
    /// `automation_id` → list of `run_id`s in newest-first order. Loaded
    /// lazily per automation; insert-only thereafter.
    index: tokio::sync::RwLock<HashMap<String, Vec<String>>>,
    /// Runs owned by an executor in this process. A newly opened store starts
    /// empty, which is the durable signal bootstrap uses to distinguish runs
    /// abandoned by the previous process from live work in this one.
    active_runs: tokio::sync::RwLock<HashSet<(String, String)>>,
}

impl AutomationRunStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, RunStoreError> {
        let legacy_root = root.into();
        let legacy_name = legacy_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("runs");
        let root = legacy_root.with_file_name(format!(".axocoatl-{legacy_name}-v1"));
        let secure_legacy_root = SecureDir::open_or_create_all(&legacy_root)?;
        let secure_root = SecureDir::open_or_create_all(&root)?;
        Ok(Self {
            root,
            secure_root,
            legacy_root,
            secure_legacy_root,
            index: tokio::sync::RwLock::new(HashMap::new()),
            active_runs: tokio::sync::RwLock::new(HashSet::new()),
        })
    }

    /// Open a run store beneath an already-existing data-directory anchor.
    pub fn open_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, RunStoreError> {
        let data_root = SecureDir::open(data_root)?;
        Self::open_in_secure(&data_root, relative)
    }

    pub fn open_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, RunStoreError> {
        let secure_legacy_root = data_root.child(relative)?;
        let secure_root = data_root.child("automation/runs-v1")?;
        Ok(Self {
            root: secure_root.path().to_path_buf(),
            secure_root,
            legacy_root: secure_legacy_root.path().to_path_buf(),
            secure_legacy_root,
            index: tokio::sync::RwLock::new(HashMap::new()),
            active_runs: tokio::sync::RwLock::new(HashSet::new()),
        })
    }

    fn run_relative(&self, automation_id: &str, run_id: &str) -> PathBuf {
        PathBuf::from(storage_key(automation_id)).join(format!("{}.json", storage_key(run_id)))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn run_path(&self, automation_id: &str, run_id: &str) -> PathBuf {
        self.root.join(self.run_relative(automation_id, run_id))
    }

    fn legacy_run_relative(&self, automation_id: &str, run_id: &str) -> Option<PathBuf> {
        let automation_key = legacy_sanitized_key(automation_id, 255)?;
        let run_key = legacy_sanitized_key(run_id, 250)?;
        Some(PathBuf::from(automation_key).join(format!("{run_key}.json")))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn legacy_run_path(&self, automation_id: &str, run_id: &str) -> Option<PathBuf> {
        self.legacy_run_relative(automation_id, run_id)
            .map(|relative| self.legacy_root.join(relative))
    }

    /// Create a fresh run record and persist the empty state.
    pub async fn start(
        &self,
        automation_id: &str,
        run_id: &str,
        trigger_input: &str,
        forked_from: Option<ForkSource>,
    ) -> Result<Run, RunStoreError> {
        let run = Run {
            run_id: run_id.to_string(),
            automation_id: automation_id.to_string(),
            trigger_input: trigger_input.to_string(),
            status: RunStatus::Running,
            started_at_unix: now_unix(),
            finished_at_unix: None,
            status_reason: None,
            checkpoints: Vec::new(),
            forked_from,
            automation_snapshot: None,
            text_inputs: HashMap::new(),
            final_content: None,
        };
        self.insert(run).await
    }

    /// Create a run with the immutable execution inputs needed for durable
    /// Interrupt recovery. Kept separate from [`Self::start`] so existing
    /// callers that only need the history store retain their API.
    pub async fn start_for_automation(
        &self,
        automation: &Automation,
        run_id: &str,
        trigger_input: &str,
        text_inputs: &HashMap<String, String>,
        forked_from: Option<ForkSource>,
    ) -> Result<Run, RunStoreError> {
        let run = Run {
            run_id: run_id.to_string(),
            automation_id: automation.id.clone(),
            trigger_input: trigger_input.to_string(),
            status: RunStatus::Running,
            started_at_unix: now_unix(),
            finished_at_unix: None,
            status_reason: None,
            checkpoints: Vec::new(),
            forked_from,
            automation_snapshot: Some(automation.clone()),
            text_inputs: text_inputs.clone(),
            final_content: None,
        };
        self.insert(run).await
    }

    async fn insert(&self, run: Run) -> Result<Run, RunStoreError> {
        // Registration and persistence share one ownership lock so startup
        // reconciliation can never observe the file between those steps and
        // mistake a newly started current-process run for an orphan.
        let mut active = self.active_runs.write().await;
        self.persist(&run)?;
        active.insert((run.automation_id.clone(), run.run_id.clone()));
        drop(active);
        let mut idx = self.index.write().await;
        idx.entry(run.automation_id.clone())
            .or_default()
            .insert(0, run.run_id.clone());
        Ok(run)
    }

    /// Append a checkpoint and persist.
    pub async fn checkpoint(
        &self,
        automation_id: &str,
        run_id: &str,
        checkpoint: Checkpoint,
    ) -> Result<(), RunStoreError> {
        let mut run = self.load(automation_id, run_id)?;
        run.checkpoints.push(checkpoint);
        self.persist(&run)?;
        Ok(())
    }

    /// Append a checkpoint and change the run status in one atomic file
    /// replacement. Interrupt parking/resume must use this boundary: writing
    /// either half separately can leave a restart with a status that disagrees
    /// with its latest continuation checkpoint.
    pub async fn transition_with_checkpoint(
        &self,
        automation_id: &str,
        run_id: &str,
        status: RunStatus,
        checkpoint: Checkpoint,
    ) -> Result<(), RunStoreError> {
        let mut active = self.active_runs.write().await;
        let mut run = self.load(automation_id, run_id)?;
        run.checkpoints.push(checkpoint);
        run.status = status;
        run.finished_at_unix = None;
        run.status_reason = None;
        self.persist(&run)?;
        match status {
            RunStatus::Running => {
                active.insert((automation_id.to_string(), run_id.to_string()));
            }
            _ => {
                active.remove(&(automation_id.to_string(), run_id.to_string()));
            }
        }
        Ok(())
    }

    /// Set the run's final status + finished_at.
    pub async fn finish(
        &self,
        automation_id: &str,
        run_id: &str,
        status: RunStatus,
    ) -> Result<(), RunStoreError> {
        self.finish_with_result(automation_id, run_id, status, None, None)
            .await
    }

    /// Finish a run and persist its observable runtime-sink result in the same
    /// atomic file replacement as the terminal status.
    pub async fn finish_with_content(
        &self,
        automation_id: &str,
        run_id: &str,
        status: RunStatus,
        final_content: Option<String>,
    ) -> Result<(), RunStoreError> {
        self.finish_with_result(automation_id, run_id, status, None, final_content)
            .await
    }

    /// Set the run's final status, completion time, and optional durable
    /// explanation in one atomic file replacement.
    pub async fn finish_with_reason(
        &self,
        automation_id: &str,
        run_id: &str,
        status: RunStatus,
        reason: Option<String>,
    ) -> Result<(), RunStoreError> {
        self.finish_with_result(automation_id, run_id, status, reason, None)
            .await
    }

    async fn finish_with_result(
        &self,
        automation_id: &str,
        run_id: &str,
        status: RunStatus,
        reason: Option<String>,
        final_content: Option<String>,
    ) -> Result<(), RunStoreError> {
        let mut active = self.active_runs.write().await;
        let mut run = self.load(automation_id, run_id)?;
        run.status = status;
        run.finished_at_unix = Some(now_unix());
        run.status_reason = reason;
        run.final_content = final_content;
        self.persist(&run)?;
        active.remove(&(automation_id.to_string(), run_id.to_string()));
        Ok(())
    }

    /// Mark a run as interrupted (HITL pause).
    pub async fn mark_interrupted(
        &self,
        automation_id: &str,
        run_id: &str,
    ) -> Result<(), RunStoreError> {
        let mut active = self.active_runs.write().await;
        let mut run = self.load(automation_id, run_id)?;
        run.status = RunStatus::Interrupted;
        run.status_reason = None;
        self.persist(&run)?;
        active.remove(&(automation_id.to_string(), run_id.to_string()));
        Ok(())
    }

    /// Resume from an interrupted state (status back to Running).
    pub async fn mark_running(
        &self,
        automation_id: &str,
        run_id: &str,
    ) -> Result<(), RunStoreError> {
        let mut active = self.active_runs.write().await;
        let mut run = self.load(automation_id, run_id)?;
        run.status = RunStatus::Running;
        run.finished_at_unix = None;
        run.status_reason = None;
        self.persist(&run)?;
        active.insert((automation_id.to_string(), run_id.to_string()));
        Ok(())
    }

    /// Mark persisted `running` records that have no executor in this process
    /// as failed. Bootstrap calls this immediately after opening the store;
    /// the ownership lock also makes the method safe if recovery is invoked
    /// after new current-process runs have started.
    pub async fn reconcile_orphaned_running(
        &self,
        reason: &str,
    ) -> Result<Vec<Run>, RunStoreError> {
        let active = self.active_runs.write().await;
        let mut reconciled = Vec::new();
        for mut run in self.list_all().await? {
            let key = (run.automation_id.clone(), run.run_id.clone());
            if run.status != RunStatus::Running || active.contains(&key) {
                continue;
            }
            run.status = RunStatus::Failed;
            run.finished_at_unix = Some(now_unix());
            run.status_reason = Some(reason.to_string());
            self.persist(&run)?;
            reconciled.push(run);
        }
        Ok(reconciled)
    }

    /// List runs for an automation, newest-first.
    pub async fn list(&self, automation_id: &str) -> Result<Vec<Run>, RunStoreError> {
        let current_dir = PathBuf::from(storage_key(automation_id));
        let legacy_dir = legacy_sanitized_key(automation_id, 255).map(PathBuf::from);
        let mut runs: HashMap<String, (bool, Run)> = HashMap::new();
        let directories = std::iter::once((&self.secure_root, current_dir, true))
            .chain(legacy_dir.map(|legacy| (&self.secure_legacy_root, legacy, false)));
        for (physical_root, dir_relative, current_physical_root) in directories {
            if !physical_root.has_exact_directory(dir_relative.as_os_str())? {
                continue;
            }
            let dir = match physical_root.existing_child(&dir_relative) {
                Ok(dir) => dir,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in dir.entries()? {
                if entry.file_type != SecureEntryType::File {
                    continue;
                }
                let entry_path = Path::new(&entry.name);
                if entry_path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(bytes) = dir.read(&entry.name) {
                    if let Ok(run) = serde_json::from_slice::<Run>(&bytes) {
                        let relative = dir_relative.join(&entry.name);
                        let belongs_to_current = current_physical_root
                            && relative == self.run_relative(automation_id, &run.run_id);
                        let belongs_to_legacy = !current_physical_root
                            && self
                                .legacy_run_relative(automation_id, &run.run_id)
                                .is_some_and(|legacy| relative == legacy);
                        if run.automation_id != automation_id
                            || (!belongs_to_current && !belongs_to_legacy)
                        {
                            continue;
                        }
                        match runs.entry(run.run_id.clone()) {
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                entry.insert((belongs_to_current, run));
                            }
                            std::collections::hash_map::Entry::Occupied(mut entry)
                                if belongs_to_current && !entry.get().0 =>
                            {
                                entry.insert((true, run));
                            }
                            std::collections::hash_map::Entry::Occupied(_) => {}
                        }
                    }
                }
            }
        }
        let mut runs = runs
            .into_values()
            .map(|(current, run)| {
                if !current {
                    self.persist(&run)?;
                }
                Ok(run)
            })
            .collect::<Result<Vec<_>, RunStoreError>>()?;
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at_unix));
        Ok(runs)
    }

    /// Scan every persisted run. Bootstrap uses this instead of walking the
    /// current Automation store because a snapshotted interrupted run remains
    /// resumable even if its Automation was edited or deleted later.
    pub async fn list_all(&self) -> Result<Vec<Run>, RunStoreError> {
        // A legacy POSIX path can coexist with its promoted portable path.
        // Keep one logical record and prefer the current path so restart
        // reconciliation never processes the same run twice or revives stale
        // pre-promotion state.
        let mut runs: HashMap<(String, String), (bool, Run)> = HashMap::new();
        for (physical_root, current_physical_root) in
            [(&self.secure_root, true), (&self.secure_legacy_root, false)]
        {
            for entry in physical_root.entries()? {
                if entry.file_type != SecureEntryType::Directory {
                    continue;
                }
                let dir = physical_root.existing_child(&entry.name)?;
                for run_entry in dir.entries()? {
                    if run_entry.file_type != SecureEntryType::File {
                        continue;
                    }
                    let entry_path = Path::new(&run_entry.name);
                    if entry_path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(bytes) = dir.read(&run_entry.name) {
                        if let Ok(run) = serde_json::from_slice::<Run>(&bytes) {
                            let relative = PathBuf::from(&entry.name).join(&run_entry.name);
                            let current = self.run_relative(&run.automation_id, &run.run_id);
                            let is_current = current_physical_root && relative == current;
                            let is_legacy = !current_physical_root
                                && self
                                    .legacy_run_relative(&run.automation_id, &run.run_id)
                                    .is_some_and(|legacy| relative == legacy);
                            if !is_current && !is_legacy {
                                continue;
                            }

                            let key = (run.automation_id.clone(), run.run_id.clone());
                            match runs.entry(key) {
                                std::collections::hash_map::Entry::Vacant(entry) => {
                                    entry.insert((is_current, run));
                                }
                                std::collections::hash_map::Entry::Occupied(mut entry)
                                    if is_current && !entry.get().0 =>
                                {
                                    entry.insert((true, run));
                                }
                                std::collections::hash_map::Entry::Occupied(_) => {}
                            }
                        }
                    }
                }
            }
        }
        let mut runs = runs
            .into_values()
            .map(|(current, run)| {
                if !current {
                    self.persist(&run)?;
                }
                Ok(run)
            })
            .collect::<Result<Vec<_>, RunStoreError>>()?;
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at_unix));
        Ok(runs)
    }

    pub fn load(&self, automation_id: &str, run_id: &str) -> Result<Run, RunStoreError> {
        let current = self.run_relative(automation_id, run_id);
        let (relative, legacy) = if self.secure_root.is_file(&current)? {
            (current, false)
        } else if let Some(legacy) = self.legacy_run_relative(automation_id, run_id) {
            if self.legacy_file_exists(&legacy)? {
                (legacy, true)
            } else {
                return Err(RunStoreError::NotFound(run_id.to_string()));
            }
        } else {
            return Err(RunStoreError::NotFound(run_id.to_string()));
        };
        let bytes = if legacy {
            self.secure_legacy_root.read(relative)?
        } else {
            self.secure_root.read(relative)?
        };
        let run: Run = serde_json::from_slice(&bytes)?;
        if run.automation_id != automation_id || run.run_id != run_id {
            return Err(RunStoreError::NotFound(run_id.to_string()));
        }
        if legacy {
            self.persist(&run)?;
        }
        Ok(run)
    }

    fn legacy_file_exists(&self, relative: &Path) -> Result<bool, RunStoreError> {
        let Some(parent_name) = relative.parent().and_then(Path::file_name) else {
            return Ok(false);
        };
        let Some(file_name) = relative.file_name() else {
            return Ok(false);
        };
        if !self.secure_legacy_root.has_exact_directory(parent_name)? {
            return Ok(false);
        }
        Ok(self
            .secure_legacy_root
            .existing_child(parent_name)?
            .has_exact_file(file_name)?)
    }

    fn persist(&self, run: &Run) -> Result<(), RunStoreError> {
        let bytes = serde_json::to_vec_pretty(run)?;
        self.secure_root
            .atomic_write(self.run_relative(&run.automation_id, &run.run_id), &bytes)?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Map a logical id to one exact, non-dot filesystem component.
///
/// Preserve simple lowercase portable ids. Other ids use a domain-independent
/// SHA-256 key: the automation and run keys occupy different path levels, and
/// hashing the full bytes keeps distinct logical ids distinct instead of
/// aliasing them through replacement characters.
fn storage_key(id: &str) -> String {
    // Leave room for the run file's `.json.tmp` suffix below common
    // 255-byte component limits. Longer ids could never have been persisted
    // reliably through the historical raw-name path.
    const MAX_PRESERVED_BYTES: usize = 200;
    let mut bytes = id.bytes();
    let safe = !id.is_empty()
        && id.len() <= MAX_PRESERVED_BYTES
        && !is_digest_key(id)
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if safe {
        id.to_string()
    } else {
        format!("sha256-{:x}", Sha256::digest(id.as_bytes()))
    }
}

fn is_digest_key(id: &str) -> bool {
    id.strip_prefix("sha256-")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Exact component produced by the run store shipped in Axocoatl 0.1.x.
///
/// That release replaced every character outside `[A-Za-z0-9._:-]` with an
/// underscore. The mapping was lossy, so callers must validate the embedded
/// automation/run identity before accepting a file. Dot components and names
/// too long to have existed on common filesystems are never consulted.
#[cfg(unix)]
fn legacy_sanitized_key(id: &str, max_bytes: usize) -> Option<String> {
    let sanitized = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    (!sanitized.is_empty()
        && sanitized.len() <= max_bytes
        && !matches!(sanitized.as_str(), "." | ".."))
    .then_some(sanitized)
}

#[cfg(not(unix))]
fn legacy_sanitized_key(_id: &str, _max_bytes: usize) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("axo-runs-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn storage_keys_preserve_safe_ids_and_hash_unsafe_ids_without_aliasing() {
        for id in ["automation-1", "run_2", "v1"] {
            assert_eq!(storage_key(id), id);
        }

        for id in [
            "",
            ".",
            "..",
            "a/b",
            r"a\b",
            "a?b",
            "/absolute",
            "Build",
            "v1.2",
            "scope:run",
        ] {
            let key = storage_key(id);
            assert!(key.starts_with("sha256-"), "unsafe id {id:?} was preserved");
            assert!(!matches!(key.as_str(), "." | ".."));
            assert!(!key.contains('/') && !key.contains('\\'));
        }

        assert!(storage_key(&"a".repeat(201)).starts_with("sha256-"));
        assert_ne!(
            storage_key(&format!("sha256-{}", "a".repeat(64))),
            format!("sha256-{}", "a".repeat(64))
        );

        assert_ne!(storage_key("a/b"), storage_key("a?b"));
        assert_ne!(storage_key("Build"), storage_key("build"));
        assert_ne!(storage_key("."), storage_key(".."));
        assert_ne!(storage_key(""), storage_key("_"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uppercase_legacy_history_is_read_then_written_to_a_distinct_portable_key() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        store.start("Build", "Run-A", "input", None).await.unwrap();
        let current = store.run_path("Build", "Run-A");
        let legacy = store.legacy_run_path("Build", "Run-A").unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::rename(&current, &legacy).unwrap();

        assert_eq!(store.load("Build", "Run-A").unwrap().trigger_input, "input");
        assert_eq!(store.list("Build").await.unwrap().len(), 1);
        assert!(store.list("build").await.unwrap().is_empty());

        store
            .finish("Build", "Run-A", RunStatus::Completed)
            .await
            .unwrap();
        assert!(current.is_file());
        assert!(legacy.is_file());
        assert_eq!(
            store.load("Build", "Run-A").unwrap().status,
            RunStatus::Completed
        );
        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, RunStatus::Completed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shipped_sanitized_history_with_spaces_promotes_without_collision_confusion() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        store
            .start("my job", "run / one", "historical", None)
            .await
            .unwrap();
        let current = store.run_path("my job", "run / one");
        let legacy = store.legacy_run_path("my job", "run / one").unwrap();
        assert_eq!(
            legacy.strip_prefix(&root).unwrap(),
            Path::new("my_job/run___one.json")
        );
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::rename(&current, &legacy).unwrap();

        // `my/job` and `run ? one` collide under the old replacement map.
        // Embedded identity prevents the wrong logical record from loading or
        // being promoted into its new collision-free key.
        assert!(matches!(
            store.load("my/job", "run ? one"),
            Err(RunStoreError::NotFound(_))
        ));
        assert!(store.list("my/job").await.unwrap().is_empty());
        assert!(!store.run_path("my/job", "run ? one").exists());

        let loaded = store.load("my job", "run / one").unwrap();
        assert_eq!(loaded.trigger_input, "historical");
        assert!(current.is_file(), "valid legacy history is promoted");
        assert!(legacy.is_file(), "promotion preserves the legacy source");
        assert_eq!(store.list_all().await.unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mixed_current_directory_prefers_current_run_and_promotes_selected_legacy() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        let current_run = store.start("job", "Run A", "current", None).await.unwrap();
        let current = store.run_path("job", "Run A");
        let legacy = store.legacy_run_path("job", "Run A").unwrap();
        assert_ne!(current.parent(), legacy.parent());

        let mut stale = current_run;
        stale.trigger_input = "legacy".to_string();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        let listed = store.list("job").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].trigger_input, "current");

        std::fs::remove_file(&current).unwrap();
        let promoted = store.list("job").await.unwrap();
        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].trigger_input, "legacy");
        assert!(current.is_file());
        assert!(legacy.is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_run_ancestor_and_predictable_legacy_temp_cannot_escape() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runs");
        std::fs::create_dir(&root).unwrap();
        let outside = parent.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, b"safe").unwrap();

        let store = AutomationRunStore::open(&root).unwrap();
        symlink(&outside, store.root.join("linked")).unwrap();
        assert!(store.start("linked", "run", "input", None).await.is_err());
        assert!(!outside.join("run.json").exists());

        let old_temporary = store.run_path("safe", "run").with_extension("json.tmp");
        std::fs::create_dir_all(old_temporary.parent().unwrap()).unwrap();
        symlink(&sentinel, &old_temporary).unwrap();
        store.start("safe", "run", "input", None).await.unwrap();
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"safe");
        assert!(store.run_path("safe", "run").is_file());
    }

    #[tokio::test]
    async fn list_all_ignores_records_whose_embedded_identity_does_not_match_the_path() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        let run = store.start("safe", "run-1", "input", None).await.unwrap();

        let mut forged_run = run.clone();
        forged_run.run_id = "forged".to_string();
        std::fs::write(
            store.root.join("safe").join("poison.json"),
            serde_json::to_vec_pretty(&forged_run).unwrap(),
        )
        .unwrap();
        let poison_dir = store.root.join("unrelated");
        std::fs::create_dir_all(&poison_dir).unwrap();
        std::fs::write(
            poison_dir.join("poison.json"),
            serde_json::to_vec_pretty(&run).unwrap(),
        )
        .unwrap();

        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].automation_id, "safe");
        assert_eq!(all[0].run_id, "run-1");
        let scoped = store.list("safe").await.unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].run_id, "run-1");
    }

    #[tokio::test]
    async fn unsafe_automation_and_run_ids_stay_contained_and_distinct() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runs");
        let outside = parent.path().join("outside.json");
        std::fs::write(&outside, b"sentinel").unwrap();
        let store = AutomationRunStore::open(&root).unwrap();

        let cases = [
            ("..", "../outside"),
            ("a/b", "same/run"),
            ("a?b", "same?run"),
        ];
        for (automation_id, run_id) in cases {
            store
                .start(automation_id, run_id, "input", None)
                .await
                .unwrap();
            let path = store.run_path(automation_id, run_id);
            assert!(path.starts_with(&store.root));
            assert_eq!(
                path.strip_prefix(&store.root).unwrap().components().count(),
                2
            );
            assert!(path.is_file());
            let loaded = store.load(automation_id, run_id).unwrap();
            assert_eq!(loaded.automation_id, automation_id);
            assert_eq!(loaded.run_id, run_id);
        }

        assert_eq!(std::fs::read(outside).unwrap(), b"sentinel");
        assert_ne!(
            store.run_path("a/b", "same/run"),
            store.run_path("a?b", "same?run")
        );
        assert_eq!(store.list("a/b").await.unwrap().len(), 1);
        assert_eq!(store.list("a?b").await.unwrap().len(), 1);
        assert_eq!(store.list_all().await.unwrap().len(), cases.len());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lowercase_current_and_uppercase_legacy_runs_coexist_without_casefold_adoption() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        let current = store
            .start("build", "run-a", "lowercase current", None)
            .await
            .unwrap();
        let mut legacy = current;
        legacy.automation_id = "Build".to_string();
        legacy.run_id = "Run-A".to_string();
        legacy.trigger_input = "uppercase legacy".to_string();
        let legacy_path = store.legacy_run_path("Build", "Run-A").unwrap();
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let lowercase = store.list("build").await.unwrap();
        assert_eq!(lowercase.len(), 1);
        assert_eq!(lowercase[0].trigger_input, "lowercase current");
        let uppercase = store.list("Build").await.unwrap();
        assert_eq!(uppercase.len(), 1);
        assert_eq!(uppercase[0].trigger_input, "uppercase legacy");
        assert_eq!(
            store.load("build", "run-a").unwrap().trigger_input,
            "lowercase current"
        );
        assert_eq!(
            store.load("Build", "Run-A").unwrap().trigger_input,
            "uppercase legacy"
        );
        let all = store.list_all().await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(legacy_path.is_file());
        assert!(store.run_path("build", "run-a").is_file());
        assert!(store.run_path("Build", "Run-A").is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn literal_legacy_digest_owner_cannot_alias_a_current_hashed_owner() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        let unsafe_id = "My job";
        let literal_digest_id = storage_key(unsafe_id);
        let current = store
            .start(unsafe_id, "run", "current hashed owner", None)
            .await
            .unwrap();
        let mut legacy = current;
        legacy.automation_id = literal_digest_id.clone();
        legacy.run_id = "run".to_string();
        legacy.trigger_input = "legacy literal owner".to_string();
        let legacy_path = store.legacy_run_path(&literal_digest_id, "run").unwrap();
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        assert_eq!(
            store.load(unsafe_id, "run").unwrap().trigger_input,
            "current hashed owner"
        );
        assert_eq!(
            store.load(&literal_digest_id, "run").unwrap().trigger_input,
            "legacy literal owner"
        );
        assert_ne!(
            store.run_path(unsafe_id, "run"),
            store.run_path(&literal_digest_id, "run")
        );
        assert!(legacy_path.is_file());
        assert_eq!(store.list_all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn start_checkpoint_finish_roundtrip() {
        let store = AutomationRunStore::open(tmpdir()).unwrap();
        let run = store.start("auto1", "run-a", "hello", None).await.unwrap();
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.checkpoints.len(), 0);

        let mut outs = HashMap::new();
        outs.insert("n1".to_string(), "step output".to_string());
        store
            .checkpoint(
                "auto1",
                "run-a",
                Checkpoint {
                    step_idx: 0,
                    node_id: "n1".into(),
                    event: CheckpointEvent::NodeCompleted,
                    failure_detail: None,
                    outputs: outs.clone(),
                    active_edges: HashSet::new(),
                    agent_outputs: Vec::new(),
                    agent_activations: Vec::new(),
                    completed_agents: Vec::new(),
                    failed_agents: Vec::new(),
                    total_token_usage: TokenUsageStats::default(),
                    token_usage_known: true,
                    at_unix: now_unix(),
                },
            )
            .await
            .unwrap();
        store
            .finish("auto1", "run-a", RunStatus::Completed)
            .await
            .unwrap();

        let runs = store.list("auto1").await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Completed);
        assert_eq!(runs[0].checkpoints.len(), 1);
        assert_eq!(
            runs[0].checkpoints[0].outputs.get("n1").unwrap(),
            "step output"
        );
        assert!(runs[0].finished_at_unix.is_some());
    }

    #[tokio::test]
    async fn terminal_result_and_status_survive_reopen_together() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        store.start("a", "run-result", "input", None).await.unwrap();
        store
            .finish_with_content(
                "a",
                "run-result",
                RunStatus::Completed,
                Some("durable Automation result".into()),
            )
            .await
            .unwrap();

        let reopened = AutomationRunStore::open(&root).unwrap();
        let run = reopened.load("a", "run-result").unwrap();
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(
            run.final_content.as_deref(),
            Some("durable Automation result")
        );
        assert!(run.finished_at_unix.is_some());
    }

    #[test]
    fn legacy_run_without_terminal_result_still_decodes() {
        let value = serde_json::json!({
            "run_id": "legacy",
            "automation_id": "a",
            "trigger_input": "input",
            "status": "completed",
            "started_at_unix": 1,
            "finished_at_unix": 2,
            "checkpoints": [],
            "forked_from": null,
            "text_inputs": {}
        });
        let run: Run = serde_json::from_value(value).unwrap();
        assert_eq!(run.final_content, None);
    }

    #[tokio::test]
    async fn fork_records_source() {
        let store = AutomationRunStore::open(tmpdir()).unwrap();
        store.start("a", "run-1", "x", None).await.unwrap();
        store
            .start(
                "a",
                "run-2",
                "x",
                Some(ForkSource {
                    source_run_id: "run-1".into(),
                    from_start: false,
                    from_step: 2,
                }),
            )
            .await
            .unwrap();
        let r2 = store.load("a", "run-2").unwrap();
        assert!(r2.forked_from.is_some());
        assert_eq!(r2.forked_from.unwrap().source_run_id, "run-1");
    }

    #[tokio::test]
    async fn interrupt_status_and_checkpoint_transition_as_one_persisted_state() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        store.start("a", "run-1", "input", None).await.unwrap();

        let parked = Checkpoint {
            step_idx: 1,
            node_id: "approval".into(),
            event: CheckpointEvent::InterruptParked,
            failure_detail: None,
            outputs: HashMap::from([("before".into(), "done".into())]),
            active_edges: HashSet::from(["before→approval".into()]),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            completed_agents: Vec::new(),
            failed_agents: Vec::new(),
            total_token_usage: TokenUsageStats::default(),
            token_usage_known: true,
            at_unix: now_unix(),
        };
        store
            .transition_with_checkpoint("a", "run-1", RunStatus::Interrupted, parked)
            .await
            .unwrap();

        // Reopen from disk rather than trusting the same store instance. The
        // status and latest checkpoint must always describe one transition.
        let reopened = AutomationRunStore::open(&root).unwrap();
        let run = reopened.load("a", "run-1").unwrap();
        assert_eq!(run.status, RunStatus::Interrupted);
        assert_eq!(
            run.checkpoints.last().map(|checkpoint| checkpoint.event),
            Some(CheckpointEvent::InterruptParked)
        );

        let resumed = Checkpoint {
            step_idx: 1,
            node_id: "approval".into(),
            event: CheckpointEvent::InterruptResumed,
            failure_detail: None,
            outputs: HashMap::from([
                ("before".into(), "done".into()),
                ("approval".into(), "approved".into()),
            ]),
            active_edges: HashSet::from(["before→approval".into(), "approval→after".into()]),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            completed_agents: Vec::new(),
            failed_agents: Vec::new(),
            total_token_usage: TokenUsageStats::default(),
            token_usage_known: true,
            at_unix: now_unix(),
        };
        reopened
            .transition_with_checkpoint("a", "run-1", RunStatus::Running, resumed)
            .await
            .unwrap();

        let reopened_again = AutomationRunStore::open(&root).unwrap();
        let run = reopened_again.load("a", "run-1").unwrap();
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(
            run.checkpoints.last().map(|checkpoint| checkpoint.event),
            Some(CheckpointEvent::InterruptResumed)
        );
        assert_eq!(run.checkpoints.len(), 2);
    }

    #[tokio::test]
    async fn restart_reconciliation_fails_only_runs_without_a_current_process_owner() {
        let root = tmpdir();
        let first_process = AutomationRunStore::open(&root).unwrap();
        first_process
            .start("a", "active-run", "input", None)
            .await
            .unwrap();
        first_process
            .start("a", "parked-run", "input", None)
            .await
            .unwrap();
        first_process
            .transition_with_checkpoint(
                "a",
                "parked-run",
                RunStatus::Interrupted,
                Checkpoint {
                    step_idx: 0,
                    node_id: "approval".into(),
                    event: CheckpointEvent::InterruptParked,
                    failure_detail: None,
                    outputs: HashMap::new(),
                    active_edges: HashSet::new(),
                    agent_outputs: Vec::new(),
                    agent_activations: Vec::new(),
                    completed_agents: Vec::new(),
                    failed_agents: Vec::new(),
                    total_token_usage: TokenUsageStats::default(),
                    token_usage_known: true,
                    at_unix: now_unix(),
                },
            )
            .await
            .unwrap();

        let reconciled = first_process
            .reconcile_orphaned_running("restart interrupted the run")
            .await
            .unwrap();
        assert!(reconciled.is_empty());
        assert_eq!(
            first_process.load("a", "active-run").unwrap().status,
            RunStatus::Running
        );

        drop(first_process);
        let restarted_process = AutomationRunStore::open(&root).unwrap();
        let reconciled = restarted_process
            .reconcile_orphaned_running("restart interrupted the run")
            .await
            .unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].run_id, "active-run");
        assert_eq!(
            restarted_process.load("a", "parked-run").unwrap().status,
            RunStatus::Interrupted
        );

        let failed = restarted_process.load("a", "active-run").unwrap();
        assert_eq!(failed.status, RunStatus::Failed);
        assert!(failed.finished_at_unix.is_some());
        assert_eq!(
            failed.status_reason.as_deref(),
            Some("restart interrupted the run")
        );

        let reopened = AutomationRunStore::open(&root).unwrap();
        let persisted = reopened.load("a", "active-run").unwrap();
        assert_eq!(persisted.status, RunStatus::Failed);
        assert_eq!(
            persisted.status_reason.as_deref(),
            Some("restart interrupted the run")
        );
        assert_eq!(
            serde_json::to_value(&persisted).unwrap()["status_reason"],
            "restart interrupted the run"
        );
    }

    #[tokio::test]
    async fn checkpoint_failure_detail_survives_reopen() {
        let root = tmpdir();
        let store = AutomationRunStore::open(&root).unwrap();
        store.start("a", "failed-run", "input", None).await.unwrap();
        store
            .checkpoint(
                "a",
                "failed-run",
                Checkpoint {
                    step_idx: 0,
                    node_id: "broken".into(),
                    event: CheckpointEvent::NodeFailed,
                    failure_detail: Some("tool: provider unavailable".into()),
                    outputs: HashMap::from([("broken".into(), String::new())]),
                    active_edges: HashSet::new(),
                    agent_outputs: Vec::new(),
                    agent_activations: Vec::new(),
                    completed_agents: Vec::new(),
                    failed_agents: Vec::new(),
                    total_token_usage: TokenUsageStats::default(),
                    token_usage_known: true,
                    at_unix: now_unix(),
                },
            )
            .await
            .unwrap();
        store
            .finish("a", "failed-run", RunStatus::Failed)
            .await
            .unwrap();

        let reopened = AutomationRunStore::open(&root).unwrap();
        let persisted = reopened.load("a", "failed-run").unwrap();
        assert_eq!(
            persisted.checkpoints[0].failure_detail.as_deref(),
            Some("tool: provider unavailable")
        );
        assert_eq!(
            serde_json::to_value(&persisted).unwrap()["checkpoints"][0]["failure_detail"],
            "tool: provider unavailable"
        );
    }
}
