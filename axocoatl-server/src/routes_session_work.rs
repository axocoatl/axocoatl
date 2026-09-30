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

pub async fn session_signals(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<axocoatl_daemon::SignalFieldView>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .session_signals(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}
pub async fn sense_session_signals(
    State(state): State<AppState>,
    Path((id, binding)): Path<(String, String)>,
) -> Result<Json<Vec<axocoatl_daemon::SignalFieldView>>, (StatusCode, Json<ErrorResponse>)> {
    let view = state
        .read()
        .await
        .sense_session_signals(&id, &binding)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(view))
}
pub async fn flag_session_signal(
    State(state): State<AppState>,
    Path((id, binding)): Path<(String, String)>,
    Json(input): Json<axocoatl_daemon::SignalFlagInput>,
) -> Result<Json<Vec<axocoatl_daemon::SignalFieldView>>, (StatusCode, Json<ErrorResponse>)> {
    let view = state
        .read()
        .await
        .flag_session_signal(&id, &binding, input)
        .await
        .map_err(attempt_err)?;
    spawn_standing_work(state, id);
    Ok(Json(view))
}
pub async fn withdraw_session_signal(
    State(state): State<AppState>,
    Path((id, binding, deposit)): Path<(String, String, String)>,
    Json(input): Json<axocoatl_daemon::SignalWithdrawInput>,
) -> Result<Json<Vec<axocoatl_daemon::SignalFieldView>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .withdraw_session_signal(&id, &binding, &deposit, input)
        .await
        .map(Json)
        .map_err(attempt_err)
}
pub async fn dispatch_session_signal(
    State(state): State<AppState>,
    Path((id, binding, slot)): Path<(String, String, String)>,
) -> Result<Json<Vec<axocoatl_daemon::SignalFieldView>>, (StatusCode, Json<ErrorResponse>)> {
    let view = state
        .read()
        .await
        .dispatch_session_signal(&id, &binding, &slot)
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
    let mut sensed_idle = false;
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
            // Settled or superseded signal work can release a held crossing.
            // Sense once at each idle point; new work continues the drain.
            if sensed_idle {
                return;
            }
            sensed_idle = true;
            let before = view.receipts.len();
            if let Err(error) = daemon.reconcile_session_signal_fields(Some(id)).await {
                tracing::warn!(session = %id, %error, "signal field requires review");
                return;
            }
            match daemon.session_work(id).await {
                Ok(after) if after.receipts.len() > before => continue,
                _ => return,
            }
        };
        sensed_idle = false;
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
        let mut timer_only = false;
        loop {
            let mut observe_later = false;
            let sessions = {
                let daemon = state.read().await;
                if daemon.shutdown_requested() {
                    return;
                }
                if !timer_only {
                    if let Err(error) = daemon.reconcile_internal_session_work() {
                        tracing::warn!(%error,"internal standing source requires review");
                    }
                }
                // Findings and changes from the turn that just settled become
                // deposits; a crossed threshold admits targeted work here.
                match daemon.reconcile_signal_fields().await {
                    Ok(deferred) => observe_later = deferred,
                    Err(error) => tracing::warn!(%error,"signal field requires review"),
                }
                // A re-check can admit signal work, so queues still drain.
                daemon.standing_work_sessions().unwrap_or_default()
            };
            for session in sessions {
                drain_standing_work(&state, &session).await;
            }
            timer_only = false;
            // A turn-ending event can arrive before the turn's state is
            // recorded; one deadline per wait, so other frames do not keep
            // postponing the look that observes its changes.
            let retry = tokio::time::sleep(std::time::Duration::from_secs(10));
            tokio::pin!(retry);
            loop {
                tokio::select! {
                    _ = shutdown.notified() => return,
                    _ = &mut retry, if observe_later => {
                        timer_only = true;
                        break;
                    }
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
