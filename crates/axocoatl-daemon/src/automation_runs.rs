//! Per-automation run history + checkpoints — the foundation for
//! LangGraph-style **time travel**.
//!
//! Every execution of an automation gets a `run_id` and a `Run` record on
//! disk under `{data_dir}/runs/{automation_id}/{run_id}.json`. As the
//! executor advances, we append a `Checkpoint` after each node completes
//! (or after a key state transition like interrupt-parked). The Run holds
//! the ordered list of checkpoints plus run metadata.
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
use std::path::PathBuf;

use axocoatl_config::Automation;
use serde::{Deserialize, Serialize};

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
    root: PathBuf,
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
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            index: tokio::sync::RwLock::new(HashMap::new()),
            active_runs: tokio::sync::RwLock::new(HashSet::new()),
        })
    }

    fn run_path(&self, automation_id: &str, run_id: &str) -> PathBuf {
        self.root
            .join(sanitize(automation_id))
            .join(format!("{}.json", sanitize(run_id)))
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
        self.finish_with_reason(automation_id, run_id, status, None)
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
        let mut active = self.active_runs.write().await;
        let mut run = self.load(automation_id, run_id)?;
        run.status = status;
        run.finished_at_unix = Some(now_unix());
        run.status_reason = reason;
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
        let dir = self.root.join(sanitize(automation_id));
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut runs: Vec<Run> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(run) = serde_json::from_slice::<Run>(&bytes) {
                    runs.push(run);
                }
            }
        }
        runs.sort_by_key(|x| std::cmp::Reverse(x.started_at_unix));
        Ok(runs)
    }

    /// Scan every persisted run. Bootstrap uses this instead of walking the
    /// current Automation store because a snapshotted interrupted run remains
    /// resumable even if its Automation was edited or deleted later.
    pub async fn list_all(&self) -> Result<Vec<Run>, RunStoreError> {
        let mut runs = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let dir = entry?.path();
            if !dir.is_dir() {
                continue;
            }
            for run_entry in std::fs::read_dir(dir)? {
                let path = run_entry?.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(run) = serde_json::from_slice::<Run>(&bytes) {
                        runs.push(run);
                    }
                }
            }
        }
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at_unix));
        Ok(runs)
    }

    pub fn load(&self, automation_id: &str, run_id: &str) -> Result<Run, RunStoreError> {
        let path = self.run_path(automation_id, run_id);
        if !path.exists() {
            return Err(RunStoreError::NotFound(run_id.to_string()));
        }
        let bytes = std::fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn persist(&self, run: &Run) -> Result<(), RunStoreError> {
        let path = self.run_path(&run.automation_id, &run.run_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(run)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strip path-traversal characters from ids before using them as filenames.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':') {
                c
            } else {
                '_'
            }
        })
        .collect()
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
