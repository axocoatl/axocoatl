//! Workspace knowledge is reached through its owning Session, like Files and History.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeQuery {
    q: Option<String>,
}

pub async fn session_knowledge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<KnowledgeQuery>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeView>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .session_knowledge(&id, query.q)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn create_session_knowledge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(edit): Json<axocoatl_daemon::SessionKnowledgeEdit>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeNote>, (StatusCode, Json<ErrorResponse>)> {
    if edit.expected_revision != 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Create requires revision zero; use the note's update route",
        ));
    }
    state
        .read()
        .await
        .save_session_knowledge(&id, edit)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn update_session_knowledge(
    State(state): State<AppState>,
    Path((id, note_id)): Path<(String, String)>,
    Json(mut edit): Json<axocoatl_daemon::SessionKnowledgeEdit>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeNote>, (StatusCode, Json<ErrorResponse>)> {
    if edit.id.as_ref().is_some_and(|value| value != &note_id) || edit.expected_revision == 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Update requires this exact note and its recorded revision",
        ));
    }
    edit.id = Some(note_id);
    state
        .read()
        .await
        .save_session_knowledge(&id, edit)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn accept_session_knowledge(
    State(state): State<AppState>,
    Path((id, proposal_id)): Path<(String, String)>,
    Json(input): Json<KnowledgeAttachmentRequest>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeView>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .decide_session_knowledge(&id, &proposal_id, true, Some(input.expected_revision))
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn reject_session_knowledge(
    State(state): State<AppState>,
    Path((id, proposal_id)): Path<(String, String)>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeView>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .decide_session_knowledge(&id, &proposal_id, false, None)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn refresh_session_knowledge_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeView>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .refresh_session_knowledge_index(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeImportRequest {
    markdown: String,
}

pub async fn preview_session_knowledge_import(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<KnowledgeImportRequest>,
) -> Result<Json<axocoatl_daemon::SessionKnowledgeEdit>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .preview_session_knowledge_import(&id, input.markdown)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeAttachmentRequest {
    expected_revision: u64,
}

pub async fn attach_session_knowledge(
    State(state): State<AppState>,
    Path((id, note_id)): Path<(String, String)>,
    Json(input): Json<KnowledgeAttachmentRequest>,
) -> Result<Json<axocoatl_session::SessionAttachmentRef>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .attach_session_knowledge(&id, &note_id, input.expected_revision)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn export_session_knowledge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<KnowledgeExportQuery>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let body = match query.note_id {
        Some(note_id) => daemon.export_session_knowledge_note(&id, &note_id).await,
        None => daemon.export_session_knowledge(&id).await,
    }
    .map_err(attempt_err)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"workspace-knowledge.md\"",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeExportQuery {
    note_id: Option<String>,
}
