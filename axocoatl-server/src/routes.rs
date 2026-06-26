use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;

// --- Dashboard (embedded SPA) ---

const DASHBOARD_HTML: &str = include_str!("../static/index.html");

pub async fn dashboard() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
        .into_response()
}

// --- @axocoatl/lattice — embedded graph-canvas ES modules ---
//
// The dashboard's Studio tab is built on @axocoatl/lattice. Its source files
// are embedded at compile time and served at `/lattice/{file}.js` so the
// browser can import them as a normal ES module graph (no build step).

macro_rules! lattice_modules {
    ($($name:literal),* $(,)?) => {
        fn lattice_module(file: &str) -> Option<&'static str> {
            match file {
                $(
                    concat!($name, ".js") => Some(include_str!(
                        concat!("../../packages/lattice/src/", $name, ".js")
                    )),
                )*
                _ => None,
            }
        }
    };
}

lattice_modules!(
    "index",
    "lattice",
    "node",
    "handle",
    "edge",
    "minimap",
    "controls",
    "viewport",
    "selection",
    "geometry",
    "history",
    "layout",
);

pub async fn lattice_asset(Path(file): Path<String>) -> Response {
    match lattice_module(&file) {
        Some(src) => (
            [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
            src,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "lattice module not found").into_response(),
    }
}

// --- Vendored frontend libraries (highlight.js) ---

/// Serve a vendored static asset embedded at compile time. Keeps the dashboard
/// fully self-contained — no CDN, works offline.
/// Serve a brand-kit asset from `branding/`.  Single-mark system: the
/// canonical mark.png + favicon.png + the wordmark family + colors.json.
/// Embedded at compile time so the daemon is self-contained.
pub async fn brand_asset(Path(file): Path<String>) -> Response {
    let (body, ctype): (&'static [u8], &str) = match file.as_str() {
        "mark.png" => (include_bytes!("../../branding/mark.png"), "image/png"),
        "favicon.png" => (include_bytes!("../../branding/favicon.png"), "image/png"),
        "wordmark.png" => (include_bytes!("../../branding/wordmark.png"), "image/png"),
        "wordmark-ink.png" => (
            include_bytes!("../../branding/wordmark-ink.png"),
            "image/png",
        ),
        "wordmark-vellum.png" => (
            include_bytes!("../../branding/wordmark-vellum.png"),
            "image/png",
        ),
        "colors.json" => (
            include_bytes!("../../branding/colors.json"),
            "application/json",
        ),
        _ => return (StatusCode::NOT_FOUND, "brand asset not found").into_response(),
    };
    ([(header::CONTENT_TYPE, ctype)], body).into_response()
}

/// The DOM-picker tap script injected into proxied pages. Served at a
/// fixed path so the proxy injector can reference it once.
pub async fn axo_tap_script() -> Response {
    let body = include_str!("../static/axo-tap.js");
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        body,
    )
        .into_response()
}

/// All vendor assets are embedded at compile time via `rust_embed`. Nested
/// paths work (Monaco's `vs/loader.js`, `vs/editor/editor.main.js`,
/// `vs/basic-languages/{lang}/{lang}.js`, etc.) without needing one match
/// arm per file.
#[derive(rust_embed::RustEmbed)]
#[folder = "static/vendor/"]
struct VendorAssets;

pub async fn vendor_asset(Path(file): Path<String>) -> Response {
    let Some(content) = VendorAssets::get(&file) else {
        return (StatusCode::NOT_FOUND, "vendor asset not found").into_response();
    };
    let ctype = mime_guess::from_path(&file)
        .first_or_octet_stream()
        .as_ref()
        .to_string();
    ([(header::CONTENT_TYPE, ctype)], content.data.into_owned()).into_response()
}

// --- Health endpoints ---

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub agents: usize,
}

pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let daemon = state.read().await;
    Json(HealthResponse {
        status: "healthy".to_string(),
        agents: daemon.agent_count().await,
    })
}

pub async fn health_ready(State(state): State<AppState>) -> StatusCode {
    let daemon = state.read().await;
    if daemon.agent_count().await > 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub async fn health_live() -> StatusCode {
    StatusCode::OK
}

#[derive(Serialize)]
pub struct LlmHealthResponse {
    pub ollama: Option<OllamaHealth>,
}

#[derive(Serialize)]
pub struct OllamaHealth {
    pub base_url: String,
    pub reachable: bool,
    pub configured: bool,
    pub missing_models: Vec<String>,
}

/// Lightweight provider-reachability probe for the dashboard. Currently only
/// checks Ollama (the default local provider) — if it's down or models aren't
/// pulled, we want the first-time user to see a one-line toast pointing them
/// at `ollama serve` + `ollama pull` instead of just a generic "agent failed".
pub async fn llm_health(State(state): State<AppState>) -> Json<LlmHealthResponse> {
    let daemon = state.read().await;
    let cfg = &daemon.config;
    let ollama = if let Some(o) = &cfg.providers.ollama {
        let wanted: std::collections::HashSet<String> = cfg
            .agents
            .iter()
            .filter(|a| a.provider == "ollama")
            .map(|a| {
                if a.model.is_empty() {
                    o.model.clone().unwrap_or_else(|| "llama3.2".to_string())
                } else {
                    a.model.clone()
                }
            })
            .collect();
        let (reachable, missing_models) = match ollama_tags(&o.base_url).await {
            Ok(present) => {
                let missing: Vec<String> = wanted
                    .into_iter()
                    .filter(|w| {
                        !present
                            .iter()
                            .any(|p| p == w || p.starts_with(&format!("{w}:")))
                    })
                    .collect();
                (true, missing)
            }
            Err(_) => (false, wanted.into_iter().collect()),
        };
        Some(OllamaHealth {
            base_url: o.base_url.clone(),
            reachable,
            configured: true,
            missing_models,
        })
    } else {
        None
    };
    Json(LlmHealthResponse { ollama })
}

async fn ollama_tags(base_url: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let models = json
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
}

// --- Agent endpoints ---

#[derive(Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub depends_on: Vec<String>,
    pub team: String,
    /// "autonomous" | "coordinator" | "worker" — lets the UI mark coordinators
    /// (which own a Layer-2 run view) distinctly.
    pub role: String,
    pub system_prompt: Option<String>,
    pub per_call_budget: Option<usize>,
    pub per_execution_budget: Option<usize>,
    pub overflow_policy: Option<String>,
}

/// Group agents into a "team" by their first dependency / role. Heuristic
/// for the UI clustering — purely cosmetic.
fn team_of(agent_id: &str) -> &'static str {
    match agent_id {
        "architect" | "planner" | "coder" | "reviewer" | "tester" | "doc-writer" => "Engineering",
        "researcher" | "summarizer" | "analyst" => "Research",
        "ops" => "Ops",
        "support" | "secretary" => "Customer",
        _ => "General",
    }
}

pub async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentInfo>> {
    let daemon = state.read().await;
    let agents: Vec<AgentInfo> = daemon
        .config
        .agents
        .iter()
        .map(|a| AgentInfo {
            id: a.id.clone(),
            name: a.name.clone(),
            provider: a.provider.clone(),
            model: a.model.clone(),
            depends_on: a.depends_on.clone(),
            team: team_of(&a.id).to_string(),
            role: format!("{:?}", a.role).to_lowercase(),
            system_prompt: a.system_prompt.clone(),
            per_call_budget: a.token_budget.as_ref().map(|b| b.per_call),
            per_execution_budget: a.token_budget.as_ref().map(|b| b.per_execution),
            overflow_policy: a
                .token_budget
                .as_ref()
                .map(|b| format!("{:?}", b.overflow_policy).to_lowercase()),
        })
        .collect();
    Json(agents)
}

#[derive(Deserialize, Default)]
pub struct AgentPatch {
    pub name: Option<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub depends_on: Option<Vec<String>>,
    pub per_call_budget: Option<usize>,
    pub per_execution_budget: Option<usize>,
    pub overflow_policy: Option<String>,
    pub restart_now: Option<bool>,
}

#[derive(Serialize)]
pub struct AgentPatchResponse {
    pub agent_id: String,
    pub restarted: bool,
    pub message: String,
}

/// Update an agent's in-memory config. The next time the agent is restarted
/// (or if `restart_now: true`) the new prompt/model/budget take effect.
/// This is in-memory only for this session — save-to-YAML is a later session.
pub async fn patch_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<AgentPatch>,
) -> Result<Json<AgentPatchResponse>, (StatusCode, Json<ErrorResponse>)> {
    use axocoatl_config::OverflowPolicyYaml;

    // Update the config in-memory (write lock).
    {
        let mut daemon = state.write().await;
        let agent = daemon
            .config
            .agents
            .iter_mut()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        error: format!("Agent '{agent_id}' not found"),
                    }),
                )
            })?;
        if let Some(n) = body.name {
            agent.name = n;
        }
        if let Some(m) = body.model {
            agent.model = m;
        }
        if let Some(sp) = body.system_prompt {
            agent.system_prompt = Some(sp);
        }
        if let Some(d) = body.depends_on {
            agent.depends_on = d;
        }
        if body.per_call_budget.is_some()
            || body.per_execution_budget.is_some()
            || body.overflow_policy.is_some()
        {
            let mut b = agent
                .token_budget
                .clone()
                .unwrap_or(axocoatl_config::TokenBudgetYaml {
                    per_call: 4096,
                    per_execution: 16000,
                    overflow_policy: OverflowPolicyYaml::Warn,
                });
            if let Some(v) = body.per_call_budget {
                b.per_call = v;
            }
            if let Some(v) = body.per_execution_budget {
                b.per_execution = v;
            }
            if let Some(p) = body.overflow_policy {
                b.overflow_policy = match p.as_str() {
                    "abort" => OverflowPolicyYaml::Abort,
                    "warn" => OverflowPolicyYaml::Warn,
                    "summarize" => OverflowPolicyYaml::Summarize,
                    _ => b.overflow_policy,
                };
            }
            agent.token_budget = Some(b);
        }
    }

    let want_restart = body.restart_now.unwrap_or(true);
    let mut restarted = false;
    if want_restart {
        let daemon = state.read().await;
        match daemon.restart_agent(&agent_id).await {
            Ok(()) => {
                restarted = true;
            }
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("Patch saved but restart failed: {e}"),
                    }),
                ))
            }
        }
    }

    Ok(Json(AgentPatchResponse {
        agent_id: agent_id.clone(),
        restarted,
        message: if restarted {
            format!("Agent '{agent_id}' updated and restarted — changes are live (in-memory; YAML unchanged).")
        } else {
            format!("Agent '{agent_id}' updated. Restart to apply.")
        },
    }))
}

#[derive(Deserialize)]
pub struct ExecuteRequest {
    pub input: String,
    /// Per-request system-prompt override for this single execution — replaces
    /// the agent's configured prompt without changing config. Mirrors the
    /// session/chat override; used to A/B-test a prompt variant of an agent.
    #[serde(default)]
    pub system_override: Option<String>,
    /// Per-request model override (same provider + credentials) for this single
    /// execution — used to A/B-test a model variant of an agent.
    #[serde(default)]
    pub model_override: Option<String>,
}

