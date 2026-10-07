//! Loadouts and loadout runs on the live daemon: the registry of built-in
//! and user loadouts, run admission (Workspace, Session with the loadout's
//! sandbox binding, environment approval), the run record, JUnit and the
//! record bundle. Owner: workstream `core`.
use super::*;
use crate::loadout::api::{
    LoadoutSummary, LoadoutView, RunAccepted, RunEventsPage, RunRequest, RunStatusView,
    ValidateLoadoutRequest, ValidateLoadoutResponse,
};

impl AxocoatlDaemon {
    /// `GET /api/loadouts`: built-in loadouts, then user loadouts from
    /// `<config dir>/loadouts/` (invalid ones listed with their error).
    pub async fn list_loadouts(&self) -> Result<Vec<LoadoutSummary>, DaemonError> {
        Err(DaemonError::NotImplemented("list_loadouts"))
    }

    /// `GET /api/loadouts/{id}`.
    pub async fn loadout_view(&self, _id: &str) -> Result<LoadoutView, DaemonError> {
        Err(DaemonError::NotImplemented("loadout_view"))
    }

    /// `POST /api/loadouts/validate`.
    pub async fn validate_loadout_text(
        &self,
        _request: ValidateLoadoutRequest,
    ) -> Result<ValidateLoadoutResponse, DaemonError> {
        Err(DaemonError::NotImplemented("validate_loadout_text"))
    }

    /// `POST /api/runs`: resolve the loadout, authorize the repository as a
    /// Workspace, create the Session bound to the loadout, approve exactly
    /// the loadout's environment, write the run manifest. The server then
    /// starts the driver task.
    pub async fn admit_loadout_run(
        &self,
        _request: RunRequest,
    ) -> Result<(RunAccepted, crate::loadout::RunContext), DaemonError> {
        Err(DaemonError::NotImplemented("admit_loadout_run"))
    }

    /// `GET /api/runs`.
    pub async fn list_loadout_runs(&self) -> Result<Vec<RunStatusView>, DaemonError> {
        Err(DaemonError::NotImplemented("list_loadout_runs"))
    }

    /// `GET /api/runs/{run_id}`.
    pub async fn loadout_run(&self, _run_id: &str) -> Result<RunStatusView, DaemonError> {
        Err(DaemonError::NotImplemented("loadout_run"))
    }

    /// `GET /api/runs/{run_id}/events`.
    pub async fn loadout_run_events(
        &self,
        _run_id: &str,
        _after: Option<u64>,
        _limit: usize,
    ) -> Result<RunEventsPage, DaemonError> {
        Err(DaemonError::NotImplemented("loadout_run_events"))
    }

    /// `GET /api/runs/{run_id}/junit`.
    pub async fn loadout_run_junit(&self, _run_id: &str) -> Result<String, DaemonError> {
        Err(DaemonError::NotImplemented("loadout_run_junit"))
    }

    /// `GET /api/runs/{run_id}/record`: the record bundle, written to `out`.
    pub async fn write_record_bundle(
        &self,
        _run_id: &str,
        _out: &mut (dyn std::io::Write + Send),
    ) -> Result<(), DaemonError> {
        Err(DaemonError::NotImplemented("write_record_bundle"))
    }
}
