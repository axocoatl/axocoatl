//! Loadout and run endpoints, and the run driver's host over the live
//! daemon. Owner: workstream `core`. See docs/design/1.3-loadouts.md
//! ("API").
//!
//! Every handler takes the daemon's read lock only for the call it makes:
//! the run's events are long-polled with the lock released between reads,
//! and the record bundle is streamed one bounded step at a time.
use super::*;
use axocoatl_daemon::bootstrap::loadout_runs::BundleCursor;
use axocoatl_daemon::loadout::api::{
    LoadoutSummary, LoadoutView, RunAccepted, RunEventsPage, RunRequest, RunStatusView,
    ValidateLoadoutRequest, ValidateLoadoutResponse,
};
use axocoatl_daemon::loadout::host::{CheckLabel, ReproRequest};
use axocoatl_daemon::loadout::{RunContext, RunError, RunHost};
use axocoatl_session::run_outcome::{
    ModelIdentity, NetworkSummary, ReproRun, RunOutcome, TurnObservation, TurnState,
};
use axocoatl_session::run_record::RunEvent;

type RouteError = (StatusCode, Json<ErrorResponse>);

/// How long `GET /api/runs/{id}/events` waits for a new event.
const EVENTS_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
/// How often a waiting read looks again.
const POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(100);
/// How often the driver's host observes a running turn.
const TURN_POLL: std::time::Duration = std::time::Duration::from_millis(500);

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

/// End a run whose driver could not record its Outcome: the run fails with
/// the reason, never left unfinished.
async fn fail_run(host: &DaemonRunHost, context: &RunContext, reason: String) {
    let mut outcome = axocoatl_daemon::loadout::driver::empty_outcome(
        context,
        axocoatl_daemon::loadout::driver::now_ms(),
    );
    outcome.error = Some(reason);
    outcome.finished_at_ms = axocoatl_daemon::loadout::driver::now_ms();
    outcome.decide(Default::default());
    let _ = host
        .record(
            &context.run_id,
            RunEvent::Ended {
                at_ms: outcome.finished_at_ms,
                outcome: Box::new(outcome.clone()),
            },
        )
        .await;
    let _ = host.finish(&context.run_id, &outcome).await;
}

/// Start the driver task of an admitted run, once.
pub fn spawn_run_driver(state: AppState, context: RunContext) {
    tokio::spawn(async move {
        let claimed = state.read().await.claim_loadout_run_driver(&context.run_id);
        if !claimed {
            return;
        }
        let host = DaemonRunHost::new(state, &context);
        if let Err(error) = axocoatl_daemon::loadout::driver::run_to_outcome(&host, &context).await
        {
            tracing::error!(run = %context.run_id, %error, "a loadout run could not record its Outcome");
            fail_run(&host, &context, error.to_string()).await;
        }
    });
}

/// `POST /api/runs`: admit the run, start its driver task, return 202. A
/// Workspace that another run or Session holds is a `409` with the code
/// `workspace_busy`, which `axocoatl run` exits 7 on.
pub async fn start_loadout_run(
    State(state): State<AppState>,
    Json(request): Json<RunRequest>,
) -> Result<(StatusCode, Json<RunAccepted>), CodedRouteError> {
    let (accepted, context) = state
        .read()
        .await
        .admit_loadout_run(request)
        .await
        .map_err(coded_err)?;
    spawn_run_driver(state.clone(), context);
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
    /// Milliseconds to wait for a new event, at most 5000 (default 1000).
    pub wait_ms: Option<u64>,
}

