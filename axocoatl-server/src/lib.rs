pub mod auth;
pub mod middleware;
pub mod routes;

use std::sync::Arc;

use axocoatl_daemon::AxocoatlDaemon;
use axum::{
    routing::{get, post},
    Router,
};
use tokio::sync::RwLock;

/// Shared application state for the Axum server.
pub type AppState = Arc<RwLock<AxocoatlDaemon>>;

/// Build the Axum router with all API routes.
///
/// `auth` gates every route except the health probes (see [`auth::enforce`]).
/// `cors_origins` is the cross-origin allow-list; empty means same-origin only.
pub fn build_router(state: AppState, auth: auth::AuthConfig, cors_origins: Vec<String>) -> Router {
    let auth_for_mw = auth.clone();
    Router::new()
        .route("/", get(routes::dashboard))
        .route("/lattice/{file}", get(routes::lattice_asset))
        .route("/vendor/{*file}", get(routes::vendor_asset))
        .route("/health", get(routes::health))
        .route("/health/ready", get(routes::health_ready))
        .route("/health/live", get(routes::health_live))
        .route("/api/llm-health", get(routes::llm_health))
        .route("/api/agents", get(routes::list_agents))
        .route(
            "/api/agents/{agent_id}/execute",
            post(routes::execute_agent),
        )
        .route("/api/agents/{agent_id}/status", get(routes::agent_status))
        .route(
            "/api/agents/{agent_id}/restart",
            post(routes::restart_agent),
        )
        .route(
            "/api/agents/{agent_id}",
            axum::routing::patch(routes::patch_agent),
        )
        .route("/api/mcp/catalog", get(routes::mcp_catalog))
        .route("/api/mcp/install", post(routes::install_mcp))
        .route(
            "/api/mcp/servers/{name}",
            post(routes::reconnect_mcp).delete(routes::remove_mcp),
        )
        .route("/api/mcp/servers", get(routes::list_mcp_servers))
        .route(
            "/api/mcp/permissions",
            get(routes::list_mcp_permissions).delete(routes::revoke_mcp_permission),
        )
        .route("/api/mcp/tools", get(routes::list_mcp_tools))
        .route("/api/schedules", get(routes::list_schedules))
        .route("/api/proactive", get(routes::list_proactive))
        .route(
            "/api/schedules/{id}",
            axum::routing::patch(routes::patch_schedule),
        )
        .route("/api/schedules/{id}/run", post(routes::run_schedule))
        .route("/api/skills", get(routes::list_skills))
        .route("/api/skills/{id}/fire", post(routes::fire_skill))
        .route("/api/events/recent", get(routes::recent_events))
        .route("/api/workflows", get(routes::list_workflows))
        .route(
            "/api/workflows/{workflow_id}/execute",
            post(routes::execute_workflow),
        )
        .route("/api/tokens/report", get(routes::token_report))
        .route(
            "/api/sessions",
            get(routes::list_sessions).post(routes::create_session),
        )
        .route("/api/sessions/{id}/execute", post(routes::execute_session))
        .route("/api/sessions/{id}/tree", get(routes::session_tree))
        .route(
            "/api/sessions/{id}/file",
            get(routes::session_file).post(routes::session_file_write),
        )
        .route(
            "/api/sessions/{id}/tasks",
            get(routes::session_tasks).post(routes::session_task_spawn),
        )
        .route(
            "/api/sessions/{id}/terminals/{tid}/ws",
            get(routes::session_terminal_ws),
        )
        .route(
            "/api/sessions/{id}/proxy/{port}",
            get(routes::session_browser_proxy_root),
        )
        .route(
            "/api/sessions/{id}/proxy/{port}/{*tail}",
            get(routes::session_browser_proxy),
        )
        .route("/axo-tap.js", get(routes::axo_tap_script))
        .route("/brand/{file}", get(routes::brand_asset))
        .route(
            "/api/automations",
            get(routes::list_automations).post(routes::create_automation),
        )
        .route(
            "/api/automations/{id}",
            get(routes::get_automation)
                .put(routes::update_automation)
                .patch(routes::update_automation)
                .delete(routes::delete_automation),
        )
        .route("/api/automations/{id}/run", post(routes::run_automation))
        .route("/api/automations/{id}/move", post(routes::move_automation))
        .route(
            "/api/automation-folders",
            get(routes::list_automation_folders)
                .post(routes::create_automation_folder)
                .patch(routes::rename_automation_folder)
                .delete(routes::delete_automation_folder),
        )
        .route("/api/automations/{id}/runs", get(routes::list_runs))
        .route("/api/automations/{id}/runs/{run_id}", get(routes::get_run))
        .route(
            "/api/automations/{id}/runs/{run_id}/fork",
            post(routes::fork_run),
        )
        .route("/api/tools", get(routes::list_tools))
        .route("/api/interrupts", get(routes::list_interrupts))
        .route(
            "/api/automations/{id}/runs/{run_id}/nodes/{node_id}/resume",
            post(routes::resume_interrupt),
        )
        .route(
            "/api/automations/{id}/runs/{run_id}/nodes/{node_id}/cancel",
            post(routes::cancel_interrupt),
        )
        .route(
            "/api/sessions/{id}",
            axum::routing::delete(routes::close_session).patch(routes::rename_session),
        )
        // ── Chats ── lightweight conversations, no directory/sandbox.
        .route(
            "/api/chat",
            get(routes::list_chats).post(routes::create_chat),
        )
        .route(
            "/api/chat/{id}",
            get(routes::get_chat)
                .patch(routes::patch_chat)
                .delete(routes::delete_chat),
        )
        .route("/api/chat/{id}/fork", post(routes::fork_chat))
        .route("/api/chat/{id}/export", get(routes::export_chat))
        .route(
            "/api/chat/{id}/attach",
            post(routes::upload_chat_attachment).put(routes::attach_file_to_chat),
        )
        .route(
            "/api/chat/{id}/attach/{file_id}",
            get(routes::get_chat_attachment)
                .delete(routes::delete_chat_attachment)
                .patch(routes::pin_chat_attachment),
        )
        // Cross-chat Files browser surface.
        .route(
            "/api/files",
            get(routes::list_files).post(routes::upload_file),
        )
        .route(
            "/api/files/{id}",
            get(routes::get_file_meta)
                .patch(routes::patch_file)
                .delete(routes::delete_file),
        )
        .route("/api/files/{id}/bytes", get(routes::get_file_bytes))
        .route("/api/llm/models", get(routes::list_models))
        .route("/api/fs/list", get(routes::fs_list_dirs))
        .route("/api/fs/project", get(routes::fs_project_probe))
        .route("/ws", get(routes::ws))
        // A2A protocol — discovery card + task intake (behind auth).
        .route("/.well-known/agent.json", get(routes::a2a_agent_card))
        .route("/a2a/tasks", post(routes::a2a_receive_task))
        // Layers run outermost-first on the request. CORS handles preflight,
        // then logging (so 401s are recorded), then auth right before handlers.
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let cfg = auth_for_mw.clone();
                async move { auth::enforce(&cfg, req, next).await }
            },
        ))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(middleware::cors_layer(&cors_origins))
        .with_state(state)
}

