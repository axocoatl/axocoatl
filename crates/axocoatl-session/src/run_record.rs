//! Durable record of one loadout run, beside the Session it ran in:
//! `{data root}/loadout-runs/{run_id}/` with `manifest.json` (written once),
//! `events.jsonl` (append-only, one synced JSON line per event) and
//! `outcome.json` (written once, when the run ends). The Session's own
//! record (turns, activations, tool calls, checks, network) stays where it
//! is; the record bundle joins both (`record_bundle`).
//!
//! Owner: workstream `core`.

use std::collections::BTreeMap;

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::run_outcome::{
    Adjudication, Finding, LoadoutRef, NotCovered, RunOutcome, RunWarning, TurnState,
};

/// Directory under the data root that holds every run record.
pub const RUN_RECORD_DIR: &str = "loadout-runs";
/// `schema` of `manifest.json`.
pub const RUN_MANIFEST_SCHEMA: &str = "axocoatl.run-manifest/1";
/// Largest event line, in bytes.
pub const MAX_RUN_EVENT_BYTES: usize = 256 * 1024;
/// Most events one run records.
pub const MAX_RUN_EVENTS: usize = 100_000;
/// The phase of a Keep as PR result: the one event a run's record takes
/// after its Outcome is written (`RunRecordStore::append_after_end`).
pub const KEEP_PHASE: &str = "keep";

/// `run-<uuid v4>`.
pub fn is_run_id(id: &str) -> bool {
    crate::is_canonical_persisted_id(id, "run-")
}

/// What a Session records about the loadout run that created it. The
/// daemon reads `network` and `workload` from here, never from the global
/// configuration, for this Session's container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadoutBinding {
    pub run_id: String,
    pub loadout: LoadoutRef,
    /// `egress` or `none`.
    pub network: String,
    /// Always `hardened`: non-root workload users, commands under `--harden`.
    pub workload: String,
}

/// Written once when a run is accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema: String,
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: String,
    pub loadout: LoadoutRef,
    /// The exact loadout file text; its SHA-256 is `loadout.digest`.
    pub loadout_text: String,
    pub params: BTreeMap<String, String>,
    pub task: String,
    /// Canonical repository path.
    pub repo: String,
    /// `HEAD` when the run started, when the repository has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_head: Option<String>,
    /// Paths with uncommitted changes when the run started (Keep as PR
    /// refuses to commit them as the run's work).
    #[serde(default)]
    pub dirty_paths: Vec<String>,
    pub started_at_ms: u64,
    /// Run options as given (keep mode, reference URL, timeouts).
    #[serde(default)]
    pub options: serde_json::Value,
}

/// One line of `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    /// Progress for `axocoatl run` and the UI: `preparing`, `applying_team`,
    /// `running`, `checks`, `review`, `repro`, `integrating`, `finishing`.
    Phase {
        at_ms: u64,
        phase: String,
        detail: String,
    },
    TurnStarted {
        at_ms: u64,
        turn_id: String,
        purpose: String,
    },
    TurnEnded {
        at_ms: u64,
        turn_id: String,
        state: TurnState,
    },
    Warning {
        at_ms: u64,
        warning: RunWarning,
    },
    Adjudication {
        at_ms: u64,
        adjudication: Box<Adjudication>,
    },
    Finding {
        at_ms: u64,
        finding: Box<Finding>,
    },
    NotCovered {
        at_ms: u64,
        entry: Box<NotCovered>,
    },
    /// A provider call retried under the transient-error policy.
    ProviderRetry {
        at_ms: u64,
        node_id: String,
        status: Option<u16>,
        reason: String,
    },
    Ended {
        at_ms: u64,
        outcome: Box<RunOutcome>,
    },
}