#[derive(Serialize)]
pub struct ExecuteResponse {
    pub output: String,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

pub async fn execute_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let input = axocoatl_core::AgentInput::text(body.input)
        .with_system_override(body.system_override)
        .with_model_override(body.model_override);
    match daemon.execute_agent_input(&agent_id, input).await {
        Ok(output) => Ok(Json(ExecuteResponse {
            output: output.content,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

#[derive(Serialize)]
pub struct AgentStatusResponse {
    pub agent_id: String,
    pub status: String,
}

pub async fn agent_status(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let id = axocoatl_core::AgentId::new(&agent_id);

    match daemon.agent_registry.get(&id).await {
        Some(actor) => {
            let status = axocoatl_actor::get_agent_status(&actor)
                .await
                .unwrap_or_else(|e| axocoatl_core::AgentStatus::Failed {
                    error: e,
                    restarts: 0,
                });
            Ok(Json(AgentStatusResponse {
                agent_id,
                status: format!("{:?}", status),
            }))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent '{}' not found", agent_id),
            }),
        )),
    }
}

// --- Workflow endpoints ---

#[derive(Serialize)]
pub struct WorkflowInfo {
    pub id: String,
    pub name: String,
    pub agents: Vec<String>,
    pub entry_point: Option<String>,
}

pub async fn list_workflows(State(state): State<AppState>) -> Json<Vec<WorkflowInfo>> {
    let daemon = state.read().await;
    let workflows: Vec<WorkflowInfo> = daemon
        .config
        .workflows
        .iter()
        .map(|w| WorkflowInfo {
            id: w.id.clone(),
            name: w.name.clone(),
            agents: w.agents.clone(),
            entry_point: w.entry_point.clone(),
        })
        .collect();
    Json(workflows)
}

#[derive(Serialize)]
pub struct WorkflowResponse {
    pub workflow_id: String,
    pub output: String,
    pub agent_outputs: Vec<WorkflowAgentOutput>,
    pub total_tokens: usize,
    pub completed_agents: Vec<String>,
    pub failed_agents: Vec<WorkflowFailedAgent>,
}

#[derive(Serialize)]
pub struct WorkflowAgentOutput {
    pub agent_id: String,
    pub content: String,
    pub tokens: usize,
}

#[derive(Serialize)]
pub struct WorkflowFailedAgent {
    pub agent_id: String,
    pub error: String,
}

pub async fn execute_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
    Json(body): Json<ExecuteRequest>,
) -> Result<Json<WorkflowResponse>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    match daemon.execute_workflow(&workflow_id, &body.input).await {
        Ok(output) => Ok(Json(WorkflowResponse {
            workflow_id: output.workflow_id,
            output: output.final_content,
            agent_outputs: output
                .agent_outputs
                .into_iter()
                .map(|(id, o)| WorkflowAgentOutput {
                    agent_id: id,
                    content: o.content,
                    tokens: o.token_usage.total(),
                })
                .collect(),
            total_tokens: output.total_token_usage.total(),
            completed_agents: output.completed_agents,
            failed_agents: output
                .failed_agents
                .into_iter()
                .map(|(id, e)| WorkflowFailedAgent {
                    agent_id: id,
                    error: e,
                })
                .collect(),
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

// --- Token endpoints ---

#[derive(Serialize)]
pub struct AgentTokenUsage {
    pub agent_id: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
}

#[derive(Serialize)]
pub struct TokenReport {
    pub per_agent: Vec<AgentTokenUsage>,
    pub total_input: usize,
    pub total_output: usize,
    pub total: usize,
}

pub async fn token_report(State(state): State<AppState>) -> Json<TokenReport> {
    let daemon = state.read().await;
    let mut per_agent = Vec::new();
    let mut total_input = 0;
    let mut total_output = 0;
    for id in daemon.agent_registry.list_ids().await {
        if let Some(actor) = daemon.agent_registry.get(&id).await {
            if let Ok(u) = axocoatl_actor::get_agent_token_usage(&actor).await {
                total_input += u.input_tokens;
                total_output += u.output_tokens;
                per_agent.push(AgentTokenUsage {
                    agent_id: id.to_string(),
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    total_tokens: u.input_tokens + u.output_tokens,
                });
            }
        }
    }
    Json(TokenReport {
        per_agent,
        total_input,
        total_output,
        total: total_input + total_output,
    })
}

// --- MCP endpoints ---

#[derive(Serialize)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,
    pub tool_count: usize,
}

pub async fn list_mcp_servers(State(state): State<AppState>) -> Json<Vec<McpServerEntry>> {
    let daemon = state.read().await;
    let reg = daemon.mcp_registry.read().await;
    let servers = reg
        .servers()
        .into_iter()
        .map(|s| McpServerEntry {
            name: s.name.clone(),
            transport: s.transport_type.clone(),
            tool_count: s.tool_count,
        })
        .collect();
    Json(servers)
}

#[derive(Serialize)]
pub struct McpToolEntry {
    pub name: String,
    pub server: String,
    pub description: String,
}

pub async fn list_mcp_tools(State(state): State<AppState>) -> Json<Vec<McpToolEntry>> {
    let daemon = state.read().await;
    let reg = daemon.mcp_registry.read().await;
    let tools = reg
        .tool_entries()
        .into_iter()
        .map(|(name, server, description)| McpToolEntry {
            name,
            server,
            description,
        })
        .collect();
    Json(tools)
}

/// Serve the curated MCP catalog. Bundled at compile time so it works
/// offline; the dashboard renders the Gallery from this JSON.
const MCP_CATALOG: &str = include_str!("../../branding/mcp-catalog.json");
pub async fn mcp_catalog() -> Response {
    let mut resp = Response::new(axum::body::Body::from(MCP_CATALOG));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/json; charset=utf-8".parse().unwrap(),
    );
    resp
}

// ── MCP permissions audit + revoke ────────────────────────────────
pub async fn list_mcp_permissions(
    State(state): State<AppState>,
) -> Json<Vec<axocoatl_mcp::permissions::PermissionRecord>> {
    let daemon = state.read().await;
    let perms = daemon.mcp_permissions.read().await;
    Json(perms.list().to_vec())
}

#[derive(serde::Deserialize)]
pub struct RevokePermissionQuery {
    pub server: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
}

pub async fn revoke_mcp_permission(
    State(state): State<AppState>,
    Query(q): Query<RevokePermissionQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let mut perms = daemon.mcp_permissions.write().await;
    let removed = perms
        .revoke(q.agent_id.as_deref(), &q.server, q.tool.as_deref())
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed })))
}

/// Re-dial an already-connected MCP server (uses its cached transport).
pub async fn reconnect_mcp(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    match daemon.reconnect_mcp_server(&name).await {
        Ok(tool_count) => Ok(Json(serde_json::json!({
            "ok": true, "name": name, "tools": tool_count
        }))),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e.to_string())),
    }
}

/// Drop an MCP server from the registry (the dashboard's Remove button).
pub async fn remove_mcp(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    match daemon.remove_mcp_server(&name).await {
        Ok(removed) => Ok(Json(serde_json::json!({ "ok": true, "removed": removed }))),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e.to_string())),
    }
}

/// Install a server from the catalog into the running mcp_servers list.
/// Body: `{ slug, env: {KEY: value, …}, requires: {key: value} }`.
/// We resolve the template (substituting `{{KEY}}` in args/env with the
/// user's values), build an McpTransportType, ask the registry to connect,
/// and on success append to the running config so subsequent boots keep it.
#[derive(serde::Deserialize)]
pub struct InstallMcpBody {
    pub slug: String,
    /// User-supplied values for the `requires` fields in the catalog entry.
    #[serde(default)]
    pub values: std::collections::HashMap<String, String>,
    /// Optional override for the server name (defaults to slug).
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn install_mcp(
    State(state): State<AppState>,
    Json(body): Json<InstallMcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Parse the bundled catalog and locate the requested slug.
    let catalog: serde_json::Value = serde_json::from_str(MCP_CATALOG).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("catalog parse: {e}"),
        )
    })?;
    let entry = catalog["servers"]
        .as_array()
        .and_then(|arr| arr.iter().find(|e| e["slug"].as_str() == Some(&body.slug)))
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                format!("catalog slug '{}' not found", body.slug),
            )
        })?
        .clone();

    // Substitute {{KEY}} placeholders in args + env with provided values.
    let substitute = |s: &str| -> String {
        let mut out = s.to_string();
        for (k, v) in &body.values {
            out = out.replace(&format!("{{{{{k}}}}}"), v);
        }
        out
    };

    let transport = entry["transport"].as_str().unwrap_or("stdio");
    let server_name = body.name.unwrap_or_else(|| body.slug.clone());

    let mcp_transport = match transport {
        "stdio" => {
            let command = entry["command"].as_str().unwrap_or("").to_string();
            let args: Vec<String> = entry["args_template"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(&substitute)
                        .collect()
                })
                .unwrap_or_default();
            // Most stdio servers take their secret via an env var (e.g.
            // BRAVE_API_KEY, GITHUB_PERSONAL_ACCESS_TOKEN). Without this the
            // dashboard's entered values are dropped and the server exits
            // before the initialize handshake.
            let env: std::collections::HashMap<String, String> = entry["env_template"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), substitute(s))))
                        .collect()
                })
                .unwrap_or_default();
            axocoatl_mcp::McpTransportType::Stdio { command, args, env }
        }
        "streamable_http" | "http" => {
            let url = substitute(entry["url"].as_str().unwrap_or(""));
            let headers: std::collections::HashMap<String, String> = entry["env_template"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), substitute(s))))
                        .collect()
                })
                .unwrap_or_default();
            axocoatl_mcp::McpTransportType::StreamableHttp { url, headers }
        }
        other => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("unsupported transport: {other}"),
            ));
        }
    };

    // Connect the server through the daemon's helper.
    let daemon = state.write().await;
    match daemon.connect_mcp_server(&server_name, mcp_transport).await {
        Ok(tool_count) => Ok(Json(serde_json::json!({
            "ok": true,
            "name": server_name,
            "tools": tool_count
        }))),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e.to_string())),
    }
}

// --- Schedules ---

#[derive(Serialize)]
pub struct ScheduleEntry {
    pub id: String,
    pub name: String,
    pub workflow: String,
    pub every: String,
    pub input: String,
    pub enabled: bool,
    pub interval_secs: u64,
    pub last_fired_unix: Option<u64>,
    pub next_fire_unix: Option<u64>,
    pub last_outcome: Option<String>,
    pub run_count: u64,
}

pub async fn list_schedules(State(state): State<AppState>) -> Json<Vec<ScheduleEntry>> {
    let daemon = state.read().await;
    let table = daemon.schedule_table.clone();
    drop(daemon);
    let entries = table
        .lock()
        .map(|v| {
            v.iter()
                .map(|s| ScheduleEntry {
                    id: s.config.id.clone(),
                    name: s.config.name.clone(),
                    workflow: s.config.workflow.clone(),
                    every: s.config.every.clone(),
                    input: s.config.input.clone(),
                    enabled: s.config.enabled,
                    interval_secs: s.interval_secs,
                    last_fired_unix: s.last_fired_unix,
                    next_fire_unix: s.next_fire_unix(),
                    last_outcome: s.last_outcome.clone(),
                    run_count: s.run_count,
                })
                .collect()
        })
        .unwrap_or_default();
    Json(entries)
}

// --- Directory sessions ---

#[derive(serde::Deserialize)]
pub struct CreateSessionBody {
    pub name: String,
    pub working_dir: String,
    /// Run mode — `{"kind":"single_agent","agent_id":"coder"}` or
    /// `{"kind":"lattice"}`.
    pub mode: axocoatl_session::SessionMode,
    /// Skill ids the session's agents may fire as tools.
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Ports to publish from the sandbox container to the host. Empty falls
    /// back to a sensible default set.
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    /// Base OCI image for the session sandbox. `None` means "use the
    /// configured default" (alpine, unless devcontainer.json overrides).
    #[serde(default)]
    pub image: Option<String>,
}

pub async fn list_sessions(State(state): State<AppState>) -> Json<Vec<axocoatl_session::Session>> {
    Json(state.read().await.list_sessions().await)
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .create_session(
            &body.name,
            &body.working_dir,
            body.mode,
            body.enabled_skills,
            body.exposed_ports,
            body.image,
        )
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

/// GET /api/sessions/{id}/messages — the session's persisted transcript, used
/// to rehydrate the cockpit on reopen and to address turns for rewind.
pub async fn session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<axocoatl_memory::session::StoredMessage>>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon.session_messages(&id).await.map(Json).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })
}

#[derive(serde::Deserialize)]
pub struct RewindSessionBody {
    /// Number of leading transcript messages to keep; everything after is dropped.
    pub keep: usize,
}

/// POST /api/sessions/{id}/rewind — truncate the transcript to `keep` messages
/// and drop the live actor so the next turn resumes from the truncated state.
pub async fn rewind_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RewindSessionBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .rewind_session(&id, body.keep)
        .await
        .map(|_| Json(serde_json::json!({ "ok": true })))
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

