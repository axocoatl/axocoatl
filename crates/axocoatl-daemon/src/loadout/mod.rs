//! Loadout runs: turn a resolved loadout into a Session, a Team and budget
//! edit and one or more turns, then build the run's Outcome.
//!
//! The run driver talks to the daemon only through [`host::RunHost`], so each
//! kind's driver is testable with a fake host. The server implements
//! `RunHost` over the live daemon (`axocoatl-server/src/routes_loadouts.rs`).
//!
//! Ownership (docs/design/1.3-loadouts.md, "Ownership"):
//! `mod.rs`, `api.rs`, `host.rs`, `driver.rs`, `team_plan.rs`, `egress.rs`
//! — core; `fix.rs`, `qa.rs` — review-qa; `audit.rs` — audit; `e2e.rs` —
//! e2e.

pub mod api;
pub mod audit;
pub mod driver;
pub mod e2e;
pub mod egress;
pub mod fix;
pub mod host;
pub mod qa;
pub mod team_plan;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use axocoatl_config::loadout::{LoadoutKind, ParamValues, ResolvedLoadout};
use axocoatl_session::run_outcome::{
    Adjudication, Finding, NotCovered, RunTurnRef, RunWarning, TurnObservation,
};
use serde::{Deserialize, Serialize};

pub use host::{RunHost, ToolCallRecord};

/// Why a run step failed.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// A hook its workstream has not filled in yet.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    /// Bad request: unknown loadout, missing parameter, invalid flag.
    #[error("{0}")]
    Usage(String),
    /// The run could not execute (environment, sandbox, provider setup).
    #[error("{0}")]
    Infrastructure(String),
    /// The wall clock ran out.
    #[error("the run's wall clock ran out")]
    Deadline,
    /// A person stopped the run.
    #[error("the run was stopped")]
    Stopped,
    /// Another run, Session turn or operation holds the run's Workspace, so
    /// a turn could not start (exit code 7). The text says who holds it
    /// when that is known.
    #[error("Workspace busy: {0}")]
    Busy(String),
}

impl From<crate::DaemonError> for RunError {
    fn from(error: crate::DaemonError) -> Self {
        match error {
            crate::DaemonError::NotImplemented(what) => RunError::NotImplemented(what),
            crate::DaemonError::InvalidRequest(message) => RunError::Usage(message),
            crate::DaemonError::WorkspaceBusy(detail) => RunError::Busy(detail),
            other => RunError::Infrastructure(other.to_string()),
        }
    }
}

/// What Keep does when a run passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepMode {
    /// Leave the changes in the working tree (the default).
    #[default]
    None,
    /// Create a branch and a commit; push nothing.
    Branch,
    /// Create a branch and a commit, push the branch and open a PR.
    Pr,
}

/// Options of one run, as the request gave them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOptions {
    pub task: String,
    /// Canonical repository path.
    pub repo: PathBuf,
    #[serde(default)]
    pub params: ParamValues,
    #[serde(default)]
    pub keep: KeepMode,
    /// `--check`: the command for a `detected` check.
    #[serde(default)]
    pub check_command: Option<String>,
    /// `--setup`: the exact setup command this run approves.
    #[serde(default)]
    pub setup_command: Option<String>,
}

/// One run in progress.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: String,
    pub resolved: ResolvedLoadout,
    pub options: RunOptions,
    pub deadline: Instant,
    /// Each native Agent's model context in tokens as admission observed
    /// it, by Agent id ([`team_plan::agent_contexts`]); an Agent whose
    /// context was not observed is missing.
    pub agent_contexts: BTreeMap<String, u64>,
}

/// What a kind's driver hands back to the Outcome builder.
#[derive(Debug, Clone, Default)]
pub struct KindReport {
    /// Every turn the driver started, in order, as observed when it ended.
    pub turns: Vec<TurnObservation>,
    pub turn_refs: Vec<RunTurnRef>,
    pub findings: Vec<Finding>,
    pub adjudications: Vec<Adjudication>,
    pub not_covered: Vec<NotCovered>,
    /// Notes for the Outcome: what is neither a gap nor a warning.
    pub notes: Vec<String>,
    pub warnings: Vec<RunWarning>,
    /// Nodes, as `(turn id, node id)`, whose missing result the kind driver
    /// accounted for itself, such as a planner it gave another turn: the
    /// Outcome builder does not list them as not covered again.
    pub accounted: Vec<(String, String)>,
    pub fail_on_findings: bool,
    pub budget_exhausted: bool,
    /// A person stopped the run; the report holds what the driver observed
    /// until then, and the Outcome is `interrupted`.
    pub stopped: bool,
}

/// One kind of loadout run.
#[async_trait]
pub trait KindDriver: Send + Sync {
    async fn drive(&self, host: &dyn RunHost, run: &RunContext) -> Result<KindReport, RunError>;
}

/// The driver of `kind`.
pub fn driver_for(kind: LoadoutKind) -> Box<dyn KindDriver> {
    match kind {
        LoadoutKind::Fix => Box::new(fix::FixDriver),
        LoadoutKind::Qa => Box::new(qa::QaDriver),
        LoadoutKind::Audit => Box::new(audit::AuditDriver),
        LoadoutKind::Custom => Box::new(driver::SingleTurnDriver),
    }
}