impl RunEvent {
    /// When the event happened, in Unix milliseconds.
    pub fn at_ms(&self) -> u64 {
        match self {
            RunEvent::Phase { at_ms, .. }
            | RunEvent::TurnStarted { at_ms, .. }
            | RunEvent::TurnEnded { at_ms, .. }
            | RunEvent::Warning { at_ms, .. }
            | RunEvent::Adjudication { at_ms, .. }
            | RunEvent::Finding { at_ms, .. }
            | RunEvent::NotCovered { at_ms, .. }
            | RunEvent::ProviderRetry { at_ms, .. }
            | RunEvent::Ended { at_ms, .. } => *at_ms,
        }
    }
}

/// Why the run record could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum RunRecordError {
    #[error("run record: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("run record storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("run record encoding: {0}")]
    Json(#[from] serde_json::Error),
    #[error("run record: {0}")]
    Invalid(String),
    #[error("no run {0}")]
    NotFound(String),
}

const MANIFEST_FILE: &str = "manifest.json";
const EVENTS_FILE: &str = "events.jsonl";
const OUTCOME_FILE: &str = "outcome.json";
/// Largest `manifest.json` read back: the loadout text is at most 64 KiB and
/// the task is bounded at admission.
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// Largest `outcome.json` read back.
const MAX_OUTCOME_BYTES: usize = 32 * 1024 * 1024;
/// Most run directories one listing reads.
const MAX_LISTED_RUNS: usize = 100_000;

/// Append state of one run's `events.jsonl`, read once per process.
#[derive(Debug, Clone, Copy)]
struct EventsState {
    /// Complete lines in the file.
    lines: u64,
}

/// The run records under one data root.
pub struct RunRecordStore {
    root: SecureDir,
    /// One writer at a time per store; caches each run's line count.
    appends: std::sync::Mutex<std::collections::HashMap<String, EventsState>>,
}

impl std::fmt::Debug for RunRecordStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunRecordStore")
            .field("root", &self.root.path())
            .finish_non_exhaustive()
    }
}

fn require_run_id(run_id: &str) -> Result<(), RunRecordError> {
    if is_run_id(run_id) {
        Ok(())
    } else {
        Err(RunRecordError::Invalid(format!(
            "{run_id:?} is not a run id (run-<uuid>)"
        )))
    }
}

fn not_found(run_id: &str) -> impl FnOnce(std::io::Error) -> RunRecordError + '_ {
    move |error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RunRecordError::NotFound(run_id.to_string())
        } else {
            RunRecordError::Io(error)
        }
    }
}

/// Split `bytes` into complete lines; a last line without its newline was
/// torn by a crash during its append and is not part of the record.
fn complete_lines(bytes: &[u8]) -> (impl Iterator<Item = &[u8]>, usize) {
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    (
        bytes[..complete]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty()),
        complete,
    )
}