// ── Git pane routes — a session is (auto-)a git repo ────────────────────
fn git_err(e: impl std::fmt::Display) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: e.to_string(),
        }),
    )
}

/// GET /api/sessions/{id}/git/status
pub async fn git_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon.git_status(&id).await.map(Json).map_err(git_err)
}

#[derive(serde::Deserialize)]
pub struct GitPathQuery {
    pub path: String,
}

/// GET /api/sessions/{id}/git/diff?path=…
pub async fn git_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<GitPathQuery>,
) -> Result<Json<axocoatl_daemon::git::GitDiff>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .git_diff(&id, &q.path)
        .await
        .map(Json)
        .map_err(git_err)
}

/// GET /api/sessions/{id}/git/branches
pub async fn git_branches(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::git::GitBranches>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon.git_branches(&id).await.map(Json).map_err(git_err)
}

#[derive(serde::Deserialize)]
pub struct GitCommitBody {
    pub message: Option<String>,
}

/// POST /api/sessions/{id}/git/commit
pub async fn git_commit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitCommitBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .git_commit(&id, body.message.as_deref().unwrap_or(""))
        .await
        .map(Json)
        .map_err(git_err)
}

#[derive(serde::Deserialize)]
pub struct GitDiscardBody {
    pub path: Option<String>,
}

/// POST /api/sessions/{id}/git/discard
pub async fn git_discard(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitDiscardBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .git_discard(&id, body.path.as_deref())
        .await
        .map(Json)
        .map_err(git_err)
}

#[derive(serde::Deserialize)]
pub struct GitCheckoutBody {
    #[serde(rename = "ref")]
    pub reference: String,
}

/// POST /api/sessions/{id}/git/checkout
pub async fn git_checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitCheckoutBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .git_checkout(&id, &body.reference)
        .await
        .map(Json)
        .map_err(git_err)
}

// ── Variants — parallel branch exploration ──────────────────────────────
fn default_variant_count() -> usize {
    3
}

#[derive(serde::Deserialize)]
pub struct VariantsBody {
    pub input: String,
    #[serde(default = "default_variant_count")]
    pub n: usize,
}

/// POST /api/sessions/{id}/variants — start N parallel variants of the
/// session's agent. Returns the variant list; each lane streams over the WS
/// keyed `{session}#{i}`.
pub async fn session_variants(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VariantsBody>,
) -> Result<Json<Vec<axocoatl_daemon::git::Variant>>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .execute_session_variants(&id, &body.input, body.n, None)
        .await
        .map(Json)
        .map_err(git_err)
}

/// GET /api/sessions/{id}/variants/status — per-lane changed-files for the
/// Compare view.
pub async fn session_variants_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<axocoatl_daemon::git::VariantStatus>>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon.variants_status(&id).await.map(Json).map_err(git_err)
}

#[derive(serde::Deserialize)]
pub struct AdoptBody {
    pub branch: String,
}

/// POST /api/sessions/{id}/variants/adopt — merge a variant branch into the
/// session's primary checkout and tear down the rest.
pub async fn session_variant_adopt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AdoptBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .adopt_variant(&id, &body.branch)
        .await
        .map(Json)
        .map_err(git_err)
}

/// POST /api/sessions/{id}/variants/discard — tear down all variants without
/// adopting any.
pub async fn session_variants_discard(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .remove_variant_worktrees(&id)
        .await
        .map(|_| Json(serde_json::json!({ "ok": true })))
        .map_err(git_err)
}

pub async fn execute_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    match daemon.execute_session(&id, &body.input).await {
        Ok(output) => Ok(Json(ExecuteResponse {
            output: output.content,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct CloseSessionQuery {
    /// When true, the session is fully deleted (JSON removed from disk).
    /// Otherwise it's a soft close: status = closed, container stopped, but
    /// the session can be reopened.
    #[serde(default)]
    pub force: bool,
}

pub async fn close_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<CloseSessionQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let result = if q.force {
        daemon.delete_session(&id).await
    } else {
        daemon.close_session(&id).await
    };
    result
        .map(|_| Json(serde_json::json!({ "ok": true, "deleted": q.force })))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

#[derive(serde::Deserialize)]
pub struct RenameSessionBody {
    pub name: String,
}

pub async fn rename_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RenameSessionBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .rename_session(&id, &body.name)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

// ─── Chats (lightweight, no directory) ────────────────────────────────
// Backend for the dashboard's Chat tab. Distinct from sessions — see
// crates/axocoatl-memory/src/chat.rs for the storage model and rationale.

#[derive(Deserialize)]
pub struct CreateChatBody {
    pub agent_id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchChatBody {
    /// Rename the chat.
    #[serde(default)]
    pub name: Option<String>,
    /// Star/unstar.
    #[serde(default)]
    pub starred: Option<bool>,
    /// Per-chat system prompt override. `Some(None)` means clear; `None` means leave alone.
    /// Use serde's `default` so the field can be omitted to mean "no change".
    #[serde(default, with = "double_option")]
    pub system_override: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub model_override: Option<Option<String>>,
}

// Helper for "field omitted vs explicit null" semantics on Option<Option<T>>.
// PatchChatBody only deserializes, but serde requires both fns be visible.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    #[allow(dead_code)]
    pub fn serialize<S, T>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        v.as_ref().map(|x| x.as_ref()).serialize(s)
    }
    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<Option<T>>::deserialize(d)
    }
}

#[derive(Deserialize)]
pub struct ForkChatBody {
    pub truncate_at: usize,
    /// Optional edited message to push onto the forked prefix. Common case:
    /// user clicks "edit and branch" on their last message — the new wording
    /// arrives here, the executor runs the chat from there.
    #[serde(default)]
    pub replacement_content: Option<String>,
    /// Role of the replacement message — defaults to User (the typical case).
    #[serde(default)]
    pub replacement_role: Option<axocoatl_core::MessageRole>,
}

#[derive(Deserialize)]
pub struct ChatSearchQuery {
    pub q: Option<String>,
}

pub async fn list_chats(
    State(state): State<AppState>,
    Query(q): Query<ChatSearchQuery>,
) -> Json<Vec<axocoatl_memory::chat::Chat>> {
    let daemon = state.read().await;
    match q.q {
        Some(query) if !query.trim().is_empty() => Json(daemon.search_chats(&query).await),
        _ => Json(daemon.list_chats().await),
    }
}

pub async fn create_chat(
    State(state): State<AppState>,
    Json(body): Json<CreateChatBody>,
) -> Result<Json<axocoatl_memory::chat::Chat>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let name = body.name.unwrap_or_else(|| "New chat".to_string());
    daemon
        .create_chat(&body.agent_id, &name)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

pub async fn get_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_memory::chat::Chat>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon.get_chat(&id).await.map(Json).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("chat {id} not found"),
        }),
    ))
}

pub async fn patch_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchChatBody>,
) -> Result<Json<axocoatl_memory::chat::Chat>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    // Apply each present field in turn — keep the last result so the response
    // reflects all updates. PatchChatBody lets a client batch rename + star
    // + overrides in one call. Empty body = no-op (returns current state).
    let mut current = daemon.get_chat(&id).await.ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("chat {id} not found"),
        }),
    ))?;
    if let Some(name) = body.name {
        current = daemon.rename_chat(&id, &name).await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    }
    if let Some(starred) = body.starred {
        current = daemon.star_chat(&id, starred).await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    }
    if body.system_override.is_some() || body.model_override.is_some() {
        let sys = body
            .system_override
            .unwrap_or(current.system_override.clone());
        let mdl = body
            .model_override
            .unwrap_or(current.model_override.clone());
        current = daemon
            .set_chat_overrides(&id, sys, mdl)
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?;
    }
    Ok(Json(current))
}

pub async fn delete_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .delete_chat(&id)
        .await
        .map(|_| Json(serde_json::json!({ "ok": true })))
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

// ── Chat attachments (multipart upload + static serve) ────────────
// Bytes land at {data_dir}/chat-attachments/{chat_id}/{file_id}.{ext}
// and are registered against the chat via ChatStore::add_attachment.
// The next ChatTurn consumes the pending list and the executor inlines
// the bytes into the LLM call (base64 image parts or `<attachment>` text
// blocks — see crates/axocoatl-actor/src/default_behavior.rs).

/// Max sizes per type. The user can adjust by editing constants if needed.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
const MAX_TEXT_BYTES: usize = 1024 * 1024; // 1 MB

pub async fn upload_chat_attachment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<axocoatl_memory::files::FileEntry>, (StatusCode, Json<ErrorResponse>)> {
    // Verify the chat exists before we touch the filesystem.
    let exists = {
        let daemon = state.read().await;
        daemon.get_chat(&id).await.is_some()
    };
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("chat {id} not found"),
            }),
        ));
    }

    let field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break Some(f),
            Ok(Some(_)) => continue,
            Ok(None) => break None,
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("multipart error: {e}"),
                    }),
                ));
            }
        }
    };
    let field = field.ok_or((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: "missing 'file' field".to_string(),
        }),
    ))?;

    let filename = field.file_name().unwrap_or("attachment").to_string();
    let mime = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = field.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("read failed: {e}"),
            }),
        )
    })?;

    // Size cap by type. Larger than before because we now properly cache to
    // disk (no re-upload needed across turns) and PDFs are valuable.
    let max = if mime.starts_with("image/") {
        MAX_IMAGE_BYTES
    } else {
        MAX_TEXT_BYTES
    };
    if bytes.len() > max {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse {
                error: format!(
                    "file is {} bytes; max for type {} is {}",
                    bytes.len(),
                    mime,
                    max
                ),
            }),
        ));
    }

    // Store in FileStore (content-addressed; dedup'd; extraction runs once).
    let entry = {
        let daemon = state.read().await;
        let fs = daemon.file_store.clone();
        let mime_for_extract = mime.clone();
        let name_for_extract = filename.clone();
        let mut guard = fs.lock().await;
        guard
            .store_with(&bytes, &filename, &mime, move |b, m| {
                axocoatl_memory::extract::extract(b, m, &name_for_extract)
            })
            .map_err(|e| {
                let _ = mime_for_extract;
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?
    };

    // Register the reference against the chat.
    {
        let daemon = state.read().await;
        daemon
            .chat_store
            .lock()
            .await
            .add_attachment(&id, &entry.id)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: e.to_string(),
                    }),
                )
            })?;
    }
    Ok(Json(entry))
}

// ─── /api/files — the cross-chat file browser ────────────────────────
// Content-addressed reads from the FileStore. The Files browser tab
// lists, searches, previews, renames, and deletes. Deleting a file here
// also cleans up any chat reference (callers shouldn't see broken refs).

#[derive(Deserialize)]
pub struct FilesQuery {
    pub q: Option<String>,
}

pub async fn list_files(
    State(state): State<AppState>,
    Query(q): Query<FilesQuery>,
) -> Json<Vec<axocoatl_memory::files::FileEntry>> {
    let file_store = {
        let daemon = state.read().await;
        daemon.file_store.clone()
    };
    let guard = file_store.lock().await;
    let out = match q.q {
        Some(s) if !s.trim().is_empty() => guard.search(&s),
        _ => guard.list(),
    };
    Json(out)
}

pub async fn get_file_meta(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_memory::files::FileEntry>, (StatusCode, Json<ErrorResponse>)> {
    let file_store = {
        let daemon = state.read().await;
        daemon.file_store.clone()
    };
    let guard = file_store.lock().await;
    guard.get(&id).map(Json).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("file {id} not found"),
        }),
    ))
}

pub async fn get_file_bytes(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let file_store = {
        let daemon = state.read().await;
        daemon.file_store.clone()
    };
    let (entry, bytes) = {
        let g = file_store.lock().await;
        let entry = g.get(&id).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("file {id} not found"),
            }),
        ))?;
        let bytes = g.read_bytes(&id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("read failed: {e}"),
                }),
            )
        })?;
        (entry, bytes)
    };
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        entry
            .mime
            .parse()
            .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
    );
    Ok(resp)
}

#[derive(Deserialize)]
pub struct PatchFileBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

