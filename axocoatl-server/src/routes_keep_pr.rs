//! `POST /api/sessions/{id}/keep-pr`. Owner: workstream `keep`.
use super::*;
use axocoatl_daemon::keep_pr::{KeepPrRequest, KeepPrResponse};

pub async fn keep_pr(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<KeepPrRequest>,
) -> Result<Json<KeepPrResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .keep_as_pr(&id, request)
        .await
        .map(Json)
        .map_err(attempt_err)
}
