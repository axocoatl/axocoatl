//! Per-automation run history + checkpoints — the foundation for
//! LangGraph-style **time travel**.
//!
//! Every execution of an automation gets a `run_id` and a `Run` record on
//! disk under `{data_dir}/runs/{automation_id}/{run_id}.json`. As the
//! executor advances, we append a `Checkpoint` after each node completes
//! (or after a key state transition like interrupt-parked). The Run holds
//! the ordered list of checkpoints plus run metadata.
//!
//! Two replay primitives use this:
//!
//! * **List & inspect** — the dashboard "Runs" panel reads back the
//!   history per automation. Each step shows its output, status, and a
//!   "fork from here" action.
//! * **Fork** — start a new run that inherits the state at step `n` of
//!   a prior run. The executor takes the snapshot's `outputs` and
//!   `active_edges`, then continues from the next unexecuted node.
//!
//! Storage is plain JSON files (atomic write via temp+rename), not SQLite,
//! because: (a) runs are append-only, (b) writes are infrequent (once
//! per node), and (c) the dashboard reads them cold via the API.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

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
    pub checkpoints: Vec<Checkpoint>,
    /// When this run was forked from another run, the source coordinates.
    /// Lets the UI render the run tree.
    pub forked_from: Option<ForkSource>,
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
}

impl AutomationRunStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, RunStoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            index: tokio::sync::RwLock::new(HashMap::new()),
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
            checkpoints: Vec::new(),
            forked_from,
        };
        self.persist(&run)?;
        let mut idx = self.index.write().await;
        idx.entry(automation_id.to_string())
            .or_default()
            .insert(0, run_id.to_string());
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

    /// Set the run's final status + finished_at.
    pub async fn finish(
        &self,
        automation_id: &str,
        run_id: &str,
        status: RunStatus,
    ) -> Result<(), RunStoreError> {
        let mut run = self.load(automation_id, run_id)?;
        run.status = status;
        run.finished_at_unix = Some(now_unix());
        self.persist(&run)?;
        Ok(())
    }

    /// Mark a run as interrupted (HITL pause).
    pub async fn mark_interrupted(
        &self,
        automation_id: &str,
        run_id: &str,
    ) -> Result<(), RunStoreError> {
        let mut run = self.load(automation_id, run_id)?;
        run.status = RunStatus::Interrupted;
        self.persist(&run)?;
        Ok(())
    }

    /// Resume from an interrupted state (status back to Running).
    pub async fn mark_running(
        &self,
        automation_id: &str,
        run_id: &str,
    ) -> Result<(), RunStoreError> {
        let mut run = self.load(automation_id, run_id)?;
        run.status = RunStatus::Running;
        self.persist(&run)?;
        Ok(())
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
        runs.sort_by(|a, b| b.started_at_unix.cmp(&a.started_at_unix));
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
        let p = std::env::temp_dir().join(format!(
            "axo-runs-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
                    from_step: 2,
                }),
            )
            .await
            .unwrap();
        let r2 = store.load("a", "run-2").unwrap();
        assert!(r2.forked_from.is_some());
        assert_eq!(r2.forked_from.unwrap().source_run_id, "run-1");
    }
}