pub async fn patch_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchFileBody>,
) -> Result<Json<axocoatl_memory::files::FileEntry>, (StatusCode, Json<ErrorResponse>)> {
    let file_store = {
        let daemon = state.read().await;
        daemon.file_store.clone()
    };
    let mut g = file_store.lock().await;
    let mut current = g.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("file {id} not found"),
        }),
    ))?;
    if let Some(n) = body.name {
        current = g.rename(&id, &n).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    }
    if let Some(tags) = body.tags {
        current = g.set_tags(&id, tags).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    }
    Ok(Json(current))
}

pub async fn delete_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // 1) Drop the file from FileStore. 2) Walk every chat and remove any
    // attachment ref pointing at this id (no orphan references left).
    let (file_store, chat_store) = {
        let daemon = state.read().await;
        (daemon.file_store.clone(), daemon.chat_store.clone())
    };
    file_store.lock().await.remove(&id).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
    })?;
    {
        let mut g = chat_store.lock().await;
        let chats: Vec<String> = g.list().into_iter().map(|c| c.id).collect();
        for cid in chats {
            let _ = g.remove_attachment(&cid, &id);
        }
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Upload to the global FileStore without referencing any chat. Used by the
/// Files-tab uploader. Reuses the multipart parsing from the chat-upload
/// route (slight duplication; both routes share the same shape).
pub async fn upload_file(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<axocoatl_memory::files::FileEntry>, (StatusCode, Json<ErrorResponse>)> {
    let field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break Some(f),
            Ok(Some(_)) => continue,
            Ok(None) => break None,
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("multipart error: {e}"),
                    }),
                ));
            }
        }
    };
    let field = field.ok_or((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: "missing 'file' field".to_string(),
        }),
    ))?;
    let filename = field.file_name().unwrap_or("file").to_string();
    let mime = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = field.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("read failed: {e}"),
            }),
        )
    })?;
    let max = if mime.starts_with("image/") {
        MAX_IMAGE_BYTES
    } else {
        MAX_TEXT_BYTES
    };
    if bytes.len() > max {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse {
                error: format!(
                    "file is {} bytes; max for type {} is {}",
                    bytes.len(),
                    mime,
                    max
                ),
            }),
        ));
    }
    let file_store = {
        let daemon = state.read().await;
        daemon.file_store.clone()
    };
    let name_for_extract = filename.clone();
    let entry = file_store
        .lock()
        .await
        .store_with(&bytes, &filename, &mime, move |b, m| {
            axocoatl_memory::extract::extract(b, m, &name_for_extract)
        })
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    Ok(Json(entry))
}

/// Attach an already-uploaded FileStore entry to a chat — the cross-chat
/// "attach from My Files" path. Body: `{ file_id: string }`.
#[derive(Deserialize)]
pub struct AttachFromFilesBody {
    pub file_id: String,
}
pub async fn attach_file_to_chat(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    Json(body): Json<AttachFromFilesBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let (file_store, chat_store) = {
        let daemon = state.read().await;
        (daemon.file_store.clone(), daemon.chat_store.clone())
    };
    let exists = file_store.lock().await.get(&body.file_id).is_some();
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("file {} not found", body.file_id),
            }),
        ));
    }
    chat_store
        .lock()
        .await
        .add_attachment(&chat_id, &body.file_id)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Remove an attachment reference from a chat. The underlying FileStore
/// entry is NOT deleted — other chats may reference the same file. Use
/// /api/files/{file_id} DELETE to truly remove a file.
pub async fn delete_chat_attachment(
    State(state): State<AppState>,
    Path((chat_id, file_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let chat_store = {
        let daemon = state.read().await;
        daemon.chat_store.clone()
    };
    let removed = chat_store
        .lock()
        .await
        .remove_attachment(&chat_id, &file_id)
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed })))
}

/// Toggle the pinned flag on a chat-attachment.
#[derive(Deserialize)]
pub struct PinAttachmentBody {
    pub pinned: bool,
}
pub async fn pin_chat_attachment(
    State(state): State<AppState>,
    Path((chat_id, file_id)): Path<(String, String)>,
    Json(body): Json<PinAttachmentBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let chat_store = {
        let daemon = state.read().await;
        daemon.chat_store.clone()
    };
    let changed = chat_store
        .lock()
        .await
        .set_attachment_pinned(&chat_id, &file_id, body.pinned)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    Ok(Json(serde_json::json!({ "ok": true, "changed": changed })))
}

/// Serve a chat-attachment file back. Now resolves via FileStore (the
/// `file_id` is a SHA-256 content hash, not a per-chat id).
pub async fn get_chat_attachment(
    State(state): State<AppState>,
    Path((chat_id, file_id)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    // Confirm the chat actually references this file (prevents using a chat
    // URL to fish arbitrary FileStore entries — caller must know both ids).
    let referenced = {
        let daemon = state.read().await;
        daemon
            .get_chat(&chat_id)
            .await
            .map(|c| c.attachments.iter().any(|a| a.file_id == file_id))
            .unwrap_or(false)
    };
    if !referenced {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("attachment {file_id} not on chat {chat_id}"),
            }),
        ));
    }
    let (entry, bytes) = {
        let daemon = state.read().await;
        let store = daemon.file_store.lock().await;
        let entry = store.get(&file_id).ok_or((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("file {file_id} missing from store"),
            }),
        ))?;
        let bytes = store.read_bytes(&file_id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("read failed: {e}"),
                }),
            )
        })?;
        (entry, bytes)
    };
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        entry
            .mime
            .parse()
            .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
    );
    Ok(resp)
}

#[derive(Deserialize)]
pub struct ExportQuery {
    /// "md" or "json". Defaults to "json" — the safe round-trip format.
    #[serde(default)]
    pub format: Option<String>,
}

/// Export a chat as either Markdown (human-readable transcript) or JSON
/// (full schema, round-trips into a re-import). Streams as the appropriate
/// content type with a `Content-Disposition: attachment` so the browser
/// triggers a download.
pub async fn export_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let chat = daemon.get_chat(&id).await.ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("chat {id} not found"),
        }),
    ))?;
    let fmt = q.format.as_deref().unwrap_or("json");
    let (body, mime, ext) = match fmt {
        "md" | "markdown" => {
            let mut out = String::new();
            out.push_str(&format!("# {}\n\n", chat.name));
            out.push_str(&format!("_agent: {}_\n", chat.agent_id));
            if let Some(sys) = &chat.system_override {
                out.push_str(&format!("\n> **System override:** {sys}\n"));
            }
            out.push('\n');
            for m in &chat.messages {
                let role = match m.role {
                    axocoatl_core::MessageRole::User => "## You",
                    axocoatl_core::MessageRole::Assistant => "## Assistant",
                    axocoatl_core::MessageRole::System => "## System",
                    axocoatl_core::MessageRole::Tool => "## Tool",
                };
                out.push_str(role);
                out.push_str("\n\n");
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
            (out, "text/markdown; charset=utf-8", "md")
        }
        _ => (
            serde_json::to_string_pretty(&chat).unwrap_or_else(|_| "{}".to_string()),
            "application/json; charset=utf-8",
            "json",
        ),
    };
    let filename = format!(
        "{}.{}",
        chat.name
            .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_"),
        ext
    );
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, mime.parse().unwrap());
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );
    Ok(resp)
}

/// List candidate models the agent's configured provider can serve. The
/// model-override picker uses this. Per locked decision, switching to a
/// different provider is NOT allowed — model name only.
///
/// - Ollama: live-query the daemon at /api/tags
/// - OpenAI/Anthropic/Gemini/Mistral: return a curated static list of
///   the chat-capable models known at build time
#[derive(Deserialize)]
pub struct ModelsQuery {
    pub agent: Option<String>,
}
pub async fn list_models(
    State(state): State<AppState>,
    Query(q): Query<ModelsQuery>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let agent = match q.agent.as_deref() {
        Some(id) => daemon.config.agents.iter().find(|a| a.id == id).cloned(),
        None => None,
    };
    let provider = agent
        .as_ref()
        .map(|a| a.provider.to_lowercase())
        .unwrap_or_default();
    let cur_model = agent.as_ref().map(|a| a.model.clone()).unwrap_or_default();

    let mut models: Vec<String> = match provider.as_str() {
        "ollama" => {
            // Live discovery from local Ollama. If the daemon's down we return
            // just the agent's current model so the picker still has one row.
            let base = daemon
                .config
                .providers
                .ollama
                .as_ref()
                .map(|o| o.base_url.clone())
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            match reqwest::get(format!("{base}/api/tags")).await {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(v) => v["models"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                    Err(_) => vec![],
                },
                Err(_) => vec![],
            }
        }
        "openai" => vec![
            "gpt-5".into(),
            "gpt-5-mini".into(),
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "o1".into(),
            "o1-mini".into(),
        ],
        "anthropic" => vec![
            "claude-opus-4-7".into(),
            "claude-sonnet-4-6".into(),
            "claude-haiku-4-5-20251001".into(),
            "claude-sonnet-3-7".into(),
            "claude-opus-3-5".into(),
        ],
        "gemini" => vec![
            "gemini-2.0-flash".into(),
            "gemini-1.5-pro".into(),
            "gemini-1.5-flash".into(),
        ],
        "mistral" => vec![
            "mistral-large-latest".into(),
            "mistral-medium-latest".into(),
            "codestral-latest".into(),
        ],
        _ => vec![],
    };
    // Ensure the agent's currently-configured model is always at the top.
    if !cur_model.is_empty() && !models.iter().any(|m| m == &cur_model) {
        models.insert(0, cur_model);
    }
    Ok(Json(models))
}

pub async fn fork_chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ForkChatBody>,
) -> Result<Json<axocoatl_memory::chat::Chat>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let replacement =
        body.replacement_content
            .map(|content| axocoatl_memory::session::StoredMessage {
                role: body
                    .replacement_role
                    .unwrap_or(axocoatl_core::MessageRole::User),
                content,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                token_count: 0,
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
    daemon
        .fork_chat(&id, body.truncate_at, replacement)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })
}

// --- Filesystem browsing (folder picker) ---

#[derive(Deserialize)]
pub struct FsListQuery {
    pub path: Option<String>,
    pub hidden: Option<bool>,
}

#[derive(Serialize)]
pub struct FsDirEntry {
    pub name: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct FsListResponse {
    pub path: String,
    pub parent: Option<String>,
    pub dirs: Vec<FsDirEntry>,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

/// List the subdirectories of a path — backs the folder picker. Read-only.
pub async fn fs_list_dirs(
    Query(q): Query<FsListQuery>,
) -> Result<Json<FsListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let raw = q
        .path
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
    let dir = std::path::Path::new(&raw)
        .canonicalize()
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{raw}: {e}")))?;
    if !dir.is_dir() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("not a directory: {}", dir.display()),
        ));
    }
    let show_hidden = q.hidden.unwrap_or(false);
    let mut dirs = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            dirs.push(FsDirEntry {
                name,
                path: p.to_string_lossy().to_string(),
            });
        }
    }
    dirs.sort_by_key(|a| a.name.to_lowercase());
    Ok(Json(FsListResponse {
        path: dir.to_string_lossy().to_string(),
        parent: dir.parent().map(|p| p.to_string_lossy().to_string()),
        dirs,
    }))
}

/// Probe a folder (pre-session-creation) to surface project-level config:
/// `.devcontainer/devcontainer.json` for runtime, `AXOCOATL.md` for agent
/// instructions. Used by the folder picker to show what's about to apply
/// before the user commits.
pub async fn fs_project_probe(
    Query(q): Query<FsListQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let raw = q.path.unwrap_or_default();
    if raw.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "path is required"));
    }
    let dir = std::path::Path::new(&raw)
        .canonicalize()
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{raw}: {e}")))?;

    // devcontainer.json — optional, well-formed only.
    let devcontainer = match axocoatl_session::DevContainer::load(&dir) {
        Ok(Some((path, dc))) => serde_json::json!({
            "path": path.display().to_string(),
            "image": dc.image,
            "post_create_scripts": dc.post_create_scripts(),
            "forwarded_ports": dc.forwarded_ports(),
            "ignored_fields": dc.ignored_fields(),
        }),
        Ok(None) => serde_json::Value::Null,
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };

    // AXOCOATL.md files along the path — just enumerate, don't read full
    // content here (kept small for the modal). Root → leaf order matches the
    // composer in the actor.
    let mut axo_files: Vec<String> = Vec::new();
    let mut ancestors: Vec<&std::path::Path> = dir.ancestors().collect();
    ancestors.reverse();
    for d in ancestors {
        let p = d.join("AXOCOATL.md");
        if p.exists() {
            axo_files.push(p.display().to_string());
        }
    }

    Ok(Json(serde_json::json!({
        "devcontainer": devcontainer,
        "axocoatl_md": axo_files,
    })))
}

