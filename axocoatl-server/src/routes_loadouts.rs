//! Loadout and run endpoints, and the run driver's host over the live
//! daemon. Owner: workstream `core`. See docs/design/1.3-loadouts.md
//! ("API").
use super::*;
use axocoatl_daemon::loadout::api::{
    LoadoutSummary, LoadoutView, RunAccepted, RunEventsPage, RunRequest, RunStatusView,
    ValidateLoadoutRequest, ValidateLoadoutResponse,
};
use axocoatl_daemon::loadout::host::ReproRequest;
use axocoatl_daemon::loadout::{RunError, RunHost};
use axocoatl_session::run_outcome::{ReproRun, TurnObservation};
use axocoatl_session::run_record::RunEvent;

type RouteError = (StatusCode, Json<ErrorResponse>);

pub async fn list_loadouts(
    State(state): State<AppState>,
) -> Result<Json<Vec<LoadoutSummary>>, RouteError> {
    state
        .read()
        .await
        .list_loadouts()
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn get_loadout(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LoadoutView>, RouteError> {
    state
        .read()
        .await
        .loadout_view(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn validate_loadout(
    State(state): State<AppState>,
    Json(request): Json<ValidateLoadoutRequest>,
) -> Result<Json<ValidateLoadoutResponse>, RouteError> {
    state
        .read()
        .await
        .validate_loadout_text(request)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// `POST /api/runs`: admit the run, start its driver task, return 202.
pub async fn start_loadout_run(
    State(state): State<AppState>,
    Json(request): Json<RunRequest>,
) -> Result<(StatusCode, Json<RunAccepted>), RouteError> {
    let (accepted, _context) = state
        .read()
        .await
        .admit_loadout_run(request)
        .await
        .map_err(attempt_err)?;
    Ok((StatusCode::ACCEPTED, Json(accepted)))
}

pub async fn list_loadout_runs(
    State(state): State<AppState>,
) -> Result<Json<Vec<RunStatusView>>, RouteError> {
    state
        .read()
        .await
        .list_loadout_runs()
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn get_loadout_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Json<RunStatusView>, RouteError> {
    state
        .read()
        .await
        .loadout_run(&run_id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(Deserialize)]
pub struct RunEventsQuery {
    pub after: Option<u64>,
    pub limit: Option<usize>,
}

pub async fn loadout_run_events_route(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(query): Query<RunEventsQuery>,
) -> Result<Json<RunEventsPage>, RouteError> {
    state
        .read()
        .await
        .loadout_run_events(&run_id, query.after, query.limit.unwrap_or(200))
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn stop_loadout_run(
    State(_state): State<AppState>,
    Path(_run_id): Path<String>,
) -> Result<StatusCode, RouteError> {
    Err(attempt_err(axocoatl_daemon::DaemonError::NotImplemented(
        "stop_loadout_run",
    )))
}

pub async fn loadout_run_junit_route(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<([(axum::http::header::HeaderName, &'static str); 1], String), RouteError> {
    let xml = state
        .read()
        .await
        .loadout_run_junit(&run_id)
        .await
        .map_err(attempt_err)?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/xml")], xml))
}

pub async fn loadout_run_record_route(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<([(axum::http::header::HeaderName, &'static str); 1], Vec<u8>), RouteError> {
    let mut bytes = Vec::new();
    state
        .read()
        .await
        .write_record_bundle(&run_id, &mut bytes)
        .await
        .map_err(attempt_err)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            axocoatl_session::record_bundle::RECORD_BUNDLE_MEDIA_TYPE,
        )],
        bytes,
    ))
}

/// The run driver's host over the live daemon. Each method takes the
/// daemon's read lock only for the call it makes, never across a wait.
#[derive(Clone)]
pub struct DaemonRunHost {
    pub state: AppState,
}

#[async_trait::async_trait]
impl RunHost for DaemonRunHost {
    async fn apply_team(
        &self,
        _session_id: &str,
        _edit: axocoatl_daemon::SessionTeamEdit,
    ) -> Result<(), RunError> {
        Err(RunError::NotImplemented("DaemonRunHost::apply_team"))
    }

    async fn send_turn(&self, _session_id: &str, _request: &str) -> Result<String, RunError> {
        Err(RunError::NotImplemented("DaemonRunHost::send_turn"))
    }

    async fn wait_turn(
        &self,
        _session_id: &str,
        _turn_id: &str,
        _deadline: std::time::Instant,
    ) -> Result<TurnObservation, RunError> {
        Err(RunError::NotImplemented("DaemonRunHost::wait_turn"))
    }

    async fn stop_turn(&self, _session_id: &str, _turn_id: &str) -> Result<(), RunError> {
        Err(RunError::NotImplemented("DaemonRunHost::stop_turn"))
    }

    async fn run_repro(
        &self,
        session_id: &str,
        request: &ReproRequest,
    ) -> Result<ReproRun, RunError> {
        Ok(self
            .state
            .read()
            .await
            .run_repro_check(session_id, request)
            .await?)
    }

    async fn read_sandbox_file(
        &self,
        _session_id: &str,
        _path: &str,
        _max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError> {
        Err(RunError::NotImplemented("DaemonRunHost::read_sandbox_file"))
    }

    async fn record(&self, _run_id: &str, _event: RunEvent) -> Result<(), RunError> {
        Err(RunError::NotImplemented("DaemonRunHost::record"))
    }
}
