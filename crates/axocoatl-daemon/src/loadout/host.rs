//! The run driver's view of the daemon. Owner: core (trait); the server's
//! `DaemonRunHost` implements it, delegating each method to the daemon
//! method named in its doc comment (whose file belongs to the workstream
//! named there).

use std::time::Instant;

use async_trait::async_trait;
use axocoatl_session::run_outcome::{NetworkSummary, ReproRun, RunOutcome, TurnObservation};
use axocoatl_session::run_record::RunEvent;
use serde::{Deserialize, Serialize};

use super::RunError;
use crate::SessionTeamEdit;

/// One reproduction run (workstream review-qa).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproRequest {
    /// Repository path of the Playwright test, under the loadout's
    /// `repro_dir`.
    pub path: String,
    /// `base_url` for `browser_check`.
    pub base_url: String,
    pub timeout_ms: u64,
}

/// How a required check is named in the Outcome: its exact argv as applied,
/// its loadout name and its timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckLabel {
    pub argv: Vec<String>,
    pub name: String,
    pub timeout_ms: u64,
}

impl CheckLabel {
    /// The labels of an applied edit's checks, aligned by index; a check
    /// without options is `check-<n>` with the default timeout.
    pub fn of_edit(edit: &SessionTeamEdit) -> Vec<CheckLabel> {
        edit.required_checks
            .iter()
            .enumerate()
            .map(|(index, argv)| {
                let options = edit.check_options.get(index);
                CheckLabel {
                    argv: argv.clone(),
                    name: options
                        .and_then(|options| options.name.clone())
                        .unwrap_or_else(|| format!("check-{}", index + 1)),
                    timeout_ms: options.map_or(
                        axocoatl_session::check_options::DEFAULT_CHECK_TIMEOUT_MS,
                        |options| options.timeout_ms(),
                    ),
                }
            })
            .collect()
    }
}

/// One tool call of a turn as the Session recorded it: which node and
/// generation made it, the tool, its arguments as the Agent gave them,
/// whether it succeeded and what it returned (the audit judges from these
/// whether an area worker examined its area).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub node_id: String,
    pub generation: u32,
    pub tool: String,
    /// The call's arguments; `null` when they could not be read.
    pub arguments: serde_json::Value,
    /// The call has an outcome and it succeeded.
    pub succeeded: bool,
    /// The value the tool returned, when the call succeeded and the Session
    /// kept its whole result; `null` otherwise (a result cut to its
    /// reserved bytes is never read as a whole one).
    #[serde(default)]
    pub result: serde_json::Value,
}

#[async_trait]
pub trait RunHost: Send + Sync {
    /// Preview and apply a Team and budget edit for future turns. The run
    /// owns its Session, so the host applies the edit on the Session's
    /// current configuration revision whatever
    /// `edit.expected_configuration_revision` says.
    /// Daemon: `preview_session_team` + `apply_session_team` (core).
    async fn apply_team(&self, session_id: &str, edit: SessionTeamEdit) -> Result<(), RunError>;

    /// Start one turn with `request` as the person's message; returns its
    /// turn id. Daemon: native Send (core).
    async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError>;

    /// Wait until the turn ends (completed, needs attention, failed, stopped
    /// or interrupted) or `deadline` passes, then observe it.
    /// Daemon: turn and control-plane projections (core).
    async fn wait_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        deadline: Instant,
    ) -> Result<TurnObservation, RunError>;

    /// Stop the turn. Daemon: `session-stop` (core).
    async fn stop_turn(&self, session_id: &str, turn_id: &str) -> Result<(), RunError>;

    /// Run one reproduction with `browser_check` in the browser container.
    /// Daemon: `run_repro_check` in `bootstrap_qa_repro.rs` (review-qa).
    async fn run_repro(
        &self,
        session_id: &str,
        request: &ReproRequest,
    ) -> Result<ReproRun, RunError>;

    /// Read a file from the Session container, at most `max_bytes`; `None`
    /// when it does not exist. Daemon: Ready sandbox handle (core).
    async fn read_sandbox_file(
        &self,
        session_id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError>;

    /// Append one event to the run record. Daemon: `RunRecordStore` (core).
    async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError>;

    /// The events recorded for the run so far (to count provider retries).
    /// Daemon: `RunRecordStore` (core). A host without a record has none.
    async fn recorded_events(&self, _run_id: &str) -> Result<Vec<RunEvent>, RunError> {
        Ok(Vec::new())
    }

    /// Every tool call the Session recorded for `turn_id`, from its
    /// invocation audit, with the arguments and results; `None` when this host keeps no
    /// such record. Daemon: `loadout_turn_tool_calls` (audit).
    async fn tool_calls(
        &self,
        _session_id: &str,
        _turn_id: &str,
    ) -> Result<Option<Vec<ToolCallRecord>>, RunError> {
        Ok(None)
    }

    /// What the Session's network record holds. Daemon: the Session's
    /// network record (core). A host without one reports nothing.
    async fn network_summary(&self, _session_id: &str) -> Result<NetworkSummary, RunError> {
        Ok(NetworkSummary::default())
    }

    /// Whether a person asked to stop the run (`POST /api/runs/{id}/stop`,
    /// Ctrl-C in `axocoatl run`). Daemon: the server's run registry (core).
    async fn stop_requested(&self, _run_id: &str) -> bool {
        false
    }

    /// Write the run's Outcome once. Daemon: `RunRecordStore::finish`
    /// (core). The run driver records the `Ended` event after it.
    async fn finish(&self, _run_id: &str, _outcome: &RunOutcome) -> Result<(), RunError> {
        Ok(())
    }
}