// --- Session file tree + file viewer ---

#[derive(Deserialize)]
pub struct SessionPathQuery {
    pub path: Option<String>,
}

#[derive(Serialize)]
pub struct TreeEntry {
    pub name: String,
    /// Path relative to the session's working directory.
    pub path: String,
    /// "dir" or "file".
    pub kind: String,
    pub size: u64,
}

#[derive(Serialize)]
pub struct FileResponse {
    pub path: String,
    pub content: String,
    pub lang: String,
    pub truncated: bool,
}

/// Resolve `rel` against a session's working dir, rejecting any path that
/// escapes it. Returns the canonical target and the canonical root.
async fn resolve_in_session(
    state: &AppState,
    id: &str,
    rel: Option<&str>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), (StatusCode, Json<ErrorResponse>)> {
    let session = state
        .read()
        .await
        .get_session(id)
        .await
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("session '{id}' not found")))?;
    let root = session
        .working_dir
        .canonicalize()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let target = match rel.filter(|r| !r.is_empty()) {
        Some(r) => root.join(r),
        None => root.clone(),
    };
    let target = target
        .canonicalize()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if !target.starts_with(&root) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "path escapes the session directory",
        ));
    }
    Ok((target, root))
}

/// One directory level of a session's file tree (lazy-loaded).
pub async fn session_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SessionPathQuery>,
) -> Result<Json<Vec<TreeEntry>>, (StatusCode, Json<ErrorResponse>)> {
    let (target, root) = resolve_in_session(&state, &id, q.path.as_deref()).await?;
    if !target.is_dir() {
        return Err(err(StatusCode::BAD_REQUEST, "not a directory"));
    }
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&target) {
        for e in rd.flatten() {
            let p = e.path();
            let md = e.metadata().ok();
            let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            entries.push(TreeEntry {
                name: e.file_name().to_string_lossy().to_string(),
                path: p
                    .strip_prefix(&root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string(),
                kind: if is_dir { "dir" } else { "file" }.to_string(),
                size: md.map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    // Directories first, then files; each alphabetical.
    entries.sort_by(|a, b| {
        (a.kind != "dir", a.name.to_lowercase()).cmp(&(b.kind != "dir", b.name.to_lowercase()))
    });
    Ok(Json(entries))
}

/// Background tasks running in a session's sandbox container.
pub async fn session_tasks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    Json(state.read().await.session_tasks(&id).await)
}

#[derive(serde::Deserialize)]
pub struct SpawnTaskRequest {
    pub command: String,
    /// When true, the task runs in a PTY (interactive) and is reached via
    /// the `/terminals/{id}` WebSocket. False (or absent) means the legacy
    /// log-only background task.
    #[serde(default)]
    pub interactive: bool,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default = "default_cols")]
    pub cols: u16,
}

fn default_rows() -> u16 {
    30
}
fn default_cols() -> u16 {
    100
}

/// Start a user-supplied command as a background task in this session's
/// sandbox container. Boots the container on first use.
pub async fn session_task_spawn(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SpawnTaskRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let cmd = body.command.trim();
    if cmd.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "command is empty"));
    }
    if body.interactive {
        match state
            .read()
            .await
            .spawn_session_terminal(&id, cmd, body.rows, body.cols)
            .await
        {
            Ok(tid) => Ok(Json(serde_json::json!({ "id": tid, "kind": "terminal" }))),
            Err(e) => Err(err(StatusCode::BAD_REQUEST, e.to_string())),
        }
    } else {
        match state.read().await.spawn_session_task(&id, cmd).await {
            Ok(task_id) => Ok(Json(serde_json::json!({ "id": task_id, "kind": "task" }))),
            Err(e) => Err(err(StatusCode::BAD_REQUEST, e.to_string())),
        }
    }
}

/// WebSocket bridge to an interactive PTY terminal. Server sends raw vt100
/// bytes as binary frames; the client sends keystrokes (binary or text) and
/// can send `{"kind":"resize","rows":N,"cols":N}` text frames to reflow.
pub async fn session_terminal_ws(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<AppState>,
    Path((session_id, terminal_id)): Path<(String, String)>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_terminal_ws(socket, state, session_id, terminal_id))
}

async fn handle_terminal_ws(
    mut socket: axum::extract::ws::WebSocket,
    state: AppState,
    session_id: String,
    terminal_id: String,
) {
    use axum::extract::ws::Message;
    use tokio::sync::broadcast::error::RecvError;

    let term = match state
        .read()
        .await
        .session_terminal(&session_id, &terminal_id)
        .await
    {
        Some(t) => t,
        None => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({ "kind": "error", "message": "no such terminal" })
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };

    // Catch up: send scrollback so a fresh attach sees the existing buffer.
    let snapshot = term.snapshot();
    if !snapshot.is_empty() {
        let _ = socket.send(Message::Binary(snapshot.into())).await;
    }

    let mut output_rx = term.output_tx.subscribe();
    let input_tx = term.input_tx.clone();
    let term_for_resize = term.clone();

    loop {
        tokio::select! {
            // PTY → WS
            chunk = output_rx.recv() => match chunk {
                Ok(bytes) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            // WS → PTY
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(b))) => { let _ = input_tx.send(b.to_vec()); }
                Some(Ok(Message::Text(t))) => {
                    // Resize message? Try to parse; otherwise treat as input.
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if v.get("kind").and_then(|x| x.as_str()) == Some("resize") {
                            let rows = v.get("rows").and_then(|x| x.as_u64()).unwrap_or(30) as u16;
                            let cols = v.get("cols").and_then(|x| x.as_u64()).unwrap_or(100) as u16;
                            term_for_resize.resize(rows, cols);
                            continue;
                        }
                    }
                    let _ = input_tx.send(t.as_bytes().to_vec());
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // ping/pong handled by axum
                Some(Err(_)) => break,
            }
        }
    }
}

/// Read one file inside a session's working directory (capped at 512 KB).
pub async fn session_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SessionPathQuery>,
) -> Result<Json<FileResponse>, (StatusCode, Json<ErrorResponse>)> {
    use std::io::Read;
    let (target, _) = resolve_in_session(&state, &id, q.path.as_deref()).await?;
    if !target.is_file() {
        return Err(err(StatusCode::BAD_REQUEST, "not a file"));
    }
    const CAP: u64 = 512 * 1024;
    let len = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    std::fs::File::open(&target)
        .and_then(|f| f.take(CAP).read_to_end(&mut buf))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let lang = target
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    Ok(Json(FileResponse {
        path: q.path.unwrap_or_default(),
        content: String::from_utf8_lossy(&buf).to_string(),
        lang,
        truncated: len > CAP,
    }))
}

#[derive(serde::Deserialize)]
pub struct WriteFileBody {
    pub content: String,
}

/// Write a file inside a session's working directory. Existing file is
/// overwritten atomically (write to `<path>.tmp` + rename). Refuses to
/// create new directories or escape the session root.
pub async fn session_file_write(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SessionPathQuery>,
    Json(body): Json<WriteFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let (target, _root) = resolve_in_session(&state, &id, q.path.as_deref()).await?;
    if target.is_dir() {
        return Err(err(StatusCode::BAD_REQUEST, "path is a directory"));
    }
    let tmp = target.with_extension(format!(
        "{}.axotmp",
        target.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, body.content.as_bytes())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}")))?;
    std::fs::rename(&tmp, &target)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "bytes": body.content.len(),
    })))
}

// --- Proactive agents ---

#[derive(Serialize)]
pub struct ProactiveEntry {
    pub id: String,
    pub name: String,
    pub agent: String,
    /// "schedule" or "event".
    pub trigger_kind: String,
    /// The interval ("5m") or event name, depending on `trigger_kind`.
    pub trigger_detail: String,
    pub input: String,
    pub enabled: bool,
    pub last_fired_unix: Option<u64>,
    pub last_outcome: Option<String>,
    pub run_count: u64,
}

pub async fn list_proactive(State(state): State<AppState>) -> Json<Vec<ProactiveEntry>> {
    use axocoatl_config::ProactiveTrigger;
    let daemon = state.read().await;
    let table = daemon.proactive_table.clone();
    drop(daemon);
    let entries = table
        .lock()
        .map(|v| {
            v.iter()
                .map(|p| {
                    let (trigger_kind, trigger_detail) = match &p.config.trigger {
                        ProactiveTrigger::Schedule { every } => {
                            ("schedule".to_string(), every.clone())
                        }
                        ProactiveTrigger::OnEvent { event } => ("event".to_string(), event.clone()),
                    };
                    ProactiveEntry {
                        id: p.config.id.clone(),
                        name: p.config.name.clone(),
                        agent: p.config.agent.clone(),
                        trigger_kind,
                        trigger_detail,
                        input: p.config.input.clone(),
                        enabled: p.config.enabled,
                        last_fired_unix: p.last_fired_unix,
                        last_outcome: p.last_outcome.clone(),
                        run_count: p.run_count,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Json(entries)
}

// --- Skills ---

#[derive(Serialize)]
pub struct SkillEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub emits: Vec<String>,
    pub reacts_to: Vec<String>,
    pub agents: Vec<String>,
}

pub async fn list_skills(State(state): State<AppState>) -> Json<Vec<SkillEntry>> {
    let daemon = state.read().await;
    let entries = daemon
        .config
        .skills
        .iter()
        .map(|g| SkillEntry {
            id: g.id.clone(),
            name: g.name.clone(),
            description: g.description.clone(),
            emits: g.emits.clone(),
            reacts_to: g.reacts_to.clone(),
            agents: g.agents.clone(),
        })
        .collect();
    Json(entries)
}

#[derive(Serialize)]
pub struct FireSkillResponse {
    pub skill_id: String,
    pub events_published: Vec<String>,
}

pub async fn fire_skill(
    State(state): State<AppState>,
    Path(skill_id): Path<String>,
) -> Result<Json<FireSkillResponse>, (StatusCode, Json<ErrorResponse>)> {
    use axocoatl_coordination::{EventId, EventType, LatticeEvent};
    use std::time::{SystemTime, UNIX_EPOCH};
    let daemon = state.read().await;
    let g = daemon
        .config
        .skills
        .iter()
        .find(|g| g.id == skill_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Skill '{skill_id}' not found"),
                }),
            )
        })?
        .clone();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut published = Vec::new();
    for emit in &g.emits {
        let ev = LatticeEvent {
            id: EventId::random(),
            event_type: EventType::Custom(emit.clone()),
            payload: serde_json::json!({
                "fired_by_skill": skill_id,
                "agents_holding": g.agents,
            }),
            produced_by: format!("skill:{skill_id}"),
            timestamp: ts,
        };
        daemon.event_lattice.publish(ev);
        published.push(emit.clone());
    }
    Ok(Json(FireSkillResponse {
        skill_id,
        events_published: published,
    }))
}

// --- Recent lattice events (for timeline / log) ---

#[derive(Serialize)]
pub struct EventEntry {
    pub id: String,
    pub event_type: String,
    pub produced_by: String,
    pub timestamp: u64,
    pub payload: serde_json::Value,
}

