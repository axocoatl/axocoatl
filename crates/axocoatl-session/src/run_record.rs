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

/// The run records under one data root.
pub struct RunRecordStore {
    _root: SecureDir,
}

impl RunRecordStore {
    /// Open (creating when missing) `{data root}/loadout-runs/`.
    pub fn open(_data_root: &SecureDir) -> Result<Self, RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::open"))
    }

    /// Create the run's directory and write its manifest once.
    pub fn create(&self, _manifest: &RunManifest) -> Result<(), RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::create"))
    }

    /// Append one event and sync it before returning.
    pub fn append(&self, _run_id: &str, _event: &RunEvent) -> Result<u64, RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::append"))
    }

    pub fn manifest(&self, _run_id: &str) -> Result<RunManifest, RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::manifest"))
    }

    /// Events after sequence number `after`, at most `limit`.
    pub fn events(
        &self,
        _run_id: &str,
        _after: Option<u64>,
        _limit: usize,
    ) -> Result<Vec<(u64, RunEvent)>, RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::events"))
    }

    /// Write `outcome.json` once; a second write with other bytes is refused.
    pub fn finish(&self, _run_id: &str, _outcome: &RunOutcome) -> Result<(), RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::finish"))
    }

    pub fn outcome(&self, _run_id: &str) -> Result<Option<RunOutcome>, RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::outcome"))
    }

    /// Run ids, newest first.
    pub fn list(&self) -> Result<Vec<String>, RunRecordError> {
        Err(RunRecordError::NotImplemented("RunRecordStore::list"))
    }
}
