use super::*;

pub async fn session_work(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::SessionWorkView>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .session_work(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}
pub async fn configure_session_work(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(edit): Json<axocoatl_daemon::SessionWorkBindingEdit>,
) -> Result<Json<axocoatl_daemon::ArmedTeamWorkBinding>, (StatusCode, Json<ErrorResponse>)> {
    let binding = state
        .read()
        .await
        .configure_session_work(&id, edit)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(binding))
}
pub async fn admit_manual_session_work(
    State(state): State<AppState>,
    Path((id, binding)): Path<(String, String)>,
    Json(input): Json<axocoatl_daemon::SessionWorkEventInput>,
) -> Result<Json<axocoatl_daemon::TeamWorkReceipt>, (StatusCode, Json<ErrorResponse>)> {
    let receipt = state
        .read()
        .await
        .admit_manual_session_work(&id, &binding, input)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(receipt))
}
pub async fn admit_signed_session_work(
    State(state): State<AppState>,
    Path((id, binding)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<axocoatl_daemon::TeamWorkReceipt>, (StatusCode, Json<ErrorResponse>)> {
    let signature = headers
        .get("x-axocoatl-signature")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let receipt = state
        .read()
        .await
        .admit_signed_session_work(&id, &binding, &body, signature)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(receipt))
}
pub async fn run_session_work(
    State(state): State<AppState>,
    Path((id, receipt)): Path<(String, String)>,
) -> Result<Json<axocoatl_daemon::SessionWorkView>, (StatusCode, Json<ErrorResponse>)> {
    let result = state.read().await.run_session_work(&id, &receipt).await;
    if let Err(error) = &result {
        let _ = state
            .read()
            .await
            .record_standing_work_blocked(&id, &receipt, error.to_string());
    }
    let view = result.map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(view))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DismissWork {
    reason: String,
}
pub async fn dismiss_session_work(
    State(state): State<AppState>,
    Path((id, receipt)): Path<(String, String)>,
    Json(input): Json<DismissWork>,
) -> Result<Json<axocoatl_daemon::TeamWorkReceipt>, (StatusCode, Json<ErrorResponse>)> {
    let receipt = state
        .read()
        .await
        .dismiss_session_work(&id, &receipt, input.reason)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(receipt))
}

/// A person settles closed work whose provider usage stayed unknown at its
/// reserved ceiling, releasing the rest of the shared budget.
pub async fn settle_session_work_at_ceiling(
    State(state): State<AppState>,
    Path((id, receipt)): Path<(String, String)>,
) -> Result<Json<axocoatl_daemon::SessionWorkView>, (StatusCode, Json<ErrorResponse>)> {
    let view = state
        .read()
        .await
        .settle_session_work_at_ceiling(&id, &receipt)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(view))
}

/// Each iteration returns to the persisted FIFO queue. No retained accepted or
/// interrupted turn is replayed; only never-admitted work can enter the driver.
pub(crate) fn spawn_standing_work(state: AppState, id: String) {
    tokio::spawn(async move {
        drain_standing_work(&state, &id).await;
    });
}

async fn drain_standing_work(state: &AppState, id: &str) {
    let mut previous = None;
    loop {
        let daemon = state.read().await;
        let Ok(view) = daemon.session_work(id).await else {
            return;
        };
        let Some(item) = view
            .receipts
            .iter()
            .find(|item| item.state != "settled" && item.state != "dismissed")
        else {
            return;
        };
        if !matches!(item.state.as_str(), "queued" | "reserved")
            || previous.as_ref() == Some(&item.receipt.receipt_id)
        {
            return;
        }
        let receipt = item.receipt.receipt_id.clone();
        if let Err(error) = daemon.run_session_work(id, &receipt).await {
            let recorded = daemon.record_standing_work_blocked(id, &receipt, error.to_string());
            tracing::info!(session = %id, receipt = %receipt, %error, ?recorded, "standing work requires review");
            return;
        }
        previous = Some(receipt);
    }
}

pub(crate) struct StandingWorkWakeups(tokio::task::JoinHandle<()>);
impl Drop for StandingWorkWakeups {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Startup and actual completion notifications revisit durable receipts. The
/// event bus never supplies candidate evidence or grants execution permission.
pub(crate) async fn start_standing_work_wakeups(state: AppState) -> StandingWorkWakeups {
    let (mut stream, shutdown) = {
        let daemon = state.read().await;
        (daemon.stream_bus.subscribe(), daemon.shutdown_notifier())
    };
    StandingWorkWakeups(tokio::spawn(async move {
        loop {
            let sessions = {
                let daemon = state.read().await;
                if daemon.shutdown_requested() {
                    return;
                }
                if let Err(error) = daemon.reconcile_internal_session_work() {
                    tracing::warn!(%error,"internal standing source requires review");
                }
                daemon.standing_work_sessions().unwrap_or_default()
            };
            for session in sessions {
                drain_standing_work(&state, &session).await;
            }
            loop {
                tokio::select! {
                    _ = shutdown.notified() => return,
                    frame = stream.recv() => match frame {
                        Ok(axocoatl_daemon::stream::StreamFrame::SessionDone {..} | axocoatl_daemon::stream::StreamFrame::SessionCancelled {..} | axocoatl_daemon::stream::StreamFrame::SessionError {..} | axocoatl_daemon::stream::StreamFrame::SessionNeedsAttention {..} | axocoatl_daemon::stream::StreamFrame::SessionEnvironmentSettled {..}) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        _ => {},
                    }
                }
            }
        }
    }))
}