pub async fn recent_events(State(state): State<AppState>) -> Json<Vec<EventEntry>> {
    let daemon = state.read().await;
    let log = daemon.event_log.clone();
    drop(daemon);
    let entries: Vec<EventEntry> = log
        .lock()
        .map(|q| {
            q.iter()
                .map(|e| EventEntry {
                    id: e.id.0.clone(),
                    event_type: format!("{:?}", e.event_type),
                    produced_by: e.produced_by.clone(),
                    timestamp: e.timestamp,
                    payload: e.payload.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    Json(entries)
}

// --- Schedule control ---

#[derive(Deserialize)]
pub struct SchedulePatch {
    pub enabled: Option<bool>,
}

#[derive(Serialize)]
pub struct ScheduleActionResponse {
    pub schedule_id: String,
    pub ok: bool,
    pub message: String,
}

pub async fn patch_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SchedulePatch>,
) -> Result<Json<ScheduleActionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let table = daemon.schedule_table.clone();
    drop(daemon);
    let mut t = table.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "schedule table poisoned".into(),
            }),
        )
    })?;
    let Some(s) = t.iter_mut().find(|s| s.config.id == id) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("schedule '{id}' not found"),
            }),
        ));
    };
    if let Some(enabled) = body.enabled {
        s.config.enabled = enabled;
    }
    Ok(Json(ScheduleActionResponse {
        schedule_id: id,
        ok: true,
        message: format!("enabled={}", s.config.enabled),
    }))
}

