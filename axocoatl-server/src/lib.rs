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
/// `auth` gates every route that reads or changes state; health probes, and in
/// local token mode the static assets and sign-in page, stay public (see
/// [`auth::enforce`]).
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
        .route("/api/sessions/{id}/network", get(routes::session_network))
        .route(
            "/api/sessions/{id}/network/screenshots/{sha256}",
            get(routes::session_network_screenshot),
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

/// For a loopback bind host, the address to serve on and the other loopback
/// address family's address. `localhost` names both.
fn loopback_bind_addresses(host: &str) -> Option<(std::net::IpAddr, std::net::IpAddr)> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return Some((
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ));
    }
    match h.parse::<IpAddr>().ok().filter(IpAddr::is_loopback)? {
        ip @ IpAddr::V4(_) => Some((ip, IpAddr::V6(Ipv6Addr::LOCALHOST))),
        ip @ IpAddr::V6(_) => Some((ip, IpAddr::V4(Ipv4Addr::LOCALHOST))),
    }
}

/// Listen on `address` beside the main loopback listener. `Ok(None)` means
/// this host has no loopback address in that family, so no other process can
/// listen there either.
fn bind_loopback_peer(
    address: std::net::SocketAddr,
) -> std::io::Result<Option<tokio::net::TcpListener>> {
    let socket = if address.is_ipv4() {
        tokio::net::TcpSocket::new_v4()
    } else {
        tokio::net::TcpSocket::new_v6()
    };
    // A kernel built without the address family cannot create the socket.
    let Ok(socket) = socket else {
        return Ok(None);
    };
    // Match `TcpListener::bind`, which sets SO_REUSEADDR on Unix.
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    match socket.bind(address) {
        Ok(()) => socket.listen(1024).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::AddrNotAvailable => Ok(None),
        Err(error) => Err(error),
    }
}

/// Bind the HTTP listeners for `host:port`. A loopback host also holds the
/// same port on the other loopback address family: browsers, curl and Node
/// try `[::1]` before `127.0.0.1` for `localhost`, so a process that listened
/// there would receive the sign-in link and the workbench's cookies. Startup
/// fails when another process already holds it.
async fn bind_listeners(host: &str, port: u16) -> std::io::Result<Vec<tokio::net::TcpListener>> {
    let Some((primary, peer)) = loopback_bind_addresses(host) else {
        return Ok(vec![
            tokio::net::TcpListener::bind(format!("{host}:{port}")).await?,
        ]);
    };
    // An ephemeral port may already be taken in the other family; pick again.
    let attempts = if port == 0 { 8 } else { 1 };
    let mut last_error = None;
    for _ in 0..attempts {
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::new(primary, port)).await?;
        let bound = listener.local_addr()?.port();
        let peer_address = std::net::SocketAddr::new(peer, bound);
        match bind_loopback_peer(peer_address) {
            Ok(Some(peer_listener)) => return Ok(vec![listener, peer_listener]),
            Ok(None) => {
                tracing::debug!(addr = %peer_address, "no loopback address in this family; serving one listener");
                return Ok(vec![listener]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                last_error = Some(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "another process is listening on {peer_address}. Browsers can send \
                         `localhost:{bound}` requests there instead of to Axocoatl on \
                         {primary}, so Axocoatl will not start beside it. Stop that process \
                         or choose another server.port."
                    ),
                ));
            }
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("could not also listen on {peer_address}: {error}"),
                ))
            }
        }
    }
    Err(last_error.unwrap_or_else(|| std::io::Error::other("no listener was bound")))
}

/// Serve `app` on every listener until the graceful-shutdown signal. A
/// listener that fails stops the others.
async fn serve_listeners(
    listeners: Vec<tokio::net::TcpListener>,
    app: Router,
    graceful: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    use std::future::IntoFuture;
    let servers = listeners.into_iter().map(|listener| {
        let mut graceful = graceful.clone();
        axum::serve(
            listener,
            app.clone()
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            while !*graceful.borrow() {
                if graceful.changed().await.is_err() {
                    break;
                }
            }
        })
        .into_future()
    });
    futures_util::future::try_join_all(servers)
        .await
        .map(|_| ())
}

/// The configured credentials for a server on `host`. On loopback,
/// `allow_unauthenticated` turns off only the local token: the Host check
/// that stops DNS rebinding stays, so a same-host proxy must send
/// `Host: localhost` or a loopback IP.
fn server_auth_config(host: &str, auth: &axocoatl_config::ServerAuthYaml) -> auth::AuthConfig {
    auth::AuthConfig::new(auth.api_keys.clone(), auth.bearer_tokens.clone())
        .with_allow_unauthenticated_remote(auth.allow_unauthenticated && !is_loopback_host(host))
}