/// Whether a bind host is loopback-only (`127.0.0.1`, `::1`, `localhost`).
/// Unknown hostnames are treated as non-loopback — the safer default.
fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Start the HTTP server.
pub async fn serve(daemon: AxocoatlDaemon, host: &str, port: u16) -> std::io::Result<()> {
    let state: AppState = Arc::new(RwLock::new(daemon));
    serve_shared(state, host, port).await
}

/// Start the HTTP server with a shared daemon state (for use alongside IPC).
pub async fn serve_shared(state: AppState, host: &str, port: u16) -> std::io::Result<()> {
    // Pull auth + CORS from the live config.
    let (auth, cors_origins, allow_unauthenticated) = {
        let d = state.read().await;
        let s = &d.config.server;
        (
            auth::AuthConfig::new(s.auth.api_keys.clone(), s.auth.bearer_tokens.clone()),
            s.cors_origins.clone(),
            s.auth.allow_unauthenticated,
        )
    };

    // Fail closed: never expose an unauthenticated API on a non-loopback
    // address. The operator must add credentials, bind to loopback, or
    // explicitly accept the risk (e.g. an auth-enforcing reverse proxy).
    if !is_loopback_host(host) && !auth.enabled && !allow_unauthenticated {
        let msg = format!(
            "refusing to bind {host}:{port}: authentication is not configured. \
             Set server.auth.api_keys / server.auth.bearer_tokens, bind to \
             127.0.0.1, or set server.auth.allow_unauthenticated = true if an \
             upstream proxy enforces auth."
        );
        tracing::error!("{msg}");
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            msg,
        ));
    }
    if auth.enabled {
        tracing::info!(host, "Axocoatl API authentication enabled");
    } else {
        tracing::warn!(
            host,
            "Axocoatl API authentication disabled — loopback/local use only"
        );
    }

    let app = build_router(state, auth, cors_origins);

    let addr = format!("{host}:{port}");
    tracing::info!(addr = %addr, "Starting Axocoatl API server");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_loopback_host;

    #[test]
    fn loopback_hosts_are_recognized() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
    }

    #[test]
    fn non_loopback_hosts_are_rejected() {
        // These would expose the API on the network — the guard must catch them.
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("::"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("10.0.0.5"));
        assert!(!is_loopback_host("example.com"));
    }
}