pub async fn run_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ScheduleActionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let (workflow_id, input) = {
        let daemon = state.read().await;
        let s = daemon.config.schedules.iter().find(|s| s.id == id).cloned();
        match s {
            Some(s) => (s.workflow.clone(), s.input.clone()),
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        error: format!("schedule '{id}' not found"),
                    }),
                ))
            }
        }
    };
    let daemon = state.read().await;
    match daemon.execute_workflow(&workflow_id, &input).await {
        Ok(out) => Ok(Json(ScheduleActionResponse {
            schedule_id: id,
            ok: true,
            message: format!(
                "ran workflow '{}' · {} agents · {} tokens",
                workflow_id,
                out.completed_agents.len(),
                out.total_token_usage.input_tokens + out.total_token_usage.output_tokens
            ),
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

// --- Agent restart ---

#[derive(Serialize)]
pub struct RestartResponse {
    pub agent_id: String,
    pub restarted: bool,
}

pub async fn restart_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<RestartResponse>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    match daemon.restart_agent(&agent_id).await {
        Ok(()) => Ok(Json(RestartResponse {
            agent_id,
            restarted: true,
        })),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )),
    }
}

// --- Unified live WebSocket ---
//
// One bidirectional socket per dashboard — the only live transport. The
// server pushes every stream-bus frame (lattice events + live agent tokens);
// the client sends commands (run-workflow, chat, ping).

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum WsCommand {
    RunWorkflow {
        id: String,
        input: String,
    },
    /// One turn in a persisted chat — looks up the Chat by id, builds the
    /// history from its messages, honors `system_override`, streams tokens
    /// over `chat-*` frames, and persists the assistant reply on done.
    ChatTurn {
        chat_id: String,
        content: String,
    },
    /// Stop the visible token stream for an in-flight ChatTurn. See
    /// `active_chat_turns` for v1 limitations.
    ChatStop {
        chat_id: String,
    },
    Session {
        id: String,
        input: String,
        /// Per-turn model override. When `Some`, the next turn dispatches
        /// to this model (e.g. `"llama3.2:1b"`) instead of the agent's
        /// configured default. Same agent, same memory, different model.
        #[serde(default)]
        model_override: Option<String>,
        /// Per-turn target agent. When the session is multi-agent and this
        /// is `Some`, only that agent runs (instead of the full lattice).
        #[serde(default)]
        target_agent: Option<String>,
    },
    /// Resolve a pending MCP-tool approval prompt. `decision` is "allow" or
    /// "deny"; `persist` is "once" / "agent_tool" / "agent_server" /
    /// "any_agent_server" (mirrors the scope buttons in the modal).
    McpApprove {
        approval_id: String,
        decision: String,
        #[serde(default = "default_once")]
        persist: String,
    },
    Ping,
}
fn default_once() -> String {
    "once".to_string()
}

pub async fn ws(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<AppState>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: axum::extract::ws::WebSocket, state: AppState) {
    use axum::extract::ws::Message;
    use tokio::sync::broadcast::error::RecvError;

    // Subscribe to the daemon's stream bus (events + live tokens).
    let mut bus_rx = { state.read().await.stream_bus.subscribe() };
    // Frames generated by this connection's own commands (chat, run results).
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let _ = socket
        .send(Message::Text(
            serde_json::json!({ "kind": "ready" }).to_string().into(),
        ))
        .await;

    // Snapshot of in-flight runs — lets a client that reloaded mid-run
    // re-attach its live view instead of losing it.
    let snapshot = {
        let daemon = state.read().await;
        let runs: Vec<_> = daemon
            .active_runs
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        axocoatl_daemon::StreamFrame::Snapshot { runs }
    };
    if let Ok(j) = serde_json::to_string(&snapshot) {
        let _ = socket.send(Message::Text(j.into())).await;
    }

    loop {
        tokio::select! {
            // ── inbound command ──
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        dispatch_ws_command(&text, &state, &out_tx).await;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            // ── stream bus → client ──
            frame = bus_rx.recv() => {
                match frame {
                    Ok(f) => {
                        if let Ok(j) = serde_json::to_string(&f) {
                            if socket.send(Message::Text(j.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    // Under heavy token load a slow client may lag — skip, don't drop the socket.
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
            // ── this connection's own frames → client ──
            local = out_rx.recv() => {
                if let Some(j) = local {
                    if socket.send(Message::Text(j.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

async fn dispatch_ws_command(
    text: &str,
    state: &AppState,
    out_tx: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    let cmd: WsCommand = match serde_json::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            let _ = out_tx.send(
                serde_json::json!({ "kind": "error", "message": format!("bad command: {e}") })
                    .to_string(),
            );
            return;
        }
    };

    match cmd {
        WsCommand::Ping => {
            let _ = out_tx.send(serde_json::json!({ "kind": "pong" }).to_string());
        }

        WsCommand::McpApprove {
            approval_id,
            decision,
            persist,
        } => {
            use axocoatl_mcp::approval::{ApprovalResolution, PersistScope};
            use axocoatl_mcp::permissions::PermissionDecision;
            let dec = match decision.as_str() {
                "allow" => PermissionDecision::Allow,
                _ => PermissionDecision::Deny,
            };
            let scope = match persist.as_str() {
                "agent_tool" => PersistScope::ThisAgentThisTool,
                "agent_server" => PersistScope::ThisAgentThisServer,
                "any_agent_server" => PersistScope::AnyAgentThisServer,
                _ => PersistScope::Once,
            };
            let gate = {
                let daemon = state.read().await;
                daemon.mcp_approval_gate.clone()
            };
            let resolved = gate
                .resolve(
                    &approval_id,
                    ApprovalResolution {
                        decision: dec,
                        persist_scope: scope,
                    },
                )
                .await;
            if !resolved {
                let _ = out_tx.send(
                    serde_json::json!({ "kind": "mcp-approval-unknown", "approval_id": approval_id }).to_string()
                );
            }
        }

        // Run a workflow — live per-agent tokens arrive over the stream bus.
        // The result is broadcast on the bus too (not sent to this one
        // connection) so a client that reconnected mid-run still sees it.
        WsCommand::RunWorkflow { id, input } => {
            let state = state.clone();
            tokio::spawn(async move {
                let (result, bus) = {
                    let daemon = state.read().await;
                    let bus = daemon.stream_bus.clone();
                    let result = daemon.execute_workflow(&id, &input).await;
                    (result, bus)
                };
                // Clear the run from the registry directly — don't depend on
                // the tracker catching the WorkflowDone frame under token lag.
                {
                    let runs = state.read().await.active_runs.clone();
                    let mut guard = runs.lock();
                    if let Ok(m) = guard.as_mut() {
                        m.remove(&id);
                    }
                }
                let frame = match result {
                    Ok(o) => axocoatl_daemon::StreamFrame::WorkflowDone {
                        workflow: o.workflow_id,
                        output: o.final_content,
                        completed: o.completed_agents,
                        tokens: o.total_token_usage.total() as u64,
                    },
                    Err(e) => axocoatl_daemon::StreamFrame::WorkflowError {
                        workflow: id,
                        error: e.to_string(),
                    },
                };
                let _ = bus.send(frame);
            });
        }

        // Chat — stream the agent's tokens straight back to this client.
        // One chat turn — runs the chat's configured agent with the chat's
        // history + system_override. Streams tokens; cancellable via ChatStop.
        WsCommand::ChatTurn { chat_id, content } => {
            let state = state.clone();
            let out = out_tx.clone();
            tokio::spawn(async move {
                // Load the chat.
                let chat = {
                    let daemon = state.read().await;
                    daemon.get_chat(&chat_id).await
                };
                let chat = match chat {
                    Some(c) => c,
                    None => {
                        let _ = out.send(
                            serde_json::json!({
                                "kind": "chat-error",
                                "chat_id": chat_id,
                                "error": format!("chat {chat_id} not found"),
                            })
                            .to_string(),
                        );
                        return;
                    }
                };

                // Resolve the agent actor.
                let actor = {
                    let daemon = state.read().await;
                    daemon
                        .agent_registry
                        .get(&axocoatl_core::AgentId::new(&chat.agent_id))
                        .await
                };
                let actor = match actor {
                    Some(a) => a,
                    None => {
                        let _ = out.send(
                            serde_json::json!({
                                "kind": "chat-error",
                                "chat_id": chat_id,
                                "error": format!("agent '{}' not found", chat.agent_id),
                            })
                            .to_string(),
                        );
                        return;
                    }
                };

                // Resolve the chat's attachment refs (both pinned and
                // pending) against the FileStore, then drain non-pinned.
                // The user message text gets a suffix listing attached file
                // names so the transcript reads sensibly without re-loading.
                let attachments_for_turn: Vec<axocoatl_memory::files::FileEntry> = {
                    let daemon = state.read().await;
                    let chat_refs = daemon
                        .chat_store
                        .lock()
                        .await
                        .consume_attachments_for_turn(&chat_id)
                        .unwrap_or_default();
                    let fs = daemon.file_store.lock().await;
                    chat_refs
                        .iter()
                        .filter_map(|a| fs.get(&a.file_id))
                        .collect()
                };
                {
                    let daemon = state.read().await;
                    let store = daemon.chat_store.clone();
                    let mut text_for_history = content.clone();
                    if !attachments_for_turn.is_empty() {
                        let names = attachments_for_turn
                            .iter()
                            .map(|e| format!("📎 {}", e.name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        text_for_history.push_str(&format!("\n\n_(attached: {names})_"));
                    }
                    let _ = store.lock().await.append_message(
                        &chat_id,
                        axocoatl_memory::session::StoredMessage {
                            role: axocoatl_core::MessageRole::User,
                            content: text_for_history,
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0),
                            token_count: 0,
                            name: None,
                            tool_calls: Vec::new(),
                            tool_call_id: None,
                        },
                    );
                }

                // Announce the turn. Carries chat_id so the UI can route the
                // stream to the right chat pane (multiple chats can be open).
                let _ = out.send(
                    serde_json::json!({
                        "kind": "chat-start",
                        "chat_id": chat_id,
                        "agent": chat.agent_id,
                    })
                    .to_string(),
                );

                // Register a cancellation slot. If a prior turn for this chat
                // is still in-flight, pre-empt it (UX: replying again cancels
                // the previous reply).
                let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
                {
                    let daemon = state.read().await;
                    let mut active = daemon.active_chat_turns.lock().await;
                    if let Some(prev) = active.remove(&chat_id) {
                        let _ = prev.send(());
                    }
                    active.insert(chat_id.clone(), cancel_tx);
                }

                // Token sink → chat-* frames. Also accumulates the text so we
                // can persist the partial assistant message on cancel.
                let accumulated = Arc::new(tokio::sync::Mutex::new(String::new()));
                let (sink_tx, mut sink_rx) =
                    tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
                {
                    let out = out.clone();
                    let chat_id = chat_id.clone();
                    let accumulated = accumulated.clone();
                    tokio::spawn(async move {
                        while let Some(chunk) = sink_rx.recv().await {
                            let f = match chunk {
                                axocoatl_actor::AgentStreamChunk::Text(d) => {
                                    accumulated.lock().await.push_str(&d);
                                    serde_json::json!({
                                        "kind": "chat-token", "chat_id": chat_id, "delta": d,
                                    })
                                }
                                axocoatl_actor::AgentStreamChunk::Reasoning(d) => {
                                    serde_json::json!({
                                        "kind": "chat-reasoning", "chat_id": chat_id, "delta": d,
                                    })
                                }
                                axocoatl_actor::AgentStreamChunk::ToolCallStarted {
                                    id,
                                    name,
                                    arguments,
                                } => serde_json::json!({
                                    "kind": "chat-tool-start", "chat_id": chat_id,
                                    "call_id": id, "name": name, "arguments": arguments,
                                }),
                                axocoatl_actor::AgentStreamChunk::ToolCallResult {
                                    id,
                                    name,
                                    result,
                                    is_error,
                                } => serde_json::json!({
                                    "kind": "chat-tool-result", "chat_id": chat_id,
                                    "call_id": id, "name": name,
                                    "result": result, "is_error": is_error,
                                }),
                            };
                            let _ = out.send(f.to_string());
                        }
                    });
                }

                // Build the AgentInput from the chat's history. The user's
                // content was already appended above, so we pass the rest as
                // history and the new content as the live turn.
                let history: Vec<axocoatl_core::ChatMessage> = chat
                    .messages
                    .iter()
                    .map(|m| axocoatl_core::ChatMessage {
                        role: m.role.clone(),
                        content: axocoatl_core::MessageContent::Text(m.content.clone()),
                        name: None,
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    })
                    .collect();
                // Resolve FileStore entries to AgentAttachments. `path` points
                // at the content-addressed file on disk; the executor reads the
                // bytes once and inlines them (image → base64 vision, text →
                // <attachment> block). Extracted text (PDF/CSV/OCR) is carried
                // alongside on AgentAttachment so the executor inlines it too.
                let core_attachments: Vec<axocoatl_core::AgentAttachment> = {
                    let daemon = state.read().await;
                    let fs = daemon.file_store.lock().await;
                    attachments_for_turn
                        .iter()
                        .filter_map(|e| {
                            let path = fs.path_of(&e.id)?.to_string_lossy().to_string();
                            Some(axocoatl_core::AgentAttachment {
                                id: e.id.clone(),
                                name: e.name.clone(),
                                mime: e.mime.clone(),
                                path,
                                size: e.size,
                                extracted_text: e
                                    .extracted_text
                                    .clone()
                                    .or_else(|| e.ocr_text.clone()),
                            })
                        })
                        .collect()
                };
                let agent_input = axocoatl_core::AgentInput::text(&content)
                    .with_history(history)
                    .with_system_override(chat.system_override.clone())
                    .with_model_override(chat.model_override.clone())
                    .with_attachments(core_attachments);

                // Race the agent execution against the cancel signal.
                let exec_fut =
                    axocoatl_actor::execute_agent_streaming(&actor, agent_input, sink_tx);
                tokio::pin!(exec_fut);
                let outcome: Option<Result<axocoatl_core::AgentOutput, String>> = tokio::select! {
                    out_res = &mut exec_fut => Some(out_res),
                    _ = &mut cancel_rx => None,
                };

                // Drop our registration if the slot still belongs to us.
                {
                    let daemon = state.read().await;
                    daemon.active_chat_turns.lock().await.remove(&chat_id);
                }

                match outcome {
                    Some(Ok(o)) => {
                        // Persist the assistant reply.
                        let daemon = state.read().await;
                        let store = daemon.chat_store.clone();
                        let final_text = if !o.content.is_empty() {
                            o.content.clone()
                        } else {
                            accumulated.lock().await.clone()
                        };
                        let _ = store.lock().await.append_message(
                            &chat_id,
                            axocoatl_memory::session::StoredMessage {
                                role: axocoatl_core::MessageRole::Assistant,
                                content: final_text,
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0),
                                token_count: o.token_usage.output_tokens,
                                name: None,
                                tool_calls: Vec::new(),
                                tool_call_id: None,
                            },
                        );
                        let _ = out.send(
                            serde_json::json!({
                                "kind": "chat-done",
                                "chat_id": chat_id,
                                "input_tokens": o.token_usage.input_tokens,
                                "output_tokens": o.token_usage.output_tokens,
                            })
                            .to_string(),
                        );
                    }
                    Some(Err(e)) => {
                        let _ = out.send(
                            serde_json::json!({
                                "kind": "chat-error",
                                "chat_id": chat_id,
                                "error": e,
                            })
                            .to_string(),
                        );
                    }
                    None => {
                        // Cancelled — save whatever we received before the stop.
                        let partial = accumulated.lock().await.clone();
                        if !partial.is_empty() {
                            let daemon = state.read().await;
                            let store = daemon.chat_store.clone();
                            let _ = store.lock().await.append_message(
                                &chat_id,
                                axocoatl_memory::session::StoredMessage {
                                    role: axocoatl_core::MessageRole::Assistant,
                                    content: partial,
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs())
                                        .unwrap_or(0),
                                    token_count: 0,
                                    name: None,
                                    tool_calls: Vec::new(),
                                    tool_call_id: None,
                                },
                            );
                        }
                        let _ = out.send(
                            serde_json::json!({
                                "kind": "chat-stopped",
                                "chat_id": chat_id,
                            })
                            .to_string(),
                        );
                    }
                }
            });
        }

        WsCommand::ChatStop { chat_id } => {
            let active = {
                let daemon = state.read().await;
                daemon.active_chat_turns.clone()
            };
            let popped = {
                let mut guard = active.lock().await;
                guard.remove(&chat_id)
            };
            if let Some(tx) = popped {
                let _ = tx.send(());
            }
        }

        // Session — stream the agent's work (tokens, reasoning, tool calls)
        // onto the bus, so the cockpit + lattice panel see it and the run is
        // reconnectable.
        WsCommand::Session {
            id,
            input,
            model_override,
            target_agent,
        } => {
            let state = state.clone();
            tokio::spawn(async move {
                let bus = { state.read().await.stream_bus.clone() };
                let _ = bus.send(axocoatl_daemon::StreamFrame::SessionStart {
                    session: id.clone(),
                });

                // Agent stream chunks → bus frames keyed by the session id.
                let (sink_tx, mut sink_rx) =
                    tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
                let fwd = {
                    let bus = bus.clone();
                    let sid = id.clone();
                    tokio::spawn(async move {
                        while let Some(chunk) = sink_rx.recv().await {
                            let frame = match chunk {
                                axocoatl_actor::AgentStreamChunk::Text(d) => {
                                    axocoatl_daemon::StreamFrame::Token {
                                        workflow: sid.clone(),
                                        agent: sid.clone(),
                                        delta: d,
                                    }
                                }
                                axocoatl_actor::AgentStreamChunk::Reasoning(d) => {
                                    axocoatl_daemon::StreamFrame::Reasoning {
                                        workflow: sid.clone(),
                                        agent: sid.clone(),
                                        delta: d,
                                    }
                                }
                                axocoatl_actor::AgentStreamChunk::ToolCallStarted {
                                    id: cid,
                                    name,
                                    arguments,
                                } => axocoatl_daemon::StreamFrame::ToolCall {
                                    workflow: sid.clone(),
                                    agent: sid.clone(),
                                    call_id: cid,
                                    name,
                                    phase: "start".to_string(),
                                    arguments: Some(arguments),
                                    result: None,
                                    is_error: false,
                                },
                                axocoatl_actor::AgentStreamChunk::ToolCallResult {
                                    id: cid,
                                    name,
                                    result,
                                    is_error,
                                } => axocoatl_daemon::StreamFrame::ToolCall {
                                    workflow: sid.clone(),
                                    agent: sid.clone(),
                                    call_id: cid,
                                    name,
                                    phase: "result".to_string(),
                                    arguments: None,
                                    result: Some(result),
                                    is_error,
                                },
                            };
                            let _ = bus.send(frame);
                        }
                    })
                };

                let result = {
                    let daemon = state.read().await;
                    daemon
                        .execute_session_streaming(
                            &id,
                            &input,
                            model_override,
                            target_agent,
                            sink_tx,
                        )
                        .await
                };
                // Drain the forwarder so every token frame is on the bus
                // before the terminal frame.
                let _ = fwd.await;
                let frame = match result {
                    Ok(o) => axocoatl_daemon::StreamFrame::SessionDone {
                        session: id,
                        input_tokens: o.token_usage.input_tokens as u64,
                        output_tokens: o.token_usage.output_tokens as u64,
                    },
                    Err(e) => axocoatl_daemon::StreamFrame::SessionError {
                        session: id,
                        error: e.to_string(),
                    },
                };
                let _ = bus.send(frame);
            });
        }
    }
}

// --- Run history (time travel) ---

pub async fn list_runs(
    State(state): State<AppState>,
    Path(automation_id): Path<String>,
) -> Result<Json<Vec<axocoatl_daemon::automation_runs::Run>>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .list_runs(&automation_id)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn get_run(
    State(state): State<AppState>,
    Path((automation_id, run_id)): Path<(String, String)>,
) -> Result<Json<axocoatl_daemon::automation_runs::Run>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .get_run(&automation_id, &run_id)
        .map(Json)
        .map_err(|e| err(StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(serde::Deserialize, Default)]
pub struct ForkRunBody {
    /// Optional override input. If absent, the source run's input is reused.
    #[serde(default)]
    pub input: Option<String>,
}

/// Fork: spawn a fresh run that starts from scratch with the same trigger
/// input as a prior run (or a user-supplied override). v0.1 doesn't yet
/// resume mid-graph from a checkpoint; this gives you "re-run with the
/// same prompt" which closes 80% of the time-travel use case (reproduce
/// a result, then iterate).
pub async fn fork_run(
    State(state): State<AppState>,
    Path((automation_id, run_id)): Path<(String, String)>,
    Json(body): Json<ForkRunBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let source = daemon
        .get_run(&automation_id, &run_id)
        .map_err(|e| err(StatusCode::NOT_FOUND, e.to_string()))?;
    let input = body.input.unwrap_or(source.trigger_input);
    // Spawn in background — the new run gets its own run_id.
    let auto_id = automation_id.clone();
    let state2 = state.clone();
    tokio::spawn(async move {
        let d = state2.read().await;
        if let Err(e) = d.execute_automation(&auto_id, &input).await {
            tracing::warn!("forked run failed: {e}");
        }
    });
    Ok(Json(
        serde_json::json!({ "ok": true, "forked_from": run_id }),
    ))
}

// --- HITL interrupts ---

/// List every pending interrupt across all in-flight automations.
pub async fn list_interrupts(State(state): State<AppState>) -> Json<Vec<serde_json::Value>> {
    let daemon = state.read().await;
    let map = daemon.pending_interrupts.read().await;
    let mut items: Vec<serde_json::Value> = map
        .values()
        .map(|p| {
            serde_json::json!({
                "automation_id": p.automation_id,
                "run_id": p.run_id,
                "node_id": p.node_id,
                "message": p.message,
                "created_at_unix": p.created_at_unix,
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b.get("created_at_unix")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .cmp(
                &a.get("created_at_unix")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            )
    });
    Json(items)
}

#[derive(serde::Deserialize, Default)]
pub struct ResumeBody {
    /// Value supplied by the operator. Per the node's `resume_strategy`
    /// this either replaces the node's output (default) or appends to
    /// the parked message.
    #[serde(default)]
    pub value: String,
}

/// Resume a parked interrupt by `{automation_id}:{run_id}:{node_id}`.
/// The executor wakes and the automation continues from there.
pub async fn resume_interrupt(
    State(state): State<AppState>,
    Path((automation_id, run_id, node_id)): Path<(String, String, String)>,
    Json(body): Json<ResumeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{automation_id}:{run_id}:{node_id}");
    let daemon = state.read().await;
    let map = daemon.pending_interrupts.read().await;
    let Some(pi) = map.get(&key).cloned() else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no pending interrupt at {key}"),
        ));
    };
    drop(map);
    *pi.resume_value.lock().await = Some(body.value);
    pi.notify.notify_one();
    Ok(Json(serde_json::json!({ "ok": true, "key": key })))
}

/// Cancel a parked interrupt. The executor wakes with an empty value
/// (regardless of resume_strategy) and the run continues — same wake
/// path as resume, just no operator input.
pub async fn cancel_interrupt(
    State(state): State<AppState>,
    Path((automation_id, run_id, node_id)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{automation_id}:{run_id}:{node_id}");
    let daemon = state.read().await;
    let map = daemon.pending_interrupts.read().await;
    let Some(pi) = map.get(&key).cloned() else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no pending interrupt at {key}"),
        ));
    };
    drop(map);
    pi.cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    *pi.resume_value.lock().await = Some(String::new());
    pi.notify.notify_one();
    Ok(Json(
        serde_json::json!({ "ok": true, "cancelled": true, "key": key }),
    ))
}

/// List every tool the automation/agent stack can call. Used by the
/// Automations editor's add-node popover to populate the Tools tab.
pub async fn list_tools(State(state): State<AppState>) -> Json<Vec<serde_json::Value>> {
    let daemon = state.read().await;
    let names = daemon.tool_executor.tool_names();
    let items: Vec<serde_json::Value> = names
        .into_iter()
        .map(|n| serde_json::json!({ "name": n, "id": n }))
        .collect();
    Json(items)
}

// --- Unified Automations API ---
//
// One concept = one endpoint. The data still lives in three legacy YAML
// sections under the hood; the daemon projects them into `Vec<Automation>`
// on demand. As phase 5 lands, this will become the authoritative store.

pub async fn list_automations(
    State(state): State<AppState>,
) -> Json<Vec<axocoatl_config::Automation>> {
    Json(state.read().await.list_automations().await)
}

pub async fn get_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_config::Automation>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let res = daemon.get_automation(&id).await;
    res.map(Json).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("automation '{id}' not found"),
        )
    })
}

/// Create a new automation. Body is the full Automation JSON.
pub async fn create_automation(
    State(state): State<AppState>,
    Json(body): Json<axocoatl_config::Automation>,
) -> Result<Json<axocoatl_config::Automation>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .create_automation(body)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))
}

/// Replace an existing automation (or insert if missing). Body is the full
/// Automation JSON; the path id must match `body.id`.
pub async fn update_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<axocoatl_config::Automation>,
) -> Result<Json<axocoatl_config::Automation>, (StatusCode, Json<ErrorResponse>)> {
    if body.id != id {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("path id '{id}' does not match body id '{}'", body.id),
        ));
    }
    let daemon = state.read().await;
    daemon
        .upsert_automation(body)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn delete_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .delete_automation(&id)
        .await
        .map(|_| Json(serde_json::json!({ "ok": true })))
        .map_err(|e| err(StatusCode::NOT_FOUND, e.to_string()))
}