/// Whether the server on `host` requires the per-daemon local API token: a
/// loopback bind with no configured credentials, unless the operator set
/// `server.auth.allow_unauthenticated`.
pub fn local_token_mode(host: &str, auth: &axocoatl_config::ServerAuthYaml) -> bool {
    is_loopback_host(host) && !auth.is_enabled() && !auth.allow_unauthenticated
}

/// The browser sign-in link for a daemon about to serve on `host:port`, or
/// `None` when the local token does not apply. Creates the token on first
/// start; [`serve_shared`] loads the same file.
pub async fn sign_in_url(
    state: &AppState,
    host: &str,
    port: u16,
) -> std::io::Result<Option<String>> {
    let daemon = state.read().await;
    if !local_token_mode(host, &daemon.config.server.auth) {
        return Ok(None);
    }
    let secret = auth::load_or_create_local_token(daemon.data_root())?;
    Ok(Some(auth::sign_in_url(port, &secret)))
}

/// Start the HTTP server.
pub async fn serve(daemon: AxocoatlDaemon, host: &str, port: u16) -> std::io::Result<()> {
    let state: AppState = Arc::new(RwLock::new(daemon));
    serve_shared(state, host, port).await
}

/// Start the HTTP server with a shared daemon state (for use alongside IPC).
pub async fn serve_shared(state: AppState, host: &str, port: u16) -> std::io::Result<()> {
    // Pull auth + CORS from the live config.
    let (auth, cors_origins, allow_unauthenticated, token_mode, rate_cfg) = {
        let d = state.read().await;
        let s = &d.config.server;
        (
            server_auth_config(host, &s.auth),
            s.cors_origins.clone(),
            s.auth.allow_unauthenticated,
            local_token_mode(host, &s.auth),
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
    // Loopback without configured credentials: only callers that can read
    // the data root (the user's own tools and browser sign-in) may use the
    // API. Session containers never see the data root.
    let local_token = if token_mode {
        let loaded = {
            let d = state.read().await;
            auth::load_or_create_local_token(d.data_root())
        };
        match loaded {
            Ok(secret) => Some(secret),
            Err(error) => {
                let msg = format!("could not load the local API token: {error}");
                tracing::error!("{msg}");
                state.read().await.begin_shutdown();
                let cleanup = state.read().await.shutdown_session_runtimes_checked().await;
                let error = std::io::Error::new(error.kind(), msg);
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => std::io::Error::other(format!(
                        "{error}; Session runtime cleanup after the token failure was incomplete: {cleanup_error}"
                    )),
                });
            }
        }
    } else {
        None
    };
    if local_token.is_some() {
        tracing::info!(
            host,
            "Axocoatl local API requires the per-daemon token; run `axocoatl url` for the sign-in link"
        );
    } else if auth.enabled {
        tracing::info!(host, "Axocoatl API authentication enabled");
    } else if is_loopback_host(host) {
        tracing::warn!(
            host,
            "Axocoatl API authentication disabled by server.auth.allow_unauthenticated — any local process can use the API; requests must still send Host: localhost or a loopback IP"
        );
    } else {
        tracing::warn!(
            host,
            "Axocoatl API authentication disabled by server.auth.allow_unauthenticated — an upstream proxy must enforce it"
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

    let addr = format!("{host}:{port}");
    tracing::info!(addr = %addr, "Starting Axocoatl API server");

    let listeners = match bind_listeners(host, port).await {
        Ok(listeners) => listeners,
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
    // The browser cookie is named after the bound port (it differs from
    // `port` only for an ephemeral `port: 0` bind).
    let auth = match local_token {
        Some(secret) => {
            let bound_port = listeners
                .first()
                .and_then(|listener| listener.local_addr().ok())
                .map_or(port, |address| address.port());
            auth.with_local_token(secret, bound_port)
        }
        None => auth,
    };
    let app = build_router(state.clone(), auth, cors_origins, rate_limiter);
    // Start draining connections as soon as OS or IPC shutdown is requested,
    // while checked runtime cleanup proceeds concurrently. A stuck WebSocket
    // or request gets a bounded grace period; aborting the server then drops
    // that request so its runtime creation lease can roll back and cleanup can
    // finish rather than hanging forever.
    let (graceful_tx, graceful_rx) = tokio::sync::watch::channel(false);
    let mut server_task = tokio::spawn(serve_listeners(listeners, app, graceful_rx));
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
    use super::{bind_listeners, is_loopback_host, local_token_mode, server_auth_config};
    use axocoatl_config::ServerAuthYaml;
    use axum::http::{header, HeaderMap};

    #[test]
    fn allow_unauthenticated_keeps_the_host_check_on_loopback() {
        let open = ServerAuthYaml {
            allow_unauthenticated: true,
            ..ServerAuthYaml::default()
        };
        let mut rebinding = HeaderMap::new();
        rebinding.insert(header::HOST, "attacker.example:8080".parse().unwrap());
        let mut local = HeaderMap::new();
        local.insert(header::HOST, "localhost:8080".parse().unwrap());
        for host in ["127.0.0.1", "localhost", "::1"] {
            let config = server_auth_config(host, &open);
            assert!(!config.enabled, "{host}");
            assert!(
                crate::auth::local_mode_rejects_host(&config, &rebinding),
                "{host}"
            );
            assert!(
                !crate::auth::local_mode_rejects_host(&config, &local),
                "{host}"
            );
        }
        let remote = server_auth_config("0.0.0.0", &open);
        assert!(!crate::auth::local_mode_rejects_host(&remote, &rebinding));
    }

    fn ipv6_loopback_available() -> bool {
        std::net::TcpListener::bind("[::1]:0").is_ok()
    }

    #[tokio::test]
    async fn loopback_listener_holds_the_port_in_both_address_families() {
        for host in ["127.0.0.1", "::1", "[::1]", "localhost"] {
            if host != "127.0.0.1" && !ipv6_loopback_available() {
                continue;
            }
            let listeners = bind_listeners(host, 0).await.unwrap();
            let addresses: Vec<_> = listeners
                .iter()
                .map(|listener| listener.local_addr().unwrap())
                .collect();
            let port = addresses[0].port();
            assert!(addresses.iter().all(|address| address.port() == port));
            assert!(addresses.iter().any(|address| address.is_ipv4()), "{host}");
            if ipv6_loopback_available() {
                assert_eq!(addresses.len(), 2, "{host}");
                assert!(addresses.iter().any(|address| address.is_ipv6()), "{host}");
            }
            for address in addresses {
                let error = std::net::TcpListener::bind(address).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse, "{address}");
            }
        }
    }

    /// A port where `taken` already has a listener and `free` has none.
    fn held_port(taken: &str, free: &str) -> (std::net::TcpListener, u16) {
        for _ in 0..20 {
            let holder = std::net::TcpListener::bind(format!("{taken}:0")).unwrap();
            let port = holder.local_addr().unwrap().port();
            if std::net::TcpListener::bind(format!("{free}:{port}")).is_ok() {
                return (holder, port);
            }
        }
        panic!("no port free on {free} while held on {taken}");
    }

    #[tokio::test]
    async fn a_listener_on_the_other_loopback_family_stops_startup() {
        if !ipv6_loopback_available() {
            return;
        }
        for (taken, host) in [("[::1]", "127.0.0.1"), ("127.0.0.1", "::1")] {
            let (_holder, port) = held_port(taken, host_literal(host));
            let error = bind_listeners(host, port).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse, "{host}");
            assert!(
                error.to_string().contains(&format!("{taken}:{port}")),
                "{error}"
            );
            // The listener on the requested address was released.
            std::net::TcpListener::bind(format!("{}:{port}", host_literal(host))).unwrap();
        }
    }

    fn host_literal(host: &str) -> &str {
        if host == "::1" {
            "[::1]"
        } else {
            host
        }
    }

    #[test]
    fn local_token_applies_to_loopback_without_credentials() {
        let none = ServerAuthYaml::default();
        for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            assert!(local_token_mode(host, &none), "{host}");
        }
        assert!(!local_token_mode("0.0.0.0", &none));
        assert!(!local_token_mode("192.168.1.10", &none));

        let configured = ServerAuthYaml {
            api_keys: vec!["key".into()],
            ..ServerAuthYaml::default()
        };
        assert!(!local_token_mode("127.0.0.1", &configured));
        let bearer = ServerAuthYaml {
            bearer_tokens: vec!["token".into()],
            ..ServerAuthYaml::default()
        };
        assert!(!local_token_mode("127.0.0.1", &bearer));
        let open = ServerAuthYaml {
            allow_unauthenticated: true,
            ..ServerAuthYaml::default()
        };
        assert!(!local_token_mode("127.0.0.1", &open));
    }

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