/// `GET /api/runs/{run_id}/events?after=&limit=`: a long poll. With nothing
/// new and the run unfinished it waits up to a second, reading again with
/// the daemon released between reads.
pub async fn loadout_run_events_route(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Query(query): Query<RunEventsQuery>,
) -> Result<Json<RunEventsPage>, RouteError> {
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let wait = query
        .wait_ms
        .map(|ms| std::time::Duration::from_millis(ms.min(5_000)))
        .unwrap_or(EVENTS_WAIT);
    let started = tokio::time::Instant::now();
    loop {
        let page = state
            .read()
            .await
            .loadout_run_events(&run_id, query.after, limit)
            .await
            .map_err(attempt_err)?;
        if !page.events.is_empty() || page.finished || started.elapsed() >= wait {
            return Ok(Json(page));
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// `POST /api/runs/{run_id}/stop`: stop the run's turn; the driver ends the
/// run as interrupted.
pub async fn stop_loadout_run(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<StatusCode, RouteError> {
    state
        .read()
        .await
        .request_loadout_run_stop(&run_id)
        .await
        .map_err(attempt_err)?;
    Ok(StatusCode::ACCEPTED)
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

/// `GET /api/runs/{run_id}/record`: the record bundle, streamed. Each step
/// takes the daemon's read lock for one bounded read.
pub async fn loadout_run_record_route(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, RouteError> {
    use axocoatl_session::record_bundle::{BundleWriter, RECORD_BUNDLE_MEDIA_TYPE};
    let header = state
        .read()
        .await
        .record_bundle_header(&run_id)
        .map_err(attempt_err)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let filename = format!(
        "{}.{}",
        header.run_id,
        axocoatl_session::record_bundle::RECORD_BUNDLE_EXTENSION
    );
    tokio::spawn(async move {
        let fail = |error: String| std::io::Error::other(error);
        let mut writer = match BundleWriter::new(Vec::new(), &header) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = tx.send(Err(fail(error.to_string()))).await;
                return;
            }
        };
        let mut cursor = BundleCursor::Start;
        loop {
            let chunk = std::mem::take(writer.get_mut());
            if !chunk.is_empty() && tx.send(Ok(bytes::Bytes::from(chunk))).await.is_err() {
                return;
            }
            if cursor == BundleCursor::Done {
                break;
            }
            let step = state
                .read()
                .await
                .record_bundle_step(&run_id, cursor.clone())
                .await;
            let (sections, next) = match step {
                Ok(step) => step,
                Err(error) => {
                    let _ = tx.send(Err(fail(error.to_string()))).await;
                    return;
                }
            };
            for (section, data) in sections {
                if let Err(error) = writer.section(section, &data) {
                    let _ = tx.send(Err(fail(error.to_string()))).await;
                    return;
                }
            }
            cursor = next;
        }
        match writer.finish() {
            Ok(tail) => {
                let _ = tx.send(Ok(bytes::Bytes::from(tail))).await;
            }
            Err(error) => {
                let _ = tx.send(Err(fail(error.to_string()))).await;
            }
        }
    });
    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(RECORD_BUNDLE_MEDIA_TYPE),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// Why the task that sent one turn failed: the daemon's error, and its
/// detail when the error was a busy Workspace.
#[derive(Debug, Clone)]
struct SendFailure {
    message: String,
    busy: Option<String>,
}

impl SendFailure {
    fn of(error: &axocoatl_daemon::DaemonError) -> Self {
        Self {
            message: error.to_string(),
            busy: match error {
                axocoatl_daemon::DaemonError::WorkspaceBusy(detail) => Some(detail.clone()),
                _ => None,
            },
        }
    }

    /// The run's error when the turn never started: busy (exit 7) when
    /// another Session or operation held the Workspace, infrastructure
    /// otherwise.
    fn not_started(&self) -> RunError {
        match &self.busy {
            Some(detail) => RunError::Busy(format!("the turn could not start: {detail}")),
            None => RunError::Infrastructure(format!("the turn could not start: {}", self.message)),
        }
    }
}

/// The outcome of the task that sent one turn.
type SendResults =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Option<Result<(), SendFailure>>>>>;

/// The run driver's host over the live daemon. Each method takes the
/// daemon's read lock only for the call it makes, never across a wait.
#[derive(Clone)]
pub struct DaemonRunHost {
    pub state: AppState,
    /// The checks of the team the run applied last, to name them.
    checks: Arc<std::sync::Mutex<Vec<CheckLabel>>>,
    /// The loadout's reviewer, named when the turn's projection cannot.
    reviewer: Option<ModelIdentity>,
    sends: SendResults,
    /// The run's id, for the provider retries recorded when a turn ends.
    run_id: String,
    /// Turns whose provider retries are already in the run record.
    retries_recorded: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl DaemonRunHost {
    pub fn new(state: AppState, context: &RunContext) -> Self {
        Self {
            state,
            checks: Arc::new(std::sync::Mutex::new(Vec::new())),
            reviewer: context.resolved.reviewer_model.as_ref().map(|model| {
                axocoatl_daemon::loadout::team_plan::model_identity(
                    model,
                    axocoatl_config::loadout::AgentRuntime::Native,
                )
            }),
            sends: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            run_id: context.run_id.clone(),
            retries_recorded: Arc::new(std::sync::Mutex::new(Default::default())),
        }
    }

    /// Record the provider retries of a turn that ended, once per turn.
    async fn record_retries(&self, turn_id: &str, events: Vec<RunEvent>) -> Result<(), RunError> {
        let first = self
            .retries_recorded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(turn_id.to_string());
        if !first {
            return Ok(());
        }
        for event in events {
            self.record(&self.run_id, event).await?;
        }
        Ok(())
    }

    fn labels(&self) -> Vec<CheckLabel> {
        self.checks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn send_result(&self, turn_id: &str) -> Option<Result<(), SendFailure>> {
        self.sends
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(turn_id)
            .cloned()
            .flatten()
    }
}

fn daemon_error(error: axocoatl_daemon::DaemonError) -> RunError {
    error.into()
}

#[async_trait::async_trait]
impl RunHost for DaemonRunHost {
    async fn apply_team(
        &self,
        session_id: &str,
        edit: axocoatl_daemon::SessionTeamEdit,
    ) -> Result<(), RunError> {
        *self
            .checks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = CheckLabel::of_edit(&edit);
        self.state
            .read()
            .await
            .apply_loadout_team(session_id, edit)
            .await
            .map_err(daemon_error)
    }

    async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError> {
        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
        self.sends
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(turn_id.clone(), None);
        let state = self.state.clone();
        let sends = self.sends.clone();
        let (session, turn, input) = (session_id.to_string(), turn_id.clone(), request.to_string());
        tokio::spawn(async move {
            // The daemon is the sole publisher of the turn's stream frames;
            // this sink is disconnected, as for a turn sent over /ws.
            let (sink, receiver) =
                tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
            drop(receiver);
            let result = {
                let daemon = state.read().await;
                daemon
                    .execute_session_turn_streaming(
                        &session,
                        &turn,
                        Some(turn.clone()),
                        None,
                        &input,
                        Vec::new(),
                        Vec::new(),
                        None,
                        None,
                        sink,
                    )
                    .await
            };
            sends
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(
                    turn,
                    Some(result.map(|_| ()).map_err(|error| SendFailure::of(&error))),
                );
        });
        Ok(turn_id)
    }

    async fn wait_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        deadline: std::time::Instant,
    ) -> Result<TurnObservation, RunError> {
        loop {
            let labels = self.labels();
            let (observed, retries) = match self
                .state
                .read()
                .await
                .loadout_turn_observation_with_retries(
                    session_id,
                    turn_id,
                    &labels,
                    self.reviewer.as_ref(),
                )
                .await
                .map_err(daemon_error)?
            {
                Some((observation, retries)) => (Some(observation), retries),
                None => (None, Vec::new()),
            };
            let sent = self.send_result(turn_id);
            match (&observed, &sent) {
                (Some(observation), _) if observation.state != TurnState::Running => {
                    self.record_retries(turn_id, retries).await?;
                    return Ok(observation.clone());
                }
                // The turn never started: the send failed before the turn
                // had a record.
                (None, Some(Err(failure))) => return Err(failure.not_started()),
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return Ok(observed.unwrap_or(TurnObservation {
                    session_id: session_id.to_string(),
                    turn_id: turn_id.to_string(),
                    state: TurnState::Running,
                    attention_reason: Some("the turn has not started".into()),
                    nodes: Vec::new(),
                    checks: Vec::new(),
                    review: None,
                    usage: Default::default(),
                }));
            }
            if let (Some(observation), Some(Err(failure))) = (&observed, &sent) {
                // The send ended with an error while the projection still
                // says running: report it as the turn's end.
                let mut observation = observation.clone();
                observation.state = TurnState::Failed;
                observation.attention_reason = Some(failure.message.clone());
                self.record_retries(turn_id, retries).await?;
                return Ok(observation);
            }
            let wait = TURN_POLL.min(deadline.saturating_duration_since(std::time::Instant::now()));
            tokio::time::sleep(wait.max(std::time::Duration::from_millis(10))).await;
        }
    }

    async fn stop_turn(&self, session_id: &str, turn_id: &str) -> Result<(), RunError> {
        self.state
            .read()
            .await
            .stop_session_turn(session_id, turn_id)
            .await
            .map(|_| ())
            .map_err(daemon_error)
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
        session_id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError> {
        self.state
            .read()
            .await
            .loadout_read_sandbox_file(session_id, path, max_bytes)
            .await
            .map_err(daemon_error)
    }

    async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError> {
        self.state
            .read()
            .await
            .record_loadout_run_event(run_id, &event)
            .map(|_| ())
            .map_err(daemon_error)
    }

    async fn recorded_events(&self, run_id: &str) -> Result<Vec<RunEvent>, RunError> {
        self.state
            .read()
            .await
            .loadout_run_recorded_events(run_id)
            .map_err(daemon_error)
    }

    async fn tool_calls(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<Vec<axocoatl_daemon::loadout::ToolCallRecord>>, RunError> {
        self.state
            .read()
            .await
            .loadout_turn_tool_calls(session_id, turn_id)
            .map_err(daemon_error)
    }

    async fn network_summary(&self, session_id: &str) -> Result<NetworkSummary, RunError> {
        Ok(self
            .state
            .read()
            .await
            .loadout_network_summary(session_id)
            .await)
    }

    async fn stop_requested(&self, run_id: &str) -> bool {
        self.state.read().await.loadout_run_stop_requested(run_id)
    }

    async fn finish(&self, run_id: &str, outcome: &RunOutcome) -> Result<(), RunError> {
        self.state
            .read()
            .await
            .finish_loadout_run(run_id, outcome)
            .map_err(daemon_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn the daemon could not start because the Workspace was held
    /// ends the run busy (exit 7); any other failure stays infrastructure.
    #[test]
    fn a_turn_refused_for_a_held_workspace_is_busy() {
        let busy = SendFailure::of(&axocoatl_daemon::DaemonError::WorkspaceBusy(
            "the Workspace is held by another Session's turn".into(),
        ));
        let error = busy.not_started();
        assert!(matches!(error, RunError::Busy(_)), "{error}");
        assert_eq!(
            error.to_string(),
            "Workspace busy: the turn could not start: the Workspace is held by another \
             Session's turn"
        );
        let other = SendFailure::of(&axocoatl_daemon::DaemonError::SessionConflict(
            "the Session is closing".into(),
        ));
        let error = other.not_started();
        assert!(matches!(error, RunError::Infrastructure(_)), "{error}");
        assert_eq!(
            error.to_string(),
            "the turn could not start: Session conflict: the Session is closing"
        );
    }
}