// ─── Automation folders ───────────────────────────────────────────
// Organizational tree for the Automations tab. Paths look like
// "client/spec-reviews"; empty string is the root. Folders persist
// independently of automations so an empty hierarchy survives across
// daemon restarts.

#[derive(serde::Deserialize)]
pub struct CreateFolderBody {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
}
#[derive(serde::Deserialize)]
pub struct RenameFolderBody {
    pub old_path: String,
    pub new_path: String,
    #[serde(default)]
    pub new_name: Option<String>,
}
#[derive(serde::Deserialize)]
pub struct DeleteFolderQuery {
    pub path: String,
    /// `true` = move contents up to parent; `false` = recursively delete.
    /// Defaults to true (safer).
    #[serde(default = "default_keep_contents")]
    pub keep_contents: bool,
}
fn default_keep_contents() -> bool {
    true
}

pub async fn list_automation_folders(
    State(state): State<AppState>,
) -> Json<Vec<axocoatl_config::AutomationFolder>> {
    let daemon = state.read().await;
    Json(daemon.list_automation_folders().await)
}

pub async fn create_automation_folder(
    State(state): State<AppState>,
    Json(body): Json<CreateFolderBody>,
) -> Result<Json<axocoatl_config::AutomationFolder>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .create_automation_folder(&body.path, body.name)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))
}

pub async fn rename_automation_folder(
    State(state): State<AppState>,
    Json(body): Json<RenameFolderBody>,
) -> Result<Json<axocoatl_config::AutomationFolder>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .rename_automation_folder(&body.old_path, &body.new_path, body.new_name)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))
}

pub async fn delete_automation_folder(
    State(state): State<AppState>,
    Query(q): Query<DeleteFolderQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .delete_automation_folder(&q.path, q.keep_contents)
        .await
        .map(|n| Json(serde_json::json!({ "ok": true, "affected_automations": n })))
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))
}

#[derive(serde::Deserialize)]
pub struct MoveAutomationBody {
    /// Target folder path, or `null` to put the automation back at the root.
    #[serde(default)]
    pub folder: Option<String>,
}

pub async fn move_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MoveAutomationBody>,
) -> Result<Json<axocoatl_config::Automation>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .set_automation_folder(&id, body.folder)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(serde::Deserialize, Default)]
pub struct RunAutomationBody {
    /// Legacy single-string input that fed every `FromTrigger` reference.
    /// New automations should prefer `inputs` keyed by TextInput node ids.
    #[serde(default)]
    pub input: String,
    /// Per-`TextInput`-node values. Keys are node ids.
    #[serde(default)]
    pub inputs: std::collections::HashMap<String, String>,
}

/// Fire an automation now. Spawns the run in the background and returns
/// immediately — the WS bus carries the live events.
pub async fn run_automation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RunAutomationBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.clone();
    let input = body.input.clone();
    let inputs = body.inputs.clone();
    let id_clone = id.clone();
    tokio::spawn(async move {
        let d = daemon.read().await;
        if let Err(e) = d
            .execute_automation_with_inputs(&id_clone, &input, &inputs)
            .await
        {
            tracing::warn!(automation = %id_clone, error = %e, "automation run failed");
        }
    });
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

// --- Browser-pane proxy (DOM-picker enabler) ---

/// Proxy a request to a port inside the session's sandbox so the iframe
/// loads same-origin to the dashboard. That lets the injected `/axo-tap.js`
/// postMessage the parent dashboard without cross-origin restrictions.
///
/// Route: `/api/sessions/{id}/proxy/{port}/{*tail}`
/// Upstream: `http://localhost:{port}/{tail}`
pub async fn session_browser_proxy(
    State(_state): State<AppState>,
    Path((_session_id, port, tail)): Path<(String, u16, String)>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    let qs = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let upstream = format!("http://localhost:{port}/{tail}{qs}");

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("client: {e}")).into_response()
        }
    };

    let resp = match client.get(&upstream).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!(
                    "couldn't reach {upstream}: {e}. Is a dev server running \
                     on port {port} inside the session sandbox?"
                ),
            )
                .into_response();
        }
    };

    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_html = ctype.contains("text/html");

    if is_html {
        // Inject the tap script + a <base> so relative URLs resolve through
        // the proxy. Read the full body; for a dev server this is bounded.
        let body = resp.bytes().await.unwrap_or_default();
        let mut s = String::from_utf8_lossy(&body).to_string();
        let base = format!(r#"<base href="/api/sessions/{_session_id}/proxy/{port}/">"#);
        let tap = r#"<script src="/axo-tap.js"></script>"#;
        // <head> injection (base must come early so relative URLs resolve).
        if let Some(i) = s.to_lowercase().find("<head>") {
            let cut = i + "<head>".len();
            s.insert_str(cut, &base);
        } else if let Some(i) = s.to_lowercase().find("<html>") {
            let cut = i + "<html>".len();
            s.insert_str(cut, &format!("<head>{base}</head>"));
        } else {
            s.insert_str(0, &base);
        }
        // <body> injection — script at the end so document is parsed.
        if let Some(i) = s.to_lowercase().rfind("</body>") {
            s.insert_str(i, tap);
        } else {
            s.push_str(tap);
        }
        let mut builder = axum::response::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8");
        // Don't forward content-length — we modified the body.
        let h = builder.headers_mut().unwrap();
        for (k, v) in resp_headers_pass(&[]) {
            h.insert(k, v);
        }
        builder
            .body(axum::body::Body::from(s))
            .unwrap()
            .into_response()
    } else {
        // Non-HTML: stream through.
        let mut builder = axum::response::Response::builder().status(status);
        if !ctype.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, ctype);
        }
        let body = resp.bytes().await.unwrap_or_default();
        builder
            .body(axum::body::Body::from(body))
            .unwrap()
            .into_response()
    }
}

fn resp_headers_pass(_unused: &[&str]) -> Vec<(axum::http::HeaderName, axum::http::HeaderValue)> {
    Vec::new()
}

/// Same handler as above but for the proxy root (no `tail`), e.g. when
/// the user types `localhost:8765` and the iframe hits the bare port.
pub async fn session_browser_proxy_root(
    State(state): State<AppState>,
    Path((session_id, port)): Path<(String, u16)>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    session_browser_proxy(State(state), Path((session_id, port, String::new())), req).await
}

// --- A2A protocol (agent-to-agent interop) ---------------------------------
//
// Exposes this Axocoatl instance over the A2A protocol so other agent systems
// can discover its agents and dispatch tasks to them. Both routes sit behind
// the server's auth layer (see build_router).

/// A2A discovery card (`GET /.well-known/agent.json`): describes this instance
/// and lists its agents as capabilities. Address one via a task's `receiver_id`.
pub async fn a2a_agent_card(State(state): State<AppState>) -> Json<axocoatl_a2a::AgentCard> {
    let daemon = state.read().await;
    let capabilities: Vec<String> = daemon.config.agents.iter().map(|a| a.id.clone()).collect();
    Json(axocoatl_a2a::AgentCard {
        id: "axocoatl".to_string(),
        name: "Axocoatl".to_string(),
        description: "Axocoatl agent runtime — address an agent by id via a task's receiver_id."
            .to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        endpoint: "/a2a/tasks".to_string(),
        capabilities,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "input": { "type": "string" } },
            "required": ["input"]
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": { "content": { "type": "string" } }
        }),
        authentication: axocoatl_a2a::AuthSpec {
            scheme: "bearer".to_string(),
            endpoint: None,
        },
    })
}

/// A2A task intake (`POST /a2a/tasks`): dispatch an inbound task to the named
/// agent (`receiver_id`) and return its result.
pub async fn a2a_receive_task(
    State(state): State<AppState>,
    Json(task): Json<axocoatl_a2a::A2ATask>,
) -> Json<axocoatl_a2a::A2ATaskResult> {
    // Accept either {"input": "..."} or a bare value; fall back to the raw JSON.
    let input = task
        .input
        .get("input")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| task.input.to_string());

    let daemon = state.read().await;
    match daemon.execute_agent(&task.receiver_id, &input).await {
        Ok(output) => Json(axocoatl_a2a::A2ATaskResult {
            task_id: task.id,
            status: axocoatl_a2a::TaskStatus::Completed,
            output: Some(serde_json::json!({ "content": output.content })),
            error: None,
        }),
        Err(e) => Json(axocoatl_a2a::A2ATaskResult {
            task_id: task.id,
            status: axocoatl_a2a::TaskStatus::Failed,
            output: None,
            error: Some(e.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_request_parses_per_request_overrides() {
        let body: ExecuteRequest = serde_json::from_str(
            r#"{"input":"hi","system_override":"be terse","model_override":"qwen3:32b"}"#,
        )
        .unwrap();
        assert_eq!(body.input, "hi");
        assert_eq!(body.system_override.as_deref(), Some("be terse"));
        assert_eq!(body.model_override.as_deref(), Some("qwen3:32b"));
    }

    #[test]
    fn execute_request_overrides_default_to_none() {
        // Callers that send only `input` keep working — overrides are optional.
        let body: ExecuteRequest = serde_json::from_str(r#"{"input":"hi"}"#).unwrap();
        assert!(body.system_override.is_none());
        assert!(body.model_override.is_none());
    }
}
