//! Request and response bodies of the loadout and run endpoints
//! (docs/design/1.3-loadouts.md, "API"). Owner: core.

use axocoatl_config::loadout::{LoadoutFile, LoadoutWarning};
use axocoatl_session::run_outcome::RunOutcome;
use axocoatl_session::run_record::RunEvent;
use serde::{Deserialize, Serialize};

use super::KeepMode;

/// One row of `GET /api/loadouts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadoutSummary {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub builtin: bool,
    pub opt_in: bool,
    pub digest: String,
    /// Declared parameters: name, kind, required, default.
    pub params: Vec<LoadoutParamView>,
    pub warnings: Vec<LoadoutWarning>,
    /// For a user loadout that cannot be used: why. Such rows have empty
    /// `version`, `kind` and `digest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For a user loadout: its file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutParamView {
    pub name: String,
    pub kind: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub description: String,
}

/// `GET /api/loadouts/{id}`: the file and a display graph. The lattice only
/// displays this graph; nothing in it is executable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadoutView {
    pub summary: LoadoutSummary,
    pub file: LoadoutFile,
    /// The exact file text.
    pub text: String,
    pub graph: LoadoutGraph,
}

/// Nodes: one per Agent, one per check, one for the reviewer, one for
/// "area workers" in an audit. Edges: dependencies and the host's order
/// (Agents → checks → review).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutGraph {
    pub nodes: Vec<LoadoutGraphNode>,
    pub edges: Vec<LoadoutGraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutGraphNode {
    pub id: String,
    /// `agent`, `check`, `review` or `area_workers`.
    pub kind: String,
    pub label: String,
    /// Model, runtime, timeout, rounds: short facts shown on the node.
    pub detail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutGraphEdge {
    pub from: String,
    pub to: String,
}

/// `POST /api/loadouts/validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateLoadoutRequest {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidateLoadoutResponse {
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub warnings: Vec<LoadoutWarning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<LoadoutSummary>,
}

/// `POST /api/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub loadout: String,
    pub task: String,
    /// Repository directory; canonicalized and authorized as a Workspace.
    pub repo: String,
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub keep: KeepMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
    /// Idempotency key: a repeat with the same key returns the same run.
    pub request_id: String,
}

/// `202 Accepted` body of `POST /api/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAccepted {
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub warnings: Vec<axocoatl_session::run_outcome::RunWarning>,
}

/// `GET /api/runs/{run_id}` and rows of `GET /api/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStatusView {
    pub run_id: String,
    pub session_id: String,
    pub loadout: String,
    /// `preparing`, `running`, `finished` or `failed`.
    pub state: String,
    /// The latest phase, in words.
    pub phase: String,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// The latest Keep of this run (`POST /api/sessions/{id}/keep-pr`),
    /// from the run record: the Outcome itself is written once, before any
    /// Keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<axocoatl_session::run_outcome::KeepResult>,
}

/// `GET /api/runs/{run_id}/events?after=&limit=`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventsPage {
    pub events: Vec<(u64, RunEvent)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<u64>,
    pub finished: bool,
}
