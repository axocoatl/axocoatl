//! The run driver's view of the daemon. Owner: core (trait); the server's
//! `DaemonRunHost` implements it, delegating each method to the daemon
//! method named in its doc comment (whose file belongs to the workstream
//! named there).

use std::time::Instant;

use async_trait::async_trait;
use axocoatl_session::run_outcome::{ReproRun, TurnObservation};
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

#[async_trait]
pub trait RunHost: Send + Sync {
    /// Preview and apply a Team and budget edit for future turns.
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
}
