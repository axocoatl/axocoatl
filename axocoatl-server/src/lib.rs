pub mod auth;
pub mod middleware;
pub mod routes;

use std::sync::Arc;

use axocoatl_daemon::AxocoatlDaemon;
use axum::{
    extract::DefaultBodyLimit,
    routing::{any, get, post},
    Router,
};
use tokio::sync::RwLock;

/// Shared application state for the Axum server.
pub type AppState = Arc<RwLock<AxocoatlDaemon>>;

/// Build the Axum router with all API routes.
///
/// `auth` gates every route except the health probes (see [`auth::enforce`]).
/// `cors_origins` is the cross-origin allow-list; empty means same-origin only.
/// `rate_limiter` throttles per client IP (a no-op when disabled, the default).
pub fn build_router(
    state: AppState,
    auth: auth::AuthConfig,
    cors_origins: Vec<String>,
    rate_limiter: Arc<middleware::RateLimiter>,
) -> Router {
    let auth_for_mw = auth.clone();
    let cors_origins_for_auth = Arc::new(cors_origins.clone());
    let preview_state = state.clone();
    let shutdown_state = state.clone();
    Router::new()
        .route("/", get(routes::dashboard))
        .route("/lattice/{file}", get(routes::lattice_asset))
        .route("/vendor/{*file}", get(routes::vendor_asset))
        .route("/ui/{*file}", get(routes::ui_asset))
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
        .route("/api/session-teams", get(routes::list_session_teams))
        .route(
            "/api/workflows/{workflow_id}/execute",
            post(routes::execute_workflow),
        )
        .route("/api/tokens/report", get(routes::token_report))
        .route(
            "/api/sessions",
            get(routes::list_sessions).post(routes::create_session),
        )
        .route(
            "/api/workspaces",
            get(routes::list_workspaces).post(routes::create_workspace),
        )
        .route(
            "/api/workspaces/{id}",
            get(routes::get_workspace).patch(routes::rename_workspace),
        )
        .route(
            "/api/workspaces/{id}/sessions",
            get(routes::list_workspace_sessions).post(routes::create_workspace_session),
        )
        .route("/api/sessions/{id}/execute", post(routes::execute_session))
        .route("/api/sessions/{id}/team", get(routes::session_team))
        .route("/api/sessions/{id}/work", get(routes::session_work))
        .route(
            "/api/sessions/{id}/knowledge",
            get(routes::session_knowledge)
                .post(routes::create_session_knowledge)
                .layer(DefaultBodyLimit::max(512 * 1024)),
        )
        .route(
            "/api/sessions/{id}/knowledge/{note_id}",
            axum::routing::put(routes::update_session_knowledge)
                .layer(DefaultBodyLimit::max(512 * 1024)),
        )
        .route(
            "/api/sessions/{id}/knowledge/index",
            post(routes::refresh_session_knowledge_index),
        )
        .route(
            "/api/sessions/{id}/knowledge/export",
            get(routes::export_session_knowledge),
        )
        .route(
            "/api/sessions/{id}/knowledge/import-preview",
            post(routes::preview_session_knowledge_import).layer(DefaultBodyLimit::max(512 * 1024)),
        )
        .route(
            "/api/sessions/{id}/knowledge/{note_id}/attach",
            post(routes::attach_session_knowledge).layer(DefaultBodyLimit::max(1024)),
        )
        .route(
            "/api/sessions/{id}/knowledge/proposals/{proposal_id}/accept",
            post(routes::accept_session_knowledge),
        )
        .route(
            "/api/sessions/{id}/knowledge/proposals/{proposal_id}/reject",
            post(routes::reject_session_knowledge),
        )
        .route(
            "/api/sessions/{id}/work/bindings",
            post(routes::configure_session_work).layer(DefaultBodyLimit::max(32 * 1024)),
        )
        .route(
            "/api/sessions/{id}/work/bindings/{binding}/manual",
            post(routes::admit_manual_session_work).layer(DefaultBodyLimit::max(32 * 1024)),
        )
        .route(
            "/api/sessions/{id}/work/bindings/{binding}/webhook",
            post(routes::admit_signed_session_work).layer(DefaultBodyLimit::max(32 * 1024)),
        )
        .route(
            "/api/sessions/{id}/work/receipts/{receipt}/run",
            post(routes::run_session_work),
        )
        .route(
            "/api/sessions/{id}/work/receipts/{receipt}/dismiss",
            post(routes::dismiss_session_work).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/api/sessions/{id}/work/receipts/{receipt}/settle-at-ceiling",
            post(routes::settle_session_work_at_ceiling),
        )
        .route(
            "/api/sessions/{id}/work/signals",
            get(routes::session_signals),
        )
        .route(
            "/api/sessions/{id}/work/signals/{binding}/sense",
            post(routes::sense_session_signals),
        )
        .route(
            "/api/sessions/{id}/work/signals/{binding}/flags",
            post(routes::flag_session_signal).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/api/sessions/{id}/work/signals/{binding}/deposits/{deposit}/withdraw",
            post(routes::withdraw_session_signal).layer(DefaultBodyLimit::max(8 * 1024)),
        )
        .route(
            "/api/sessions/{id}/work/signals/{binding}/routes/{slot}/dispatch",
            post(routes::dispatch_session_signal),
        )
        .route(
            "/api/sessions/{id}/ways-history",
            get(routes::session_ways_history),
        )
        .route(
            "/api/sessions/{id}/ways-history/configuration",
            post(routes::configure_session_ways_history),
        )
        .route(
            "/api/sessions/{id}/ways-history/{decision}",
            get(routes::export_session_ways_decision).delete(routes::delete_session_ways_decision),
        )
        .route(
            "/api/sessions/{id}/team/preview",
            post(routes::preview_session_team),
        )
        .route(
            "/api/sessions/{id}/team/apply",
            post(routes::apply_session_team),
        )
        .route(
            "/api/sessions/{id}/team/cancel",
            post(routes::cancel_session_team),
        )
        .route("/api/sessions/{id}/messages", get(routes::session_messages))
        .route("/api/sessions/{id}/turns", get(routes::session_turns))
        .route(
            "/api/sessions/{id}/active-turn",
            get(routes::active_session_turn),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}",
            get(routes::session_turn),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/control-plane",
            get(routes::session_turn_control_plane),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/graph-edits/preview",
            post(routes::preview_session_graph_edit).layer(DefaultBodyLimit::max(
                axocoatl_session::control_command::MAX_CONTROL_REQUEST_BYTES,
            )),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/graph-edits/apply",
            post(routes::apply_session_graph_edit).layer(DefaultBodyLimit::max(
                axocoatl_session::control_command::MAX_CONTROL_REQUEST_BYTES,
            )),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/grants",
            get(routes::session_control_grants),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/grants/preview",
            post(routes::preview_session_grant).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/grants/decide",
            post(routes::decide_session_grant).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/grants/revoke",
            post(routes::revoke_session_grant).layer(DefaultBodyLimit::max(4096)),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/control-plan",
            post(routes::plan_session_control).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/sessions/{id}/turns/{turn_id}/control-commands",
            post(routes::submit_session_control_action).layer(DefaultBodyLimit::max(
                axocoatl_session::control_command::MAX_CONTROL_REQUEST_BYTES,
            )),
        )
        .route(
            "/api/session-turns/search",
            get(routes::search_session_turns),
        )
        .route("/api/sessions/{id}/export", get(routes::export_session))
        .route("/api/sessions/{id}/rewind", post(routes::rewind_session))
        .route(
            "/api/sessions/{id}/attachments",
            get(routes::list_session_attachments)
                .post(routes::upload_session_attachment)
                // Multipart has boundary overhead in addition to the 25 MiB
                // document cap enforced while reading the file field.
                .layer(DefaultBodyLimit::max(26 * 1024 * 1024)),
        )
        .route(
            "/api/sessions/{id}/attachments/{reference_id}",
            get(routes::get_session_attachment)
                .patch(routes::patch_session_attachment)
                .delete(routes::delete_session_attachment),
        )
        .route(
            "/api/sessions/{id}/attachments/{reference_id}/content",
            get(routes::get_session_attachment_content),
        )
        .route("/api/sessions/{id}/git/status", get(routes::git_status))
        .route("/api/sessions/{id}/git/diff", get(routes::git_diff))
        .route("/api/sessions/{id}/git/branches", get(routes::git_branches))
        .route("/api/sessions/{id}/git/commit", post(routes::git_commit))
        .route(
            "/api/sessions/{id}/check",
            axum::routing::put(routes::set_session_check),
        )
        .route(
            "/api/sessions/{id}/environment",
            axum::routing::put(routes::configure_session_environment),
        )
        .route(
            "/api/sessions/{id}/environment/rebuild",
            post(routes::rebuild_session_environment),
        )
        .route(
            "/api/sessions/{id}/environment/confirm-runtime-cleanup",
            post(routes::confirm_session_runtime_cleanup),
        )
        .route("/api/sessions/{id}/git/hunks", get(routes::git_hunks))
        .route(
            "/api/sessions/{id}/git/hunk/discard",
            post(routes::git_revert_hunk),
        )
        .route("/api/sessions/{id}/git/hunk", post(routes::git_apply_hunk))
        .route("/api/sessions/{id}/git/stage", post(routes::git_stage))
        .route("/api/sessions/{id}/git/unstage", post(routes::git_unstage))
        .route("/api/sessions/{id}/git/discard", post(routes::git_discard))
        .route(
            "/api/sessions/{id}/git/checkout",
            post(routes::git_checkout),
        )
        .route(
            "/api/sessions/{id}/variants",
            post(routes::session_variants),
        )
        .route(
            "/api/sessions/{id}/variants/status",
            get(routes::session_variants_status),
        )
        .route(
            "/api/sessions/{id}/variants/results",
            get(routes::session_variants_results),
        )
        .route(
            "/api/sessions/{id}/variants/trajectories",
            get(routes::session_variants_trajectories),
        )
        .route(
            "/api/sessions/{id}/variants/verify",
            post(routes::session_variants_verify),
        )
        .route(
            "/api/sessions/{id}/variants/diff",
            get(routes::session_variant_diff),
        )
        .route(
            "/api/sessions/{id}/variants/judge",
            post(routes::session_variants_judge),
        )
        .route("/api/variants/probe", get(routes::variants_probe))
        .route(
            "/api/sessions/{id}/variants/plan",
            post(routes::session_variants_plan),
        )
        .route(
            "/api/sessions/{id}/variants/cost",
            get(routes::session_variants_cost),
        )
        .route(
            "/api/sessions/{id}/variants/adopt",
            post(routes::session_variant_adopt),
        )
        .route(
            "/api/sessions/{id}/variants/discard",
            post(routes::session_variants_discard),
        )
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
            any(routes::session_browser_proxy_root),
        )
        .route(
            "/api/sessions/{id}/proxy/{port}/{*tail}",
            any(routes::session_browser_proxy),
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
        .route("/api/sessions/{id}/reopen", post(routes::reopen_session))
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
            post(routes::upload_chat_attachment)
                .put(routes::attach_file_to_chat)
                // Multipart has boundary overhead in addition to the 10 MiB
                // image cap enforced by the upload handler. Without this
                // override Axum's default body limit rejects valid images
                // before the handler can apply its type-specific limit.
                .layer(DefaultBodyLimit::max(11 * 1024 * 1024)),
        )
        .route(
            "/api/chat/{id}/attach/{file_id}",
            get(routes::get_chat_attachment)
                .delete(routes::delete_chat_attachment)
                .patch(routes::pin_chat_attachment),
        )
        // Retained cross-chat FileStore API (no peer Files browser surface).
        .route(
            "/api/files",
            get(routes::list_files)
                .post(routes::upload_file)
                // Keep the compatibility FileStore uploader aligned with the
                // same bounded 10 MiB image contract as chat attachments.
                .layer(DefaultBodyLimit::max(11 * 1024 * 1024)),
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
        // Layers run outermost-first on the request: CORS handles preflight,
        // then logging (so 401s/429s are recorded), then the rate limiter
        // (throttles before auth so unauthenticated floods are capped), then
        // auth right before handlers.
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let cfg = auth_for_mw.clone();
                let allowed_origins = cors_origins_for_auth.clone();
                async move { auth::enforce(&cfg, allowed_origins.as_slice(), req, next).await }
            },
        ))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let rl = rate_limiter.clone();
                async move { middleware::rate_limit(rl, req, next).await }
            },
        ))
        .layer(axum::middleware::from_fn(middleware::request_logging))
        .layer(middleware::cors_layer(&cors_origins))
        // Preview uses the same listener but never the workbench router. This
        // outermost Host boundary maps every virtual-host path or WebSocket to
        // one Session/port and rejects invalid/non-local Preview hosts.
        .layer(axum::middleware::from_fn_with_state(
            preview_state,
            routes::preview_host_boundary,
        ))
        // Outermost request ownership: a shutdown notification drops every
        // in-flight HTTP handler, including Preview routing and runtime
        // creation, before checked Session cleanup waits on its admission
        // barrier. Upgraded socket tasks use their own lifecycle gates.
        .layer(axum::middleware::from_fn_with_state(
            shutdown_state,
            middleware::cancel_on_shutdown,
        ))
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
    let (auth, cors_origins, allow_unauthenticated, rate_cfg) = {
        let d = state.read().await;
        let s = &d.config.server;
        (
            auth::AuthConfig::new(s.auth.api_keys.clone(), s.auth.bearer_tokens.clone())
                .with_allow_unauthenticated_remote(s.auth.allow_unauthenticated),
            s.cors_origins.clone(),
            s.auth.allow_unauthenticated,
            s.rate_limit.clone(),
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
        state.read().await.begin_shutdown();
        let cleanup = state.read().await.shutdown_session_runtimes_checked().await;
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup_error) => std::io::Error::other(format!(
                "{error}; Session runtime cleanup after the rejected bind was incomplete: {cleanup_error}"
            )),
        });
    }
    if auth.enabled {
        tracing::info!(host, "Axocoatl API authentication enabled");
    } else {
        tracing::warn!(
            host,
            "Axocoatl API authentication disabled — loopback/local use only"
        );
    }

    let rate_limiter = Arc::new(middleware::RateLimiter::new(middleware::RateLimitConfig {
        max_requests: rate_cfg.max_requests,
        window_secs: rate_cfg.window_secs,
        enabled: rate_cfg.enabled,
    }));
    if rate_cfg.enabled {
        tracing::info!(
            max_requests = rate_cfg.max_requests,
            window_secs = rate_cfg.window_secs,
            "HTTP rate limiting enabled"
        );
    }

    let app = build_router(state.clone(), auth, cors_origins, rate_limiter);

    let addr = format!("{host}:{port}");
    tracing::info!(addr = %addr, "Starting Axocoatl API server");

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            state.read().await.begin_shutdown();
            let cleanup = state.read().await.shutdown_session_runtimes_checked().await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => std::io::Error::other(format!(
                    "{error}; Session runtime cleanup after bind failure was incomplete: {cleanup_error}"
                )),
            });
        }
    };
    let _standing_work_wakeups = routes::start_standing_work_wakeups(state.clone()).await;
    // Start draining connections as soon as OS or IPC shutdown is requested,
    // while checked runtime cleanup proceeds concurrently. A stuck WebSocket
    // or request gets a bounded grace period; aborting the server then drops
    // that request so its runtime creation lease can roll back and cleanup can
    // finish rather than hanging forever.
    let (graceful_tx, mut graceful_rx) = tokio::sync::watch::channel(false);
    let mut server_task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            while !*graceful_rx.borrow() {
                if graceful_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
    });
    let shutdown_request = wait_for_shutdown_request(state.clone());
    tokio::pin!(shutdown_request);

    tokio::select! {
        joined = &mut server_task => {
            let server = match joined {
                Ok(server) => server,
                Err(error) => Err(std::io::Error::other(format!("HTTP server task failed: {error}"))),
            };
            state.read().await.begin_shutdown();
            let cleanup = state.read().await.shutdown_session_runtimes_checked().await;
            combine_server_cleanup(server, cleanup)
        }
        _ = &mut shutdown_request => {
            state.read().await.begin_shutdown();
            let _ = graceful_tx.send(true);
            let cleanup_state = state.clone();
            let cleanup_task = tokio::spawn(async move {
                cleanup_state.read().await.shutdown_session_runtimes_checked().await
            });

            let server = match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                &mut server_task,
            ).await {
                Ok(joined) => match joined {
                    Ok(server) => server,
                    Err(error) => Err(std::io::Error::other(format!(
                        "HTTP server task failed: {error}"
                    ))),
                },
                Err(_) => {
                    tracing::warn!("forcing remaining HTTP connections closed after shutdown grace period");
                    server_task.abort();
                    let _ = server_task.await;
                    Ok(())
                }
            };
            let cleanup = match cleanup_task.await {
                Ok(cleanup) => cleanup,
                Err(error) => {
                    let server_detail = server
                        .as_ref()
                        .err()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "HTTP server stopped".to_string());
                    return Err(std::io::Error::other(format!(
                        "{server_detail}; Session runtime cleanup task failed: {error}"
                    )));
                }
            };
            combine_server_cleanup(server, cleanup)
        }
    }
}

fn combine_server_cleanup(
    server: std::io::Result<()>,
    cleanup: Result<(), axocoatl_daemon::DaemonError>,
) -> std::io::Result<()> {
    match (server, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => Err(std::io::Error::other(cleanup_error.to_string())),
        (Err(error), Err(cleanup_error)) => Err(std::io::Error::other(format!(
            "{error}; Session runtime shutdown was incomplete: {cleanup_error}"
        ))),
    }
}

async fn wait_for_shutdown_request(state: AppState) {
    let notifier = state.read().await.shutdown_notifier();
    if state.read().await.shutdown_requested() {
        return;
    }
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                    _ = notifier.notified() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not install SIGTERM handler; shutdown remains available through Ctrl-C and the daemon control path");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = notifier.notified() => {}
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = notifier.notified() => {}
        }
    }
    state.read().await.begin_shutdown();
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
