//! A2A protocol server — exposes Axocoatl agents as A2A-compatible endpoints.

use std::sync::Arc;

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::RwLock;

use crate::types::*;

/// State shared with A2A route handlers.
pub struct A2AServerState {
    pub agent_card: AgentCard,
    /// Callback to execute a task. The caller provides this to wire into the agent system.
    pub task_handler: Arc<dyn TaskHandler>,
}

/// Trait for handling incoming A2A tasks — implemented by the daemon.
#[async_trait::async_trait]
pub trait TaskHandler: Send + Sync + 'static {
    async fn handle_task(&self, task: A2ATask) -> Result<A2ATaskResult, String>;
}

/// Build the A2A Axum router.
pub fn build_a2a_router(state: Arc<RwLock<A2AServerState>>) -> Router {
    Router::new()
        .route("/.well-known/agent.json", get(serve_agent_card))
        .route("/tasks", post(receive_task))
        .with_state(state)
}

async fn serve_agent_card(State(state): State<Arc<RwLock<A2AServerState>>>) -> Json<AgentCard> {
    let state = state.read().await;
    Json(state.agent_card.clone())
}

async fn receive_task(
    State(state): State<Arc<RwLock<A2AServerState>>>,
    Json(task): Json<A2ATask>,
) -> Result<Json<A2ATaskResult>, (axum::http::StatusCode, String)> {
    let state = state.read().await;
    match state.task_handler.handle_task(task).await {
        Ok(result) => Ok(Json(result)),
        Err(e) => Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}
