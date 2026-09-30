use super::*;
#[derive(Deserialize)]
pub struct RevokeGrant {
    pub grant_id: String,
    pub expected_grant_revision: u64,
}
pub async fn session_control_grants(
    State(state): State<AppState>,
    Path((id, turn)): Path<(String, String)>,
) -> Result<
    Json<axocoatl_daemon::session_dispatch::SessionGrantView>,
    (StatusCode, Json<ErrorResponse>),
> {
    state
        .read()
        .await
        .session_control_grants(&id, &turn)
        .await
        .map(Json)
        .map_err(attempt_err)
}
pub async fn preview_session_grant(
    State(state): State<AppState>,
    Path((id, turn)): Path<(String, String)>,
    Json(request): Json<axocoatl_daemon::session_dispatch::SessionGrantChange>,
) -> Result<
    Json<axocoatl_daemon::session_dispatch::SessionGrantPreview>,
    (StatusCode, Json<ErrorResponse>),
> {
    state
        .read()
        .await
        .preview_session_grant(&id, &turn, request)
        .await
        .map(Json)
        .map_err(attempt_err)
}
pub async fn decide_session_grant(
    State(state): State<AppState>,
    Path((id, turn)): Path<(String, String)>,
    Json(request): Json<axocoatl_daemon::session_dispatch::SessionGrantDecision>,
) -> Result<
    Json<axocoatl_daemon::session_dispatch::SessionGrantView>,
    (StatusCode, Json<ErrorResponse>),
> {
    state
        .read()
        .await
        .decide_session_grant(&id, &turn, request)
        .await
        .map(Json)
        .map_err(attempt_err)
}
pub async fn revoke_session_grant(
    State(state): State<AppState>,
    Path((id, turn)): Path<(String, String)>,
    Json(request): Json<RevokeGrant>,
) -> Result<
    Json<axocoatl_daemon::session_dispatch::SessionGrantView>,
    (StatusCode, Json<ErrorResponse>),
> {
    state
        .read()
        .await
        .revoke_session_grant(
            &id,
            &turn,
            &request.grant_id,
            request.expected_grant_revision,
        )
        .await
        .map(Json)
        .map_err(attempt_err)
}