impl RunRecordStore {
    /// Open (creating when missing) `{data root}/loadout-runs/`.
    pub fn open(data_root: &SecureDir) -> Result<Self, RunRecordError> {
        let root = data_root.child(RUN_RECORD_DIR)?;
        root.restrict_owner_only()?;
        Ok(Self {
            root,
            appends: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn appends(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, EventsState>> {
        self.appends
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn run_dir(&self, run_id: &str) -> Result<SecureDir, RunRecordError> {
        require_run_id(run_id)?;
        self.root.existing_child(run_id).map_err(not_found(run_id))
    }

    /// Create the run's directory and write its manifest once.
    pub fn create(&self, manifest: &RunManifest) -> Result<(), RunRecordError> {
        require_run_id(&manifest.run_id)?;
        if manifest.schema != RUN_MANIFEST_SCHEMA {
            return Err(RunRecordError::Invalid(format!(
                "a manifest's schema is {RUN_MANIFEST_SCHEMA}"
            )));
        }
        let bytes = serde_json::to_vec_pretty(manifest)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(RunRecordError::Invalid(format!(
                "the run manifest is larger than {MAX_MANIFEST_BYTES} bytes"
            )));
        }
        let _appends = self.appends();
        let dir = self.root.create_child(&manifest.run_id).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                RunRecordError::Invalid(format!("run {} already exists", manifest.run_id))
            } else {
                RunRecordError::Io(error)
            }
        })?;
        dir.restrict_owner_only()?;
        dir.atomic_write(MANIFEST_FILE, &bytes)?;
        dir.sync_all()?;
        self.root.sync_all()?;
        Ok(())
    }

    /// Append one event and sync it before returning. Returns its sequence
    /// number (1 for the first event).
    pub fn append(&self, run_id: &str, event: &RunEvent) -> Result<u64, RunRecordError> {
        self.append_checked(run_id, event, false)
    }

    /// Append a Keep as PR result ([`KEEP_PHASE`]) to a run that has ended:
    /// Keep runs after the Outcome is written, which itself never changes.
    /// Any other event is refused once the run has ended, as by
    /// [`Self::append`]; a run that has not ended takes it like any event.
    pub fn append_after_end(&self, run_id: &str, event: &RunEvent) -> Result<u64, RunRecordError> {
        if !matches!(event, RunEvent::Phase { phase, .. } if phase == KEEP_PHASE) {
            return Err(RunRecordError::Invalid(
                "only a Keep result is recorded after a run has ended".into(),
            ));
        }
        self.append_checked(run_id, event, true)
    }

    fn append_checked(
        &self,
        run_id: &str,
        event: &RunEvent,
        after_end: bool,
    ) -> Result<u64, RunRecordError> {
        let dir = self.run_dir(run_id)?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        if line.len() > MAX_RUN_EVENT_BYTES {
            return Err(RunRecordError::Invalid(format!(
                "a run event is larger than {MAX_RUN_EVENT_BYTES} bytes"
            )));
        }
        let mut appends = self.appends();
        let state = match appends.get(run_id) {
            Some(state) => *state,
            None => {
                let state = Self::recover_events(&dir)?;
                appends.insert(run_id.to_string(), state);
                state
            }
        };
        if state.lines >= MAX_RUN_EVENTS as u64 {
            return Err(RunRecordError::Invalid(format!(
                "run {run_id} has recorded {MAX_RUN_EVENTS} events, the most one run keeps"
            )));
        }
        if !after_end && dir.has_exact_file(OUTCOME_FILE)? {
            return Err(RunRecordError::Invalid(format!(
                "run {run_id} has ended; its record takes no more events"
            )));
        }
        dir.append(EVENTS_FILE, &line, true)?;
        let next = EventsState {
            lines: state.lines + 1,
        };
        appends.insert(run_id.to_string(), next);
        Ok(next.lines)
    }

    /// Count the complete lines of `events.jsonl` and cut a torn last line,
    /// so the next append starts on a line of its own.
    fn recover_events(dir: &SecureDir) -> Result<EventsState, RunRecordError> {
        let bytes = match dir.read_leaf_limited(EVENTS_FILE, MAX_RUN_EVENTS * MAX_RUN_EVENT_BYTES) {
            Ok(None) => return Ok(EventsState { lines: 0 }),
            Ok(Some(SecureLeafBytes::Regular { bytes, .. })) => bytes,
            Ok(Some(_)) => {
                return Err(RunRecordError::Invalid(
                    "events.jsonl is not a regular file".into(),
                ))
            }
            Err(error) => return Err(RunRecordError::Io(error)),
        };
        let (lines, complete) = complete_lines(&bytes);
        let lines = lines.count() as u64;
        if complete < bytes.len() {
            let file = dir.open_append(EVENTS_FILE)?;
            file.set_len(complete as u64)?;
            file.sync_all()?;
        }
        Ok(EventsState { lines })
    }

    pub fn manifest(&self, run_id: &str) -> Result<RunManifest, RunRecordError> {
        let dir = self.run_dir(run_id)?;
        let bytes = dir
            .read_limited(MANIFEST_FILE, MAX_MANIFEST_BYTES)
            .map_err(not_found(run_id))?;
        let manifest: RunManifest = serde_json::from_slice(&bytes)?;
        if manifest.run_id != run_id {
            return Err(RunRecordError::Invalid(format!(
                "the manifest in {run_id} names run {}",
                manifest.run_id
            )));
        }
        Ok(manifest)
    }

    /// Events after sequence number `after`, at most `limit`. A torn last
    /// line is not an event.
    pub fn events(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<(u64, RunEvent)>, RunRecordError> {
        let dir = self.run_dir(run_id)?;
        let bytes = match dir.read_leaf_limited(EVENTS_FILE, MAX_RUN_EVENTS * MAX_RUN_EVENT_BYTES) {
            Ok(None) => return Ok(Vec::new()),
            Ok(Some(SecureLeafBytes::Regular { bytes, .. })) => bytes,
            Ok(Some(_)) => {
                return Err(RunRecordError::Invalid(
                    "events.jsonl is not a regular file".into(),
                ))
            }
            Err(error) => return Err(RunRecordError::Io(error)),
        };
        let after = after.unwrap_or(0);
        let (lines, _) = complete_lines(&bytes);
        let mut out = Vec::new();
        for (index, line) in lines.enumerate() {
            let seq = index as u64 + 1;
            if seq <= after {
                continue;
            }
            if out.len() >= limit {
                break;
            }
            out.push((seq, serde_json::from_slice(line)?));
        }
        Ok(out)
    }

    /// Write `outcome.json` once; a second write with other bytes is refused,
    /// the same bytes again are accepted.
    pub fn finish(&self, run_id: &str, outcome: &RunOutcome) -> Result<(), RunRecordError> {
        let dir = self.run_dir(run_id)?;
        if outcome.run_id != run_id {
            return Err(RunRecordError::Invalid(format!(
                "the Outcome names run {}, not {run_id}",
                outcome.run_id
            )));
        }
        let bytes = serde_json::to_vec_pretty(outcome)?;
        if bytes.len() > MAX_OUTCOME_BYTES {
            return Err(RunRecordError::Invalid(format!(
                "the Outcome is larger than {MAX_OUTCOME_BYTES} bytes"
            )));
        }
        let _appends = self.appends();
        if dir.has_exact_file(OUTCOME_FILE)? {
            let existing = dir.read_limited(OUTCOME_FILE, MAX_OUTCOME_BYTES)?;
            if existing == bytes {
                return Ok(());
            }
            return Err(RunRecordError::Invalid(format!(
                "run {run_id} has ended already; its Outcome is written once"
            )));
        }
        dir.atomic_write(OUTCOME_FILE, &bytes)?;
        dir.sync_all()?;
        Ok(())
    }

    pub fn outcome(&self, run_id: &str) -> Result<Option<RunOutcome>, RunRecordError> {
        let dir = self.run_dir(run_id)?;
        if !dir.has_exact_file(OUTCOME_FILE)? {
            return Ok(None);
        }
        let bytes = dir.read_limited(OUTCOME_FILE, MAX_OUTCOME_BYTES)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Whether the run has its Outcome.
    pub fn is_finished(&self, run_id: &str) -> Result<bool, RunRecordError> {
        Ok(self.run_dir(run_id)?.has_exact_file(OUTCOME_FILE)?)
    }

    /// Run ids, newest first (by the manifest's start time, then id). A
    /// directory that is not a run is left out.
    pub fn list(&self) -> Result<Vec<String>, RunRecordError> {
        let mut runs = Vec::new();
        for entry in self.root.entries_limited(MAX_LISTED_RUNS)? {
            if entry.file_type != axocoatl_core::SecureEntryType::Directory {
                continue;
            }
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            if !is_run_id(name) {
                continue;
            }
            let started = self
                .manifest(name)
                .map(|manifest| manifest.started_at_ms)
                .unwrap_or(0);
            runs.push((started, name.to_string()));
        }
        runs.sort_by(|left, right| right.cmp(left));
        Ok(runs.into_iter().map(|(_, id)| id).collect())
    }

    /// Runs with a manifest and no Outcome: at startup these were cut off by
    /// a daemon restart.
    pub fn unfinished(&self) -> Result<Vec<String>, RunRecordError> {
        let mut out = Vec::new();
        for run_id in self.list()? {
            if !self.is_finished(&run_id)? {
                out.push(run_id);
            }
        }
        Ok(out)
    }
}

use axocoatl_core::SecureLeaf as SecureLeafBytes;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::run_outcome::{exit_code, NetworkSummary, RunUsage, RunVerdict, RUN_OUTCOME_SCHEMA};

    fn run_id() -> String {
        format!("run-{}", uuid::Uuid::new_v4())
    }

    fn loadout_ref() -> LoadoutRef {
        LoadoutRef {
            id: "mine".into(),
            version: 1,
            kind: "custom".into(),
            digest: "a".repeat(64),
            builtin: false,
        }
    }

    fn manifest(run_id: &str) -> RunManifest {
        RunManifest {
            schema: RUN_MANIFEST_SCHEMA.into(),
            run_id: run_id.into(),
            session_id: format!("ses-{}", uuid::Uuid::new_v4()),
            workspace_id: "wsp-1".into(),
            loadout: loadout_ref(),
            loadout_text: "schema: axocoatl.loadout/1\n".into(),
            params: BTreeMap::new(),
            task: "fix it".into(),
            repo: "/repo".into(),
            repo_head: Some("0".repeat(40)),
            dirty_paths: vec!["src/a.rs".into()],
            started_at_ms: 10,
            options: serde_json::json!({"keep": "none"}),
        }
    }

    fn outcome(manifest: &RunManifest) -> RunOutcome {
        RunOutcome {
            schema: RUN_OUTCOME_SCHEMA.into(),
            run_id: manifest.run_id.clone(),
            session_id: manifest.session_id.clone(),
            workspace_id: manifest.workspace_id.clone(),
            loadout: manifest.loadout.clone(),
            task: manifest.task.clone(),
            started_at_ms: 10,
            finished_at_ms: 20,
            verdict: RunVerdict::Pass,
            exit_code: exit_code::PASS,
            attention: Vec::new(),
            turns: Vec::new(),
            checks: Vec::new(),
            review: None,
            adjudications: Vec::new(),
            findings: Vec::new(),
            not_covered: Vec::new(),
            unreadable_findings: Vec::new(),
            notes: Vec::new(),
            warnings: Vec::new(),
            usage: RunUsage::default(),
            network: NetworkSummary::default(),
            keep: None,
            error: None,
        }
    }

    fn phase(at_ms: u64, phase: &str) -> RunEvent {
        RunEvent::Phase {
            at_ms,
            phase: phase.into(),
            detail: String::new(),
        }
    }

    fn store() -> (tempfile::TempDir, SecureDir, RunRecordStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = SecureDir::open(dir.path()).unwrap();
        let store = RunRecordStore::open(&root).unwrap();
        (dir, root, store)
    }

    #[test]
    fn create_append_finish_round_trip() {
        let (_dir, _root, store) = store();
        let id = run_id();
        let manifest = manifest(&id);
        store.create(&manifest).unwrap();
        assert_eq!(store.manifest(&id).unwrap(), manifest);
        assert!(
            store.create(&manifest).is_err(),
            "a manifest is written once"
        );
        assert_eq!(store.append(&id, &phase(11, "preparing")).unwrap(), 1);
        assert_eq!(store.append(&id, &phase(12, "running")).unwrap(), 2);
        let events = store.events(&id, None, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].0, 2);
        assert_eq!(store.events(&id, Some(1), 10).unwrap().len(), 1);
        assert_eq!(store.events(&id, Some(0), 1).unwrap().len(), 1);
        assert!(!store.is_finished(&id).unwrap());
        assert_eq!(store.unfinished().unwrap(), vec![id.clone()]);
        let done = outcome(&manifest);
        store.finish(&id, &done).unwrap();
        assert_eq!(store.outcome(&id).unwrap(), Some(done.clone()));
        assert!(store.unfinished().unwrap().is_empty());
        assert_eq!(store.list().unwrap(), vec![id.clone()]);
        // A reopened store reads the same record.
        let again = RunRecordStore::open(&_root).unwrap();
        assert_eq!(again.events(&id, None, 10).unwrap().len(), 2);
    }

    #[test]
    fn the_outcome_is_written_once() {
        let (_dir, _root, store) = store();
        let id = run_id();
        let manifest = manifest(&id);
        store.create(&manifest).unwrap();
        let done = outcome(&manifest);
        store.finish(&id, &done).unwrap();
        store.finish(&id, &done).unwrap();
        let mut other = done.clone();
        other.exit_code = exit_code::NEEDS_ATTENTION;
        other.verdict = RunVerdict::NeedsAttention;
        assert!(store.finish(&id, &other).is_err());
        assert_eq!(store.outcome(&id).unwrap(), Some(done.clone()));
        assert!(
            store.append(&id, &phase(30, "late")).is_err(),
            "an ended run takes no more events"
        );
        // Only a Keep result is recorded after the end, and only through
        // append_after_end; the Outcome stays as written.
        assert!(store.append(&id, &phase(31, KEEP_PHASE)).is_err());
        assert!(store.append_after_end(&id, &phase(32, "late")).is_err());
        let before = store.events(&id, None, 10).unwrap().len();
        let seq = store.append_after_end(&id, &phase(33, KEEP_PHASE)).unwrap();
        assert_eq!(seq as usize, before + 1);
        assert!(
            matches!(&store.events(&id, None, 10).unwrap().last().unwrap().1,
            RunEvent::Phase { phase, .. } if phase == KEEP_PHASE)
        );
        assert_eq!(store.outcome(&id).unwrap(), Some(done));
    }

    #[test]
    fn a_torn_last_line_is_ignored_and_cut_before_the_next_append() {
        let (dir, _root, store) = store();
        let id = run_id();
        store.create(&manifest(&id)).unwrap();
        store.append(&id, &phase(11, "preparing")).unwrap();
        let events = dir.path().join(RUN_RECORD_DIR).join(&id).join(EVENTS_FILE);
        let mut bytes = std::fs::read(&events).unwrap();
        bytes.extend_from_slice(b"{\"kind\":\"phase\",\"at_ms\":12,\"pha");
        std::fs::write(&events, &bytes).unwrap();
        let reopened = RunRecordStore::open(&_root).unwrap();
        assert_eq!(reopened.events(&id, None, 10).unwrap().len(), 1);
        assert_eq!(reopened.append(&id, &phase(13, "running")).unwrap(), 2);
        let events = reopened.events(&id, None, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1].1, RunEvent::Phase { phase, .. } if phase == "running"));
    }

    #[test]
    fn symlinks_are_refused() {
        let (dir, _root, store) = store();
        let outside = tempfile::tempdir().unwrap();
        let id = run_id();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(RUN_RECORD_DIR).join(&id))
            .unwrap();
        assert!(store.manifest(&id).is_err());
        assert!(store.append(&id, &phase(1, "x")).is_err());
        let id = run_id();
        store.create(&manifest(&id)).unwrap();
        let run_dir = dir.path().join(RUN_RECORD_DIR).join(&id);
        std::os::unix::fs::symlink(outside.path().join("events"), run_dir.join(EVENTS_FILE))
            .unwrap();
        assert!(store.append(&id, &phase(1, "x")).is_err());
        assert!(store.events(&id, None, 10).is_err());
        assert!(!outside.path().join("events").exists());
    }

    #[test]
    fn ids_and_bounds_are_checked() {
        let (_dir, _root, store) = store();
        assert!(matches!(
            store.manifest("../etc"),
            Err(RunRecordError::Invalid(_))
        ));
        assert!(matches!(
            store.manifest(&run_id()),
            Err(RunRecordError::NotFound(_))
        ));
        let id = run_id();
        store.create(&manifest(&id)).unwrap();
        let huge = RunEvent::Phase {
            at_ms: 1,
            phase: "x".into(),
            detail: "y".repeat(MAX_RUN_EVENT_BYTES),
        };
        assert!(store.append(&id, &huge).is_err());
    }
}
