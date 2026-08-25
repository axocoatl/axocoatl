use axum::{
    extract::{
        ws::{Message as AxumWsMessage, WebSocket as AxumWebSocket},
        ConnectInfo, FromRequestParts, OriginalUri, Path, Query, State, WebSocketUpgrade,
    },
    http::{header, uri::Authority, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as UpstreamWsMessage};

use crate::AppState;

// --- Dashboard (embedded SPA) ---

const DASHBOARD_HTML: &str = include_str!("../static/index.html");

/// The app. One route, because there is one product: a session, its chat, and
/// modules that open around it.
pub async fn dashboard(headers: HeaderMap, OriginalUri(uri): OriginalUri) -> Response {
    if let Some(location) = canonical_workbench_location(&headers, &uri) {
        let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
        response.headers_mut().insert(header::LOCATION, location);
        return response;
    }
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
        .into_response()
}

/// Keep the browser workbench and its `<session>-p<port>.localhost` Preview
/// frames on the same browser site so ordinary app cookies keep working. Only
/// the exact dashboard route calls this helper: API, health, asset, CLI, and
/// non-loopback operator hosts never redirect.
fn canonical_workbench_location(headers: &HeaderMap, uri: &Uri) -> Option<HeaderValue> {
    let authority = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Authority::from_str(value).ok())?;
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let ip = host.parse::<std::net::IpAddr>().ok()?;
    if !ip.is_loopback() {
        return None;
    }
    let listener = authority
        .port_u16()
        .map(|port| format!("localhost:{port}"))
        .unwrap_or_else(|| "localhost".to_string());
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    HeaderValue::from_str(&format!("http://{listener}{path_and_query}")).ok()
}

// --- @axocoatl/lattice — embedded graph-canvas ES modules ---
//
// The session Agent graph and Automation graph use @axocoatl/lattice. A
// package-local, byte-identical mirror of the canonical workspace source is
// embedded at compile time and served at `/lattice/{file}.js` so the browser
// can import it as a normal ES module graph (no build step). Keeping the mirror
// inside this crate is required because crates.io packages cannot read sibling
// workspace directories; build.rs rejects drift from the canonical source.

macro_rules! lattice_modules {
    ($($name:literal),* $(,)?) => {
        fn lattice_module(file: &str) -> Option<&'static str> {
            match file {
                $(
                    concat!($name, ".js") => Some(include_str!(
                        concat!("../static/lattice/", $name, ".js")
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

// --- Embedded brand kit ---

/// Serve a package-local mirror of the canonical brand kit. Single-mark
/// system: mark.png + favicon.png + the wordmark family + colors.json.
/// Embedded at compile time so both the daemon and the published crate are
/// self-contained; build.rs rejects drift from `branding/` in the workspace.
pub async fn brand_asset(Path(file): Path<String>) -> Response {
    let (body, ctype): (&'static [u8], &str) = match file.as_str() {
        "mark.png" => (include_bytes!("../static/brand/mark.png"), "image/png"),
        "favicon.png" => (include_bytes!("../static/brand/favicon.png"), "image/png"),
        "wordmark.png" => (include_bytes!("../static/brand/wordmark.png"), "image/png"),
        "wordmark-ink.png" => (
            include_bytes!("../static/brand/wordmark-ink.png"),
            "image/png",
        ),
        "wordmark-vellum.png" => (
            include_bytes!("../static/brand/wordmark-vellum.png"),
            "image/png",
        ),
        "colors.json" => (
            include_bytes!("../static/brand/colors.json"),
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

/// The app's own UI modules — `ax-*` custom elements and the token sheet.
///
/// Separate from `vendor/` (third-party) and from `packages/lattice` (a
/// published library with its own release cycle). Embedded the same way, so
/// nested paths work and there is no build step: the browser imports these as a
/// native ES module graph.
#[derive(rust_embed::RustEmbed)]
#[folder = "static/ui/"]
struct UiAssets;

pub async fn ui_asset(Path(file): Path<String>) -> Response {
    let Some(content) = UiAssets::get(&file) else {
        return (StatusCode::NOT_FOUND, "ui asset not found").into_response();
    };
    let ctype = mime_guess::from_path(&file)
        .first_or_octet_stream()
        .as_ref()
        .to_string();
    ([(header::CONTENT_TYPE, ctype)], content.data.into_owned()).into_response()
}

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
    /// Explicitly remove the configured local token guard. A missing budget field
    /// cannot express this because `None` also means "leave unchanged" in a
    /// PATCH request.
    #[serde(default)]
    pub clear_token_budget: bool,
    pub restart_now: Option<bool>,
}

#[derive(Serialize)]
pub struct AgentPatchResponse {
    pub agent_id: String,
    pub restarted: bool,
    pub message: String,
}

fn apply_token_budget_patch(
    current: &mut Option<axocoatl_config::TokenBudgetYaml>,
    patch: &AgentPatch,
) -> Result<(), String> {
    use axocoatl_config::OverflowPolicyYaml;

    let configures_budget = patch.per_call_budget.is_some()
        || patch.per_execution_budget.is_some()
        || patch.overflow_policy.is_some();
    if patch.clear_token_budget {
        if configures_budget {
            return Err(
                "clear_token_budget cannot be combined with token budget values".to_string(),
            );
        }
        *current = None;
        return Ok(());
    }
    if !configures_budget {
        return Ok(());
    }

    let mut budget = current.clone().unwrap_or(axocoatl_config::TokenBudgetYaml {
        per_call: 4096,
        per_execution: 16000,
        overflow_policy: OverflowPolicyYaml::Warn,
    });
    if let Some(value) = patch.per_call_budget {
        budget.per_call = value;
    }
    if let Some(value) = patch.per_execution_budget {
        budget.per_execution = value;
    }
    if let Some(policy) = patch.overflow_policy.as_deref() {
        budget.overflow_policy = match policy {
            "abort" => OverflowPolicyYaml::Abort,
            "warn" => OverflowPolicyYaml::Warn,
            // Kept for compatibility with older configs. The UI presents the
            // current `warn` spelling, but API clients may still send it.
            "summarize" => OverflowPolicyYaml::Summarize,
            _ => return Err(format!("Unknown overflow policy '{policy}'")),
        };
    }
    *current = Some(budget);
    Ok(())
}

/// Update an agent's in-memory config. The next time the agent is restarted
/// (or if `restart_now: true`) the new prompt/model/budget take effect.
/// This is in-memory only for this session — save-to-YAML is a later session.
pub async fn patch_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<AgentPatch>,
) -> Result<Json<AgentPatchResponse>, (StatusCode, Json<ErrorResponse>)> {
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
        // Validate the whole budget transition before mutating any agent
        // fields so a rejected PATCH cannot partially apply identity edits.
        let mut next_token_budget = agent.token_budget.clone();
        apply_token_budget_patch(&mut next_token_budget, &body)
            .map_err(|error| (StatusCode::BAD_REQUEST, Json(ErrorResponse { error })))?;
        if let Some(n) = body.name.as_ref() {
            agent.name = n.clone();
        }
        if let Some(m) = body.model.as_ref() {
            agent.model = m.clone();
        }
        if let Some(sp) = body.system_prompt.as_ref() {
            agent.system_prompt = Some(sp.clone());
        }
        if let Some(d) = body.depends_on.as_ref() {
            agent.depends_on = d.clone();
        }
        agent.token_budget = next_token_budget;
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
    /// Run this call statelessly — no read/write of the agent's persistent
    /// session or checkpoint, so the override fully controls the call and
    /// independent inputs don't anchor on each other. Implied when an override
    /// is set; set explicitly to isolate even without one (scoring/eval).
    #[serde(default)]
    pub stateless: Option<bool>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// One outward execution's token accounting. The numeric fields always carry
/// the best known value; `token_usage_known == false` means that value is a
/// lower bound because at least one dispatched provider call returned no
/// terminal accounting.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExecutionUsageResponse {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_tokens: usize,
    pub total_tokens: usize,
    pub token_usage_known: bool,
}

impl ExecutionUsageResponse {
    fn new(usage: &axocoatl_core::TokenUsageStats, token_usage_known: bool) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens.unwrap_or(0),
            total_tokens: usage.total(),
            token_usage_known,
        }
    }
}

#[derive(Serialize)]
pub struct ExecuteResponse {
    pub output: String,
    #[serde(flatten)]
    pub usage: ExecutionUsageResponse,
}

#[derive(Serialize)]
pub struct MeasuredErrorResponse {
    pub error: String,
    #[serde(flatten)]
    pub usage: ExecutionUsageResponse,
}

fn measured_error_response(
    status: StatusCode,
    error: impl ToString,
    usage: &axocoatl_core::TokenUsageStats,
    token_usage_known: bool,
) -> (StatusCode, Json<MeasuredErrorResponse>) {
    (
        status,
        Json(MeasuredErrorResponse {
            error: error.to_string(),
            usage: ExecutionUsageResponse::new(usage, token_usage_known),
        }),
    )
}

fn measured_daemon_failure_response(
    failure: axocoatl_daemon::MeasuredDaemonFailure,
) -> (StatusCode, Json<MeasuredErrorResponse>) {
    let status = if matches!(
        &failure.error,
        axocoatl_daemon::DaemonError::AttemptConflict(_)
            | axocoatl_daemon::DaemonError::SessionConflict(_)
    ) {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    measured_error_response(
        status,
        failure.error,
        &failure.token_usage,
        failure.token_usage_known,
    )
}

fn workflow_error_response(
    status: StatusCode,
    error: axocoatl_daemon::DaemonError,
) -> (StatusCode, Json<MeasuredErrorResponse>) {
    let (usage, token_usage_known) = error
        .workflow_token_usage()
        .map(|(usage, known)| (usage.clone(), known))
        .unwrap_or_else(|| (axocoatl_core::TokenUsageStats::default(), true));
    measured_error_response(status, error, &usage, token_usage_known)
}

pub async fn execute_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(body): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, Json<MeasuredErrorResponse>)> {
    let daemon = state.read().await;
    // An override is meaningless without isolation, so it implies stateless; an
    // explicit `stateless: true` isolates even without one (scoring over inputs).
    let stateless = body.stateless.unwrap_or(false)
        || body.system_override.is_some()
        || body.model_override.is_some();
    let input = axocoatl_core::AgentInput::text(body.input)
        .with_system_override(body.system_override)
        .with_model_override(body.model_override)
        .with_stateless(stateless);
    match daemon.execute_agent_input_measured(&agent_id, input).await {
        Ok(measured) => Ok(Json(ExecuteResponse {
            usage: ExecutionUsageResponse::new(
                &measured.output.token_usage,
                measured.token_usage_known,
            ),
            output: measured.output.content,
        })),
        Err(failure) => Err(measured_daemon_failure_response(failure)),
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
                status: format!("{status:?}"),
            }))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Agent '{agent_id}' not found"),
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
    use axocoatl_config::{AutomationNodeKind, AutomationTrigger};

    let daemon = state.read().await;
    let mut workflows: Vec<WorkflowInfo> = daemon
        .list_automations()
        .await
        .into_iter()
        .filter(|automation| matches!(&automation.trigger, AutomationTrigger::Manual))
        .map(|automation| {
            let agents: Vec<String> = automation
                .nodes
                .iter()
                .filter_map(|node| match &node.kind {
                    AutomationNodeKind::Agent { agent_id, .. } => Some(agent_id.clone()),
                    _ => None,
                })
                .collect();
            let entry_point = automation.nodes.iter().find_map(|node| {
                let has_incoming = automation.edges.iter().any(|edge| edge.to == node.id);
                match (&node.kind, has_incoming) {
                    (AutomationNodeKind::Agent { agent_id, .. }, false) => Some(agent_id.clone()),
                    _ => None,
                }
            });
            WorkflowInfo {
                id: automation.id,
                name: automation.name,
                agents,
                entry_point,
            }
        })
        .collect();
    workflows.sort_by(|a, b| a.id.cmp(&b.id));
    Json(workflows)
}

#[derive(Serialize)]
pub struct WorkflowResponse {
    pub workflow_id: String,
    pub output: String,
    pub agent_outputs: Vec<WorkflowAgentOutput>,
    #[serde(flatten)]
    pub usage: ExecutionUsageResponse,
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
) -> Result<Json<WorkflowResponse>, (StatusCode, Json<MeasuredErrorResponse>)> {
    use axocoatl_config::AutomationTrigger;

    let context = {
        let daemon = state.read().await;
        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon)
    };
    let automation = context
        .get_automation(&workflow_id)
        .await
        .filter(|automation| matches!(&automation.trigger, AutomationTrigger::Manual))
        .ok_or_else(|| {
            measured_error_response(
                StatusCode::NOT_FOUND,
                format!("manual automation '{workflow_id}' not found"),
                &axocoatl_core::TokenUsageStats::default(),
                true,
            )
        })?;
    let result = axocoatl_daemon::automation_executor::execute_automation_in_context(
        &context,
        &automation,
        &body.input,
    )
    .await;
    axocoatl_daemon::record_automation_outcome(&context, &automation, &result);
    match result {
        Ok(output) => {
            if let Some(error) = output.terminal_error() {
                return Err(workflow_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                ));
            }
            Ok(Json(WorkflowResponse {
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
                usage: ExecutionUsageResponse::new(
                    &output.total_token_usage,
                    output.token_usage_known,
                ),
                completed_agents: output.completed_agents,
                failed_agents: Vec::new(),
            }))
        }
        Err(error) => Err(workflow_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            error,
        )),
    }
}

// --- Token endpoints ---

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AgentTokenUsage {
    /// Exact live actor identity. Session actors use `{session}:{agent}`.
    pub agent_id: String,
    /// Configured Agent template when the actor identity can be mapped without
    /// suffix guessing. `None` keeps unknown/ephemeral actor scopes explicit.
    pub template_agent_id: Option<String>,
    /// `global`, `session`, or `other`.
    pub scope: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_tokens: usize,
    pub total_tokens: usize,
    /// False means the numbers are a known subtotal for this actor.
    pub token_usage_known: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TokenReport {
    pub per_agent: Vec<AgentTokenUsage>,
    pub total_input: usize,
    pub total_output: usize,
    pub total_reasoning: usize,
    pub total: usize,
    /// True only when every included actor has complete lifetime accounting.
    pub token_usage_known: bool,
}

fn classify_token_actor(
    actor_id: &str,
    configured_agents: &std::collections::HashSet<String>,
    session_ids: &std::collections::HashSet<String>,
) -> (Option<String>, &'static str) {
    if configured_agents.contains(actor_id) {
        return (Some(actor_id.to_string()), "global");
    }
    if let Some((session_id, template_id)) = actor_id.split_once(':') {
        if session_ids.contains(session_id) && configured_agents.contains(template_id) {
            return (Some(template_id.to_string()), "session");
        }
    }
    (None, "other")
}

fn build_token_report(
    usages: Vec<(String, axocoatl_core::MeasuredTokenUsage)>,
    configured_agents: &std::collections::HashSet<String>,
    session_ids: &std::collections::HashSet<String>,
) -> TokenReport {
    let mut per_agent = Vec::with_capacity(usages.len());
    let mut total_input = 0_usize;
    let mut total_output = 0_usize;
    let mut total_reasoning = 0_usize;
    let mut token_usage_known = true;
    for (agent_id, measured) in usages {
        let usage = measured.usage;
        token_usage_known &= measured.complete;
        total_input = total_input.saturating_add(usage.input_tokens);
        total_output = total_output.saturating_add(usage.output_tokens);
        total_reasoning = total_reasoning.saturating_add(usage.reasoning_tokens.unwrap_or(0));
        let (template_agent_id, scope) =
            classify_token_actor(&agent_id, configured_agents, session_ids);
        per_agent.push(AgentTokenUsage {
            agent_id,
            template_agent_id,
            scope: scope.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens.unwrap_or(0),
            total_tokens: usage.total(),
            token_usage_known: measured.complete,
        });
    }
    TokenReport {
        per_agent,
        total_input,
        total_output,
        total_reasoning,
        total: total_input
            .saturating_add(total_output)
            .saturating_add(total_reasoning),
        token_usage_known,
    }
}

pub async fn token_report(State(state): State<AppState>) -> Json<TokenReport> {
    let daemon = state.read().await;
    let configured_agents = daemon
        .config
        .agents
        .iter()
        .map(|agent| agent.id.clone())
        .collect::<std::collections::HashSet<_>>();
    let session_ids = daemon
        .list_sessions()
        .await
        .into_iter()
        .map(|session| session.id)
        .collect::<std::collections::HashSet<_>>();
    let mut usages = Vec::new();
    for id in daemon.agent_registry.list_ids().await {
        if let Some(actor) = daemon.agent_registry.get(&id).await {
            if let Ok(u) = axocoatl_actor::get_agent_measured_token_usage(&actor).await {
                usages.push((id.to_string(), u));
            }
        }
    }
    Json(build_token_report(usages, &configured_agents, &session_ids))
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
const MCP_CATALOG: &str = include_str!("../static/brand/mcp-catalog.json");
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

/// Connect a catalog server to the process-local MCP registry.
/// Body: `{ slug, env: {KEY: value, …}, requires: {key: value} }`.
/// We resolve the template (substituting `{{KEY}}` in args/env with the
/// user's values), build an McpTransportType, and ask the registry to connect.
/// Durable reconnection remains explicit YAML configuration so entered secrets
/// are never written to disk as a surprise side effect of this route.
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
    pub last_error: Option<String>,
    pub run_count: u64,
}

pub async fn list_schedules(State(state): State<AppState>) -> Json<Vec<ScheduleEntry>> {
    use axocoatl_config::AutomationTrigger;
    use std::collections::HashMap;

    let (automations, table) = {
        let daemon = state.read().await;
        (
            daemon.list_automations().await,
            daemon.schedule_table.clone(),
        )
    };
    let observations: HashMap<String, axocoatl_daemon::ScheduleState> = table
        .lock()
        .map(|rows| {
            rows.iter()
                .cloned()
                .map(|row| (row.automation_id.clone(), row))
                .collect()
        })
        .unwrap_or_default();
    let mut entries: Vec<ScheduleEntry> = automations
        .into_iter()
        .filter_map(|automation| {
            // Legacy `pro:` schedule records remain represented by the
            // compatibility proactive endpoint. Every other canonical
            // Schedule projects here.
            if automation.id.starts_with("pro:") {
                return None;
            }
            let AutomationTrigger::Schedule { every, input } = automation.trigger else {
                return None;
            };
            let observation = observations.get(&automation.id);
            let interval_secs = observation
                .map(|row| row.interval_secs)
                .unwrap_or_else(|| axocoatl_daemon::parse_interval(&every).unwrap_or(0));
            Some(ScheduleEntry {
                id: automation
                    .id
                    .strip_prefix("sched:")
                    .unwrap_or(&automation.id)
                    .to_string(),
                name: automation.name,
                workflow: automation.id.clone(),
                every,
                input: input.unwrap_or_default(),
                enabled: automation.enabled,
                interval_secs,
                last_fired_unix: observation.and_then(|row| row.last_fired_unix),
                next_fire_unix: observation.and_then(|row| row.next_fire_unix()),
                last_outcome: observation.and_then(|row| row.last_outcome.clone()),
                last_error: observation.and_then(|row| row.last_error.clone()),
                run_count: observation.map(|row| row.run_count).unwrap_or(0),
            })
        })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    Json(entries)
}

// --- Directory sessions ---

fn session_image_backend_conflict(
    backend: &str,
    template: Option<&str>,
    image: Option<&str>,
) -> Option<String> {
    let image = image.map(str::trim).filter(|image| !image.is_empty())?;
    if backend != "e2b" {
        return None;
    }
    Some(format!(
        "this Axocoatl daemon uses the E2B template '{}' and cannot honor per-Session OCI image '{image}'. Clear the Base image or switch sandbox.backend to podman",
        template.unwrap_or("base")
    ))
}

async fn session_runtime_capability(state: &AppState) -> serde_json::Value {
    let daemon = state.read().await;
    let backend = daemon.config.sandbox.backend.as_str();
    let template = daemon
        .config
        .sandbox
        .e2b
        .as_ref()
        .map(|config| config.template.as_str());
    serde_json::json!({
        "backend": backend,
        "image_mode": if backend == "e2b" { "template" } else { "oci" },
        "supports_session_image": backend != "e2b",
        "supports_preview": backend != "e2b",
        "template": if backend == "e2b" { template } else { None },
        "auto_approve_devcontainer_setup": daemon.config.sandbox.allow_post_create_command,
    })
}

async fn reject_unsupported_session_ports(
    state: &AppState,
    exposed_ports: &[u16],
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if exposed_ports.is_empty() || state.read().await.config.sandbox.backend != "e2b" {
        return Ok(());
    }
    Err(err(
        StatusCode::BAD_REQUEST,
        "this Axocoatl daemon uses E2B, which does not expose Session Preview ports; clear Exposed ports or switch sandbox.backend to podman",
    ))
}

async fn reject_unsupported_session_image(
    state: &AppState,
    working_dir: Option<&std::path::Path>,
    explicit_image: Option<&str>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let (backend, template) = {
        let daemon = state.read().await;
        (
            daemon.config.sandbox.backend.clone(),
            daemon
                .config
                .sandbox
                .e2b
                .as_ref()
                .map(|config| config.template.clone()),
        )
    };
    if backend != "e2b" {
        return Ok(());
    }

    let requested_image = match explicit_image
        .map(str::trim)
        .filter(|image| !image.is_empty())
    {
        Some(image) => Some(image.to_string()),
        None => match working_dir {
            Some(directory) => axocoatl_session::DevContainer::load(directory)
                .map_err(|error| err(StatusCode::BAD_REQUEST, error.to_string()))?
                .and_then(|(_, config)| config.image),
            None => None,
        },
    };
    if let Some(message) =
        session_image_backend_conflict(&backend, template.as_deref(), requested_image.as_deref())
    {
        return Err(err(StatusCode::BAD_REQUEST, message));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspaceBody {
    pub path: String,
    /// Omit to use the folder basename for a new Workspace or preserve the
    /// existing custom name when the canonical path is already registered.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameWorkspaceBody {
    pub name: String,
}

pub async fn list_workspaces(
    State(state): State<AppState>,
) -> Json<Vec<axocoatl_session::Workspace>> {
    Json(state.read().await.list_workspaces().await)
}

pub async fn create_workspace(
    State(state): State<AppState>,
    Json(body): Json<CreateWorkspaceBody>,
) -> Result<Json<axocoatl_session::Workspace>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .create_workspace(&body.path, body.name.as_deref())
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn get_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_session::Workspace>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .get_workspace(&id)
        .await
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("workspace '{id}' not found")))
}

pub async fn rename_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RenameWorkspaceBody>,
) -> Result<Json<axocoatl_session::Workspace>, (StatusCode, Json<ErrorResponse>)> {
    if state.read().await.get_workspace(&id).await.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("workspace '{id}' not found"),
        ));
    }
    state
        .read()
        .await
        .rename_workspace(&id, &body.name)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn list_workspace_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<axocoatl_session::Session>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .list_workspace_sessions(&id)
        .await
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("workspace '{id}' not found")))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspaceSessionBody {
    pub name: String,
    pub mode: axocoatl_session::SessionMode,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    #[serde(default)]
    pub image: Option<String>,
    /// Exact project setup proposed in the creation UI. Presence is not
    /// consent; `setup_approved` must independently be true.
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub setup_approved: bool,
    /// Distinguishes a deliberate no-setup decision from a legacy Session
    /// whose environment has never been reviewed.
    #[serde(default)]
    pub setup_reviewed: bool,
}

pub async fn create_workspace_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateWorkspaceSessionBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    let workspace = state
        .read()
        .await
        .get_workspace(&id)
        .await
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("workspace '{id}' not found")))?;
    reject_unsupported_session_image(
        &state,
        Some(workspace.canonical_path.as_path()),
        body.image.as_deref(),
    )
    .await?;
    reject_unsupported_session_ports(&state, &body.exposed_ports).await?;
    state
        .read()
        .await
        .create_session_in_workspace(
            &id,
            &body.name,
            body.mode,
            body.enabled_skills,
            body.exposed_ports,
            body.image,
            body.setup_command,
            body.setup_approved,
            body.setup_reviewed,
        )
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionBody {
    pub name: String,
    pub working_dir: String,
    /// Run mode — `{"kind":"single_agent","agent_id":"coder"}` or
    /// `{"kind":"lattice"}`.
    pub mode: axocoatl_session::SessionMode,
    /// Skill ids the session's agents may fire as tools.
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Ports to publish from the sandbox container to loopback. Empty means
    /// no Preview exposure.
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    /// Base OCI image for the session sandbox. `None` means "use the
    /// configured default" (alpine, unless devcontainer.json overrides).
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub setup_approved: bool,
    #[serde(default)]
    pub setup_reviewed: bool,
}

pub async fn list_sessions(State(state): State<AppState>) -> Json<Vec<axocoatl_session::Session>> {
    Json(state.read().await.list_sessions().await)
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    reject_unsupported_session_image(
        &state,
        Some(std::path::Path::new(&body.working_dir)),
        body.image.as_deref(),
    )
    .await?;
    reject_unsupported_session_ports(&state, &body.exposed_ports).await?;
    let daemon = state.read().await;
    daemon
        .create_session_with_environment(
            &body.name,
            &body.working_dir,
            body.mode,
            body.enabled_skills,
            body.exposed_ports,
            body.image,
            body.setup_command,
            body.setup_approved,
            body.setup_reviewed,
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

/// GET /api/sessions/{id}/turns — canonical user-visible Session turns.
pub async fn session_turns(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<axocoatl_session::SessionTurn>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .list_session_turns(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(Serialize)]
pub struct ActiveSessionTurnResponse {
    /// Exact live run in this daemon process. `null` is an authoritative idle
    /// answer, not an inference from the durable turn ledger.
    pub run: Option<axocoatl_daemon::RunState>,
}

/// GET /api/sessions/{id}/active-turn — exact current Stop ownership plus the
/// reconnectable per-agent buffers available for that run.
pub async fn active_session_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ActiveSessionTurnResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .active_session_run(&id)
        .await
        .map(|run| Json(ActiveSessionTurnResponse { run }))
        .map_err(attempt_err)
}

/// GET /api/sessions/{id}/turns/{turn_id} — one canonical Session turn.
pub async fn session_turn(
    State(state): State<AppState>,
    Path((id, turn_id)): Path<(String, String)>,
) -> Result<Json<axocoatl_session::SessionTurn>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .get_session_turn(&id, &turn_id)
        .await
        .map_err(attempt_err)?
        .map(Json)
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                format!("turn {turn_id} not found in session {id}"),
            )
        })
}

#[derive(Deserialize)]
pub struct SessionTurnSearchQuery {
    pub q: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// GET /api/session-turns/search — literal search over canonical transcripts.
pub async fn search_session_turns(
    State(state): State<AppState>,
    Query(query): Query<SessionTurnSearchQuery>,
) -> Result<Json<Vec<axocoatl_session::SessionTurnSearchHit>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .search_session_turns(query.session_id.as_deref(), &query.q)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(Deserialize)]
pub struct SessionExportQuery {
    #[serde(default)]
    pub format: Option<String>,
}

/// GET /api/sessions/{id}/export — canonical transcript as Markdown or JSON.
pub async fn export_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<SessionExportQuery>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let format = query.format.as_deref().unwrap_or("markdown");
    let daemon = state.read().await;
    let (body, content_type, extension) = match format {
        "json" => (
            daemon.export_session_json(&id).await.map_err(attempt_err)?,
            "application/json; charset=utf-8",
            "json",
        ),
        "md" | "markdown" => (
            daemon
                .export_session_markdown(&id)
                .await
                .map_err(attempt_err)?,
            "text/markdown; charset=utf-8",
            "md",
        ),
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "format must be markdown or json",
            ));
        }
    };
    let filename = format!("session-{}.{}", safe_download_name(&id), extension);
    let mut response = Response::new(axum::body::Body::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    Ok(response)
}

#[derive(serde::Deserialize)]
pub struct RewindSessionBody {
    /// Canonical boundary. `None` supersedes the complete Session transcript.
    #[serde(default)]
    pub keep_through_turn_id: Option<String>,
    /// Legacy raw checkpoint-message boundary retained for compatibility.
    #[serde(default)]
    pub keep: Option<usize>,
}

/// POST /api/sessions/{id}/rewind — truncate the transcript to `keep` messages
/// and drop the live actor so the next turn resumes from the truncated state.
pub async fn rewind_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RewindSessionBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    if body.keep_through_turn_id.is_some() || body.keep.is_none() {
        daemon
            .rewind_session_to_turn(&id, body.keep_through_turn_id.as_deref())
            .await
            .map(|turns| Json(serde_json::json!({ "ok": true, "turns": turns })))
            .map_err(attempt_err)
    } else {
        daemon
            .rewind_session(&id, body.keep.unwrap_or(0))
            .await
            .map(|_| Json(serde_json::json!({ "ok": true })))
            .map_err(attempt_err)
    }
}

const MAX_SESSION_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_SESSION_DOCUMENT_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Deserialize)]
pub struct SessionAttachmentQuery {
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchSessionAttachmentBody {
    pub scope: String,
}

fn parse_context_scope(
    scope: Option<&str>,
) -> Result<axocoatl_session::TurnContextScope, (StatusCode, Json<ErrorResponse>)> {
    match scope.unwrap_or("this_turn") {
        "this_turn" => Ok(axocoatl_session::TurnContextScope::ThisTurn),
        "session" => Ok(axocoatl_session::TurnContextScope::Session),
        _ => Err(err(
            StatusCode::BAD_REQUEST,
            "scope must be this_turn or session",
        )),
    }
}

fn session_extraction_snapshot(
    entry: &axocoatl_memory::files::FileEntry,
) -> axocoatl_session::SessionAttachmentExtractionSnapshot {
    use axocoatl_memory::files::ExtractionStatus;
    let status = match entry.extraction.status {
        ExtractionStatus::Complete => {
            let truncated = entry
                .extraction
                .extracted_text
                .as_ref()
                .is_some_and(|metadata| metadata.truncated)
                || entry
                    .extraction
                    .ocr_text
                    .as_ref()
                    .is_some_and(|metadata| metadata.truncated);
            if truncated {
                axocoatl_session::SessionAttachmentExtractionStatus::Partial
            } else {
                axocoatl_session::SessionAttachmentExtractionStatus::Ready
            }
        }
        ExtractionStatus::NotApplicable => {
            axocoatl_session::SessionAttachmentExtractionStatus::Unsupported
        }
        ExtractionStatus::Unavailable => {
            axocoatl_session::SessionAttachmentExtractionStatus::Failed
        }
        ExtractionStatus::Unknown => axocoatl_session::SessionAttachmentExtractionStatus::Pending,
    };
    let extracted_char_count = entry
        .inline_text()
        .map(|text| u64::try_from(text.chars().count()).unwrap_or(u64::MAX));
    let truncated = entry
        .extraction
        .extracted_text
        .as_ref()
        .is_some_and(|metadata| metadata.truncated)
        || entry
            .extraction
            .ocr_text
            .as_ref()
            .is_some_and(|metadata| metadata.truncated);
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "extraction_version".to_string(),
        serde_json::Value::Number(entry.extraction.version.into()),
    );
    axocoatl_session::SessionAttachmentExtractionSnapshot {
        status,
        extractor: None,
        extracted_char_count,
        truncated,
        error: None,
        metadata,
    }
}

pub async fn list_session_attachments(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<axocoatl_session::SessionAttachmentRef>>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .list_session_attachments(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn get_session_attachment(
    State(state): State<AppState>,
    Path((id, reference_id)): Path<(String, String)>,
) -> Result<Json<axocoatl_session::SessionAttachmentRef>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .get_session_attachment(&id, &reference_id)
        .await
        .map_err(attempt_err)?
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "session attachment not found"))
}

/// Upload one bounded file and create a Session-owned reference to its
/// immutable content-addressed blob. The multipart field is read chunk by
/// chunk so an absent or false Content-Length cannot bypass the product cap.
pub async fn upload_session_attachment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<SessionAttachmentQuery>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<axocoatl_session::SessionAttachmentRef>, (StatusCode, Json<ErrorResponse>)> {
    if state.read().await.get_session(&id).await.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("session {id} not found"),
        ));
    }
    let scope = parse_context_scope(query.scope.as_deref())?;
    let mut field = loop {
        match multipart.next_field().await {
            Ok(Some(field)) if field.name() == Some("file") => break field,
            Ok(Some(_)) => continue,
            Ok(None) => return Err(err(StatusCode::BAD_REQUEST, "missing file field")),
            Err(error) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!("multipart error: {error}"),
                ));
            }
        }
    };
    let filename = field.file_name().unwrap_or("attachment").to_string();
    let declared_mime = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();
    let max_bytes = if declared_mime.starts_with("image/") {
        MAX_SESSION_IMAGE_BYTES
    } else {
        MAX_SESSION_DOCUMENT_BYTES
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("attachment read failed: {error}"),
        )
    })? {
        let next_len = bytes.len().saturating_add(chunk.len());
        if u64::try_from(next_len).unwrap_or(u64::MAX) > max_bytes {
            return Err(err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachment exceeds the {} MiB limit",
                    max_bytes / 1024 / 1024
                ),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "attachment is empty"));
    }

    let file_store = { state.read().await.file_store.clone() };
    let entry = tokio::task::spawn_blocking({
        let filename = filename.clone();
        let declared_mime = declared_mime.clone();
        move || {
            let mut guard = file_store.blocking_lock();
            guard.store_reader_with_output(
                Cursor::new(bytes),
                None,
                &filename,
                &declared_mime,
                axocoatl_memory::files::BlobIngestLimit::new(max_bytes),
                |content, mime| {
                    axocoatl_memory::extract::extract_with_limits(
                        content,
                        mime,
                        &filename,
                        axocoatl_memory::extract::ExtractionLimits::default(),
                    )
                },
            )
        }
    })
    .await
    .map_err(|error| err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
    .map_err(|error| match error {
        axocoatl_memory::MemoryError::BlobTooLarge { .. } => {
            err(StatusCode::PAYLOAD_TOO_LARGE, error.to_string())
        }
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    })?;

    let presentation =
        axocoatl_memory::files::FilePresentation::from_upload(&filename, &declared_mime);
    let reference = axocoatl_session::CreateSessionAttachmentRef {
        reference_id: None,
        session_id: id,
        blob_id: format!("sha256:{}", entry.id),
        display_name: presentation.display_name,
        declared_mime: Some(presentation.declared_media_type),
        size: entry.size,
        scope,
        extraction: session_extraction_snapshot(&entry),
        metadata: serde_json::Map::new(),
    };
    state
        .read()
        .await
        .create_session_attachment(reference)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn patch_session_attachment(
    State(state): State<AppState>,
    Path((id, reference_id)): Path<(String, String)>,
    Json(body): Json<PatchSessionAttachmentBody>,
) -> Result<Json<axocoatl_session::SessionAttachmentRef>, (StatusCode, Json<ErrorResponse>)> {
    let scope = parse_context_scope(Some(&body.scope))?;
    state
        .read()
        .await
        .update_session_attachment_scope(&id, &reference_id, scope)
        .await
        .map(Json)
        .map_err(attempt_err)
}

pub async fn delete_session_attachment(
    State(state): State<AppState>,
    Path((id, reference_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .detach_session_attachment(&id, &reference_id)
        .await
        .map(|_| Json(serde_json::json!({ "ok": true })))
        .map_err(attempt_err)
}

fn read_session_attachment_bytes(
    file_store: &axocoatl_memory::files::FileStore,
    reference: &axocoatl_session::SessionAttachmentRef,
) -> Result<Vec<u8>, axocoatl_memory::MemoryError> {
    let blob_id = reference
        .blob_id
        .strip_prefix("sha256:")
        .unwrap_or(&reference.blob_id);
    file_store.read_bytes(blob_id)
}

/// Serve raw attachment bytes only through the owning Session relation. MIME
/// is untrusted upload metadata, so executable/browser-active types download;
/// only images and PDFs may render inline, always with nosniff.
pub async fn get_session_attachment_content(
    State(state): State<AppState>,
    Path((id, reference_id)): Path<(String, String)>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let reference = daemon
        .get_session_attachment(&id, &reference_id)
        .await
        .map_err(attempt_err)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "session attachment not found"))?;
    let file_store = daemon.file_store.clone();
    drop(daemon);
    let bytes = {
        let file_store = file_store.lock().await;
        read_session_attachment_bytes(&file_store, &reference).map_err(|error| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("attachment read failed: {error}"),
            )
        })?
    };
    let declared = reference
        .declared_mime
        .as_deref()
        .unwrap_or("application/octet-stream");
    let inline_type = safe_inline_media_type(declared);
    let inline = inline_type.is_some();
    let content_type = inline_type.unwrap_or("application/octet-stream");
    let disposition = if inline { "inline" } else { "attachment" };
    let filename = safe_download_name(&reference.display_name);
    let mut response = Response::new(axum::body::Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("{disposition}; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    if content_type == "application/pdf" {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            "sandbox; default-src 'none'".parse().unwrap(),
        );
    }
    Ok(response)
}

/// User-declared media types are not an authority for active browser content.
/// Only inert raster formats and sandboxed PDF render inline; SVG/HTML/XML and
/// every unknown type are downloaded as opaque bytes.
fn safe_inline_media_type(value: &str) -> Option<&'static str> {
    match value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/avif" => Some("image/avif"),
        "application/pdf" => Some("application/pdf"),
        _ => None,
    }
}

fn safe_download_name(value: &str) -> String {
    let name: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(160)
        .collect();
    if name.is_empty() {
        "attachment".to_string()
    } else {
        name
    }
}

fn safe_attachment_response(bytes: Vec<u8>, declared_mime: &str, name: &str) -> Response {
    let inline_type = safe_inline_media_type(declared_mime);
    let content_type = inline_type.unwrap_or("application/octet-stream");
    let disposition = if inline_type.is_some() {
        "inline"
    } else {
        "attachment"
    };
    let filename = safe_download_name(name);
    let content_disposition =
        HeaderValue::from_str(&format!("{disposition}; filename=\"{filename}\""))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"attachment\""));

    let mut response = Response::new(axum::body::Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, content_disposition);
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    if content_type == "application/pdf" {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'"),
        );
    }
    response
}

fn export_content_disposition(name: &str, extension: &str) -> HeaderValue {
    let filename = format!("{}.{}", safe_download_name(name), extension);
    HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"chat.json\""))
}

/// Attempt and Session lifecycle conflicts are safe concurrency guards, not
/// malformed requests. Keep all other failures on the existing 400 contract.
fn attempt_err(error: axocoatl_daemon::DaemonError) -> (StatusCode, Json<ErrorResponse>) {
    let status = if matches!(
        &error,
        axocoatl_daemon::DaemonError::AttemptConflict(_)
            | axocoatl_daemon::DaemonError::SessionConflict(_)
    ) {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(ErrorResponse {
            error: error.to_string(),
        }),
    )
}

#[derive(Serialize)]
pub struct WaysControlErrorResponse {
    pub error: String,
    pub control_usage: axocoatl_daemon::git::ControlUsage,
}

pub type WaysControlRouteError = (StatusCode, Json<WaysControlErrorResponse>);

fn ways_control_err(error: axocoatl_daemon::WaysControlFailure) -> WaysControlRouteError {
    let status = if matches!(
        &error.error,
        axocoatl_daemon::DaemonError::AttemptConflict(_)
            | axocoatl_daemon::DaemonError::SessionConflict(_)
    ) {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(WaysControlErrorResponse {
            error: error.to_string(),
            control_usage: error.control_usage,
        }),
    )
}

fn ways_readiness_err(error: RouteError) -> WaysControlRouteError {
    let (status, Json(error)) = error;
    (
        status,
        Json(WaysControlErrorResponse {
            error: error.error,
            // The readiness guard runs before any Plan/Judge provider call.
            control_usage: axocoatl_daemon::git::ControlUsage::known(
                None,
                0,
                &axocoatl_core::TokenUsageStats::default(),
            ),
        }),
    )
}

type RouteError = (StatusCode, Json<ErrorResponse>);

/// Return the HTTP lifecycle error for a Session-backed product surface.
///
/// History, attachments, environment repair, and attempt resolution have
/// explicit non-Ready semantics and do not use this guard. Every route that
/// can read or mutate the live checkout, start work, or attach to its runtime
/// does. Keeping the policy here prevents each handler from inventing a
/// different status code or accidentally treating a missing Session as an
/// environment conflict.
fn session_environment_route_error(
    id: &str,
    session: Option<&axocoatl_session::Session>,
) -> Option<RouteError> {
    use axocoatl_session::{SessionEnvironmentState, SessionStatus};

    let session = match session {
        Some(session) => session,
        None => {
            return Some(err(
                StatusCode::NOT_FOUND,
                format!("session '{id}' not found"),
            ));
        }
    };

    if session.status == SessionStatus::Closed {
        return Some(err(
            StatusCode::CONFLICT,
            format!(
                "Session '{}' is closed; reopen it explicitly before using its runtime",
                session.name
            ),
        ));
    }

    let action = match session.environment.state {
        SessionEnvironmentState::Ready => return None,
        SessionEnvironmentState::AwaitingApproval => {
            "needs setup approval; review its Session environment before continuing".to_string()
        }
        SessionEnvironmentState::Preparing => {
            "is still preparing; wait for its Session environment to become Ready".to_string()
        }
        SessionEnvironmentState::Unprepared => {
            "is not prepared; review or rebuild its Session environment before continuing"
                .to_string()
        }
        SessionEnvironmentState::Failed => session
            .environment
            .error
            .as_deref()
            .map(|cause| {
                format!("environment failed ({cause}); change or rebuild its Session environment")
            })
            .unwrap_or_else(|| {
                "environment failed; change or rebuild its Session environment".to_string()
            }),
    };

    Some(err(
        StatusCode::CONFLICT,
        format!("Session '{}' {action}", session.name),
    ))
}

/// Fail closed before a Session-backed HTTP handler reaches the daemon.
async fn require_ready_session(state: &AppState, id: &str) -> Result<(), RouteError> {
    let session = state.read().await.get_session(id).await;
    match session_environment_route_error(id, session.as_ref()) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// GET /api/sessions/{id}/git/status
pub async fn git_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon.git_status(&id).await.map(Json).map_err(attempt_err)
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
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_diff(&id, &q.path)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct VariantDiffQuery {
    pub attempt_set_id: String,
    /// 0-based lane index.
    pub index: usize,
    pub path: String,
}

/// GET /api/sessions/{id}/variants/diff?attempt_set_id=…&index=…&path=…
///
/// One file's before/after **within a single attempt's isolated checkout**. The
/// comparison view calls this per attempt: same path, different answer.
pub async fn session_variant_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<VariantDiffQuery>,
) -> Result<Json<axocoatl_daemon::git::GitDiff>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .variant_diff(&id, &q.attempt_set_id, q.index, &q.path)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// GET /api/sessions/{id}/git/branches
pub async fn git_branches(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::git::GitBranches>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_branches(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct GitCommitBody {
    pub message: Option<String>,
    /// Stage the whole tree first. Absent means commit the index as the user
    /// built it — the only reading under which staging a hunk means anything.
    #[serde(default)]
    pub stage_all: bool,
}

/// POST /api/sessions/{id}/git/commit
pub async fn git_commit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitCommitBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_commit(&id, body.message.as_deref().unwrap_or(""), body.stage_all)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct GitDiscardBody {
    pub path: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct StageBody {
    /// Paths to stage or unstage. Empty means everything.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(serde::Deserialize)]
pub struct HunksQuery {
    pub path: String,
    /// Look at the staged diff instead of the working-tree one.
    #[serde(default)]
    pub staged: bool,
}

/// GET /api/sessions/{id}/git/hunks?path=…&staged=…
pub async fn git_hunks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HunksQuery>,
) -> Result<Json<Vec<axocoatl_daemon::git::Hunk>>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_hunks(&id, &q.path, q.staged)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct HunkBody {
    pub path: String,
    pub index: usize,
    /// True to stage this hunk, false to unstage it.
    #[serde(default = "yes")]
    pub stage: bool,
}

fn yes() -> bool {
    true
}

/// POST /api/sessions/{id}/git/hunk — stage or unstage one hunk.
pub async fn git_apply_hunk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<HunkBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_apply_hunk(&id, &body.path, body.index, body.stage)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// POST /api/sessions/{id}/git/stage
pub async fn git_stage(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StageBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_stage(&id, &body.paths)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// POST /api/sessions/{id}/git/unstage
pub async fn git_unstage(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StageBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_unstage(&id, &body.paths)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// POST /api/sessions/{id}/git/discard
pub async fn git_discard(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitDiscardBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_discard(&id, body.path.as_deref())
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct GitCheckoutBody {
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct GitHunkDiscardBody {
    pub path: String,
    pub index: usize,
}

/// POST /api/sessions/{id}/git/hunk/discard — throw away one unstaged hunk.
pub async fn git_revert_hunk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitHunkDiscardBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_revert_hunk(&id, &body.path, body.index)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// POST /api/sessions/{id}/git/checkout
pub async fn git_checkout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<GitCheckoutBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .git_checkout(&id, &body.reference)
        .await
        .map(Json)
        .map_err(attempt_err)
}

// ── Variants — parallel branch exploration ──────────────────────────────
fn default_variant_count() -> usize {
    3
}

#[derive(serde::Deserialize)]
pub struct VariantsBody {
    pub input: String,
    /// The task these attempts are solving. Defaults to `input`, which keeps
    /// direct prompts and planned instructions on the same contract.
    #[serde(default)]
    pub task: Option<String>,
    /// Number of attempts, all on the agent's configured model. Ignored when
    /// `lanes` is given.
    #[serde(default = "default_variant_count")]
    pub n: usize,
    /// Per-lane configuration. When present this defines the run exactly — one
    /// lane per entry, each with its own model — so a plan can be executed
    /// concurrently by several different (e.g. cheaper, local) models.
    #[serde(default)]
    pub lanes: Option<Vec<axocoatl_daemon::git::LaneConfig>>,
}

impl VariantsBody {
    /// Resolve the wire defaults before crossing into the daemon contract.
    fn into_attempt_run(self) -> (String, String, Vec<axocoatl_daemon::git::LaneConfig>) {
        let task = self.task.unwrap_or_else(|| self.input.clone());
        let lanes = self
            .lanes
            .unwrap_or_else(|| vec![axocoatl_daemon::git::LaneConfig::default(); self.n]);
        (task, self.input, lanes)
    }
}

/// Keep a mutation alive through its daemon-owned completion boundary even if
/// the browser disconnects while awaiting the response. Dropping a normal
/// route future cancels every nested await; Ways launch publishes durable
/// `Preparing` metadata before clone/container setup, so request cancellation
/// there would otherwise strand a partial transaction.
async fn run_request_owned<T, F>(
    operation: &'static str,
    future: F,
) -> Result<T, tokio::sync::oneshot::error::RecvError>
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = future.await;
        if result_tx.send(result).is_err() {
            tracing::info!(
                operation,
                "request disconnected; daemon-owned operation reached its completion boundary"
            );
        }
    });
    result_rx.await
}

/// POST /api/sessions/{id}/variants — start parallel attempts for the session.
/// Returns their durable attempt set; each attempt streams over the WS keyed
/// `{session}#{i}`.
pub async fn session_variants(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VariantsBody>,
) -> Result<Json<axocoatl_daemon::git::AttemptSet>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let (task, input, lanes) = body.into_attempt_run();
    let launch = run_request_owned("Ways launch", async move {
        let daemon = state.read().await;
        daemon
            .execute_session_variants(&id, &task, &input, &lanes)
            .await
    })
    .await
    .map_err(|error| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Ways launch task ended before returning a result: {error}"),
        )
    })?;
    launch.map(Json).map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct AttemptSetQuery {
    pub attempt_set_id: String,
}

/// GET /api/sessions/{id}/variants/status?attempt_set_id=… — per-attempt
/// changed files for Compare.
pub async fn session_variants_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<AttemptSetQuery>,
) -> Result<Json<Vec<axocoatl_daemon::git::VariantStatus>>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .variants_status(&id, &q.attempt_set_id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// GET /api/sessions/{id}/variants/results — the whole comparison in one read:
/// which lanes exist and what each is, their verdicts, their spend, the
/// ranking. What a scoreboard re-attaches to after a reload.
pub async fn session_variants_results(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_daemon::git::RunResults>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon.run_results(&id).await.map(Json).map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct TrajectoryQuery {
    pub attempt_set_id: String,
    /// Lane every other lane is read against. Defaults to the first lane, which
    /// is what the scoreboard shows before the user re-bases.
    #[serde(default)]
    pub baseline: usize,
}

/// GET /api/sessions/{id}/variants/trajectories?attempt_set_id=…&baseline=…
/// — the attempts' routes, normalised and aligned, with each row marked agreed
/// or diverged.
///
/// Answers the question a scoreboard cannot: when two candidates both pass, how
/// did they get there, and where exactly did they part.
pub async fn session_variants_trajectories(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TrajectoryQuery>,
) -> Result<Json<axocoatl_daemon::trajectory::Alignment>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .variants_trajectories(&id, &q.attempt_set_id, q.baseline)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct VerifyBody {
    pub attempt_set_id: String,
    /// The project's own check command — tests, build, typecheck. Run through
    /// `sh` inside each lane's worktree.
    pub check: String,
}

/// POST /api/sessions/{id}/variants/verify — run Checks for every attempt and
/// report which survive, so a reviewer does not have to read known failures.
pub async fn session_variants_verify(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VerifyBody>,
) -> Result<Json<Vec<axocoatl_daemon::git::LaneVerdict>>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let daemon = state.read().await;
    daemon
        .verify_variants(&id, &body.attempt_set_id, &body.check)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct JudgeBody {
    pub attempt_set_id: String,
    /// Configured Agent to judge with. Its provider, model, system prompt,
    /// sampling controls, fallback policy, and per-run budget all apply.
    pub agent_id: String,
}

/// POST /api/sessions/{id}/variants/judge — rank the surviving attempts and say
/// why, so a reviewer reads one diff with a reason instead of N without.
pub async fn session_variants_judge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<JudgeBody>,
) -> Result<Json<axocoatl_daemon::git::Judgment>, WaysControlRouteError> {
    require_ready_session(&state, &id)
        .await
        .map_err(ways_readiness_err)?;
    let daemon = state.read().await;
    daemon
        .judge_variants(&id, &body.attempt_set_id, &body.agent_id)
        .await
        .map(Json)
        .map_err(ways_control_err)
}

#[derive(serde::Deserialize)]
pub struct ProbeQuery {
    pub provider: String,
    /// Comma-separated model ids — the lane list, checked in one call.
    pub models: String,
}

/// GET /api/variants/probe?provider=…&models=a,b,c — can these models drive a
/// lane? Answered in seconds, so a model that cannot is caught before a run is
/// spent finding out.
pub async fn variants_probe(
    State(state): State<AppState>,
    Query(q): Query<ProbeQuery>,
) -> Result<Json<Vec<axocoatl_daemon::git::ModelProbe>>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    let mut out = Vec::new();
    for model in q.models.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        out.push(
            daemon
                .probe_lane_model(&q.provider, model)
                .await
                .map_err(attempt_err)?,
        );
    }
    Ok(Json(out))
}

#[derive(serde::Deserialize)]
pub struct PlanBody {
    pub task: String,
    /// Configured Agent to plan with. The daemon executes it statelessly so the
    /// plan does not mutate that Agent's conversation.
    pub agent_id: String,
}

/// A plan plus the instruction it renders to — so the client fans out with
/// exactly the text the lanes will receive, rather than reassembling it.
#[derive(serde::Serialize)]
pub struct PlanResponse {
    #[serde(flatten)]
    pub plan: axocoatl_daemon::git::Plan,
    pub instruction: String,
    pub control_usage: axocoatl_daemon::git::ControlUsage,
}

/// POST /api/sessions/{id}/variants/plan — turn a task into a spec precise
/// enough for cheap models to execute. Returned for review before fanning out:
/// one plan corrected beats N executions of a bad one.
pub async fn session_variants_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PlanBody>,
) -> Result<Json<PlanResponse>, WaysControlRouteError> {
    require_ready_session(&state, &id)
        .await
        .map_err(ways_readiness_err)?;
    let daemon = state.read().await;
    let (plan, control_usage) = daemon
        .plan_task(&id, &body.task, &body.agent_id)
        .await
        .map_err(ways_control_err)?;
    let instruction = plan.render(&body.task);
    Ok(Json(PlanResponse {
        plan,
        instruction,
        control_usage,
    }))
}

#[derive(serde::Deserialize)]
pub struct CostQuery {
    pub attempt_set_id: String,
    /// Model to price the counterfactual against — the one expensive model you
    /// would otherwise have run the whole task on.
    pub baseline: String,
    /// Provider serving the explicitly selected baseline. This distinguishes a
    /// known-free Ollama model from an unpriced remote model with the same id.
    #[serde(default)]
    pub baseline_provider: Option<String>,
}

/// GET /api/sessions/{id}/variants/cost?attempt_set_id=…&baseline=… — what
/// these attempts cost, and what the same tokens would have cost on one
/// expensive model.
pub async fn session_variants_cost(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<CostQuery>,
) -> Result<Json<axocoatl_daemon::git::RunCost>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .variants_cost(
            &id,
            &q.attempt_set_id,
            &q.baseline,
            q.baseline_provider.as_deref(),
        )
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct AdoptBody {
    pub attempt_set_id: String,
    pub index: usize,
}

/// POST /api/sessions/{id}/variants/adopt — Keep one attempt's changes in the
/// session's primary workspace and tear down the rest.
pub async fn session_variant_adopt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AdoptBody>,
) -> Result<Json<axocoatl_daemon::git::GitStatus>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .adopt_variant(&id, &body.attempt_set_id, body.index)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct DiscardAttemptBody {
    pub attempt_set_id: String,
}

/// POST /api/sessions/{id}/variants/discard — Discard this attempt set without
/// keeping any of its changes.
pub async fn session_variants_discard(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DiscardAttemptBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .discard_attempt(&id, &body.attempt_set_id)
        .await
        .map(|_| Json(serde_json::json!({ "ok": true })))
        .map_err(attempt_err)
}

pub async fn execute_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, (StatusCode, Json<MeasuredErrorResponse>)> {
    require_ready_session(&state, &id)
        .await
        .map_err(|(status, error)| {
            measured_error_response(
                status,
                error.0.error,
                &axocoatl_core::TokenUsageStats::default(),
                true,
            )
        })?;
    let daemon = state.read().await;
    match daemon.execute_session_measured(&id, &body.input).await {
        Ok(measured) => Ok(Json(ExecuteResponse {
            usage: ExecutionUsageResponse::new(
                &measured.output.token_usage,
                measured.token_usage_known,
            ),
            output: measured.output.content,
        })),
        Err(failure) => Err(measured_daemon_failure_response(failure)),
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

/// POST /api/sessions/{id}/reopen — explicit Closed → Active transition.
pub async fn reopen_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .reopen_session(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
pub struct RenameSessionBody {
    pub name: String,
}

#[derive(serde::Deserialize)]
pub struct CheckBody {
    /// The project's check command. Null or empty clears it.
    #[serde(default)]
    pub check_command: Option<String>,
}

/// PUT /api/sessions/{id}/check — set the project's own check command.
pub async fn set_session_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CheckBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    let daemon = state.read().await;
    daemon
        .set_session_check(&id, body.check_command)
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureSessionEnvironmentBody {
    /// `null` selects Axocoatl's trusted default image.
    #[serde(default)]
    pub image: Option<String>,
    /// `null` plus `setup_reviewed: true` is an explicit no-setup decision.
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub setup_approved: bool,
    pub setup_reviewed: bool,
}

impl ConfigureSessionEnvironmentBody {
    fn validate(&self) -> Result<(), &'static str> {
        if !self.setup_reviewed {
            return Err("changing a runtime requires an explicit setup decision");
        }
        if self.setup_approved
            && self
                .setup_command
                .as_deref()
                .is_none_or(|command| command.trim().is_empty())
        {
            return Err("setup approval must name the exact non-empty command");
        }
        Ok(())
    }
}

/// PUT /api/sessions/{id}/environment — replace and synchronously prepare the
/// runtime contract. Runtime/setup failure is represented by the returned
/// Session's durable `environment.state = failed`, not hidden in daemon logs.
pub async fn configure_session_environment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConfigureSessionEnvironmentBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    if let Err(message) = body.validate() {
        return Err(err(StatusCode::BAD_REQUEST, message));
    }
    reject_unsupported_session_image(&state, None, body.image.as_deref()).await?;
    state
        .read()
        .await
        .configure_session_environment(
            &id,
            body.image,
            body.setup_command,
            body.setup_approved,
            true,
        )
        .await
        .map(Json)
        .map_err(attempt_err)
}

/// POST /api/sessions/{id}/environment/rebuild — reproduce the currently
/// approved plan. It never upgrades a proposal into approval.
pub async fn rebuild_session_environment(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .rebuild_session_environment(&id)
        .await
        .map(Json)
        .map_err(attempt_err)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmSessionRuntimeCleanupBody {
    #[serde(default)]
    pub runtime_id: Option<String>,
    #[serde(default)]
    pub creation_token: Option<String>,
    #[serde(default)]
    pub confirmed: bool,
    #[serde(default)]
    pub confirmed_all_matching_sandboxes_deleted: bool,
}

/// POST /api/sessions/{id}/environment/confirm-runtime-cleanup — record the
/// operator's exact, explicit assertion that either one retained runtime id,
/// or every sandbox bearing one retained creation metadata token, was deleted
/// outside Axocoatl.
pub async fn confirm_session_runtime_cleanup(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConfirmSessionRuntimeCleanupBody>,
) -> Result<Json<axocoatl_session::Session>, (StatusCode, Json<ErrorResponse>)> {
    state
        .read()
        .await
        .confirm_session_runtime_cleanup(
            &id,
            body.runtime_id.as_deref(),
            body.creation_token.as_deref(),
            body.confirmed,
            body.confirmed_all_matching_sandboxes_deleted,
        )
        .await
        .map(Json)
        .map_err(attempt_err)
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
// Retained API/storage backend for directoryless chats. The one app has no
// peer lightweight-Chat destination; see crates/axocoatl-memory/src/chat.rs
// for the distinct transcript model and compatibility rationale.

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
// Content-addressed FileStore compatibility API: list, search, preview,
// rename, and delete. The one app has no cross-chat Files destination.
// Deleting a file also cleans up chat references so callers do not see
// broken refs.

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
    let inline_type = safe_inline_media_type(&entry.mime);
    let content_type = inline_type.unwrap_or("application/octet-stream");
    let disposition = if inline_type.is_some() {
        "inline"
    } else {
        "attachment"
    };
    let filename = safe_download_name(&entry.name);
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("{disposition}; filename=\"{filename}\"")
            .parse()
            .unwrap(),
    );
    resp.headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    if content_type == "application/pdf" {
        resp.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            "sandbox; default-src 'none'".parse().unwrap(),
        );
    }
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

fn session_relations_retain_blob(
    references: &axocoatl_session::SessionAttachmentStore,
    blob_id: &str,
) -> bool {
    references
        .blob_ids_in_use()
        .iter()
        .any(|candidate| candidate.strip_prefix("sha256:").unwrap_or(candidate) == blob_id)
}

pub async fn delete_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // 1) Drop the file from FileStore. 2) Walk every chat and remove any
    // attachment ref pointing at this id (no orphan references left).
    let (file_store, chat_store, session_attachment_store) = {
        let daemon = state.read().await;
        (
            daemon.file_store.clone(),
            daemon.chat_store.clone(),
            daemon.session_attachment_store.clone(),
        )
    };
    {
        // Match the turn-context lock order so validation and deletion cannot
        // cross between checking a relation and removing its bytes.
        let mut files = file_store.lock().await;
        let references = session_attachment_store.lock().await;
        let session_owned = session_relations_retain_blob(&references, &id);
        if session_owned {
            return Err(err(
                StatusCode::CONFLICT,
                "file bytes are retained by active or historical Session context",
            ));
        }
        files.remove(&id).map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;
    }
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

/// Attach an already-uploaded FileStore entry through the retained chat API.
/// Body: `{ file_id: string }`.
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
    Ok(safe_attachment_response(bytes, &entry.mime, &entry.name))
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
    let mut resp = Response::new(axum::body::Body::from(body));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, mime.parse().unwrap());
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        export_content_disposition(&chat.name, ext),
    );
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
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
    State(state): State<AppState>,
    Query(q): Query<FsListQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let raw = q.path.unwrap_or_default();
    if raw.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "path is required"));
    }
    let dir = std::path::Path::new(&raw)
        .canonicalize()
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("{raw}: {e}")))?;

    // devcontainer.json — optional, well-formed only. Its postCreate command
    // takes precedence as the concrete proposal. The runtime capability tells
    // the browser whether daemon policy defaults that exact command to checked;
    // the per-Session reviewed decision remains authoritative.
    let loaded_devcontainer = axocoatl_session::DevContainer::load(&dir);
    let suggested_setup = match &loaded_devcontainer {
        Ok(Some((_path, dc))) if !dc.post_create_scripts().is_empty() => Some(serde_json::json!({
            "command": dc.post_create_scripts().join(" && "),
            "source": "devcontainer",
        })),
        _ => axocoatl_session::detect_setup_command(&dir).map(|command| {
            serde_json::json!({
                "command": command,
                "source": "package-lock",
            })
        }),
    };
    let devcontainer = match loaded_devcontainer {
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

    let runtime = session_runtime_capability(&state).await;

    Ok(Json(serde_json::json!({
        "devcontainer": devcontainer,
        "axocoatl_md": axo_files,
        "suggested_setup": suggested_setup,
        "runtime": runtime,
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

/// One directory level of a session's file tree (lazy-loaded).
pub async fn session_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SessionPathQuery>,
) -> Result<Json<Vec<TreeEntry>>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    let entries = state
        .read()
        .await
        .session_list_directory(&id, q.path.as_deref())
        .await
        .map_err(attempt_err)?
        .into_iter()
        .map(|entry| TreeEntry {
            name: entry.name,
            path: entry.path,
            kind: entry.kind,
            size: entry.size,
        })
        .collect();
    Ok(Json(entries))
}

/// Background tasks running in a session's sandbox container.
pub async fn session_tasks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    require_ready_session(&state, &id).await?;
    Ok(Json(
        state
            .read()
            .await
            .session_tasks(&id)
            .await
            .map_err(attempt_err)?,
    ))
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
    require_ready_session(&state, &id).await?;
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
            Err(e) => Err(attempt_err(e)),
        }
    } else {
        match state.read().await.spawn_session_task(&id, cmd).await {
            Ok(task_id) => Ok(Json(serde_json::json!({ "id": task_id, "kind": "task" }))),
            Err(e) => Err(attempt_err(e)),
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
    if let Err(error) = require_ready_session(&state, &session_id).await {
        return error.into_response();
    }
    let term = match state
        .read()
        .await
        .session_terminal(&session_id, &terminal_id)
        .await
    {
        Ok(Some(term)) => term,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such terminal").into_response(),
        Err(error) => return attempt_err(error).into_response(),
    };
    ws.on_upgrade(move |socket| handle_terminal_ws(socket, term))
}

const TERMINAL_RESYNC_CLOSE_CODE: u16 = 4001;

#[derive(Debug, PartialEq, Eq)]
enum TerminalOutputEvent {
    Chunk(Vec<u8>),
    Resync { missed: u64 },
    Closed,
}

fn terminal_output_event(
    output: Result<Vec<u8>, tokio::sync::broadcast::error::RecvError>,
) -> TerminalOutputEvent {
    use tokio::sync::broadcast::error::RecvError;
    match output {
        Ok(bytes) => TerminalOutputEvent::Chunk(bytes),
        Err(RecvError::Lagged(missed)) => TerminalOutputEvent::Resync { missed },
        Err(RecvError::Closed) => TerminalOutputEvent::Closed,
    }
}

fn terminal_resync_close_frame(missed: u64) -> axum::extract::ws::CloseFrame {
    axum::extract::ws::CloseFrame {
        code: TERMINAL_RESYNC_CLOSE_CODE,
        reason: format!("terminal-resync-required: missed {missed} output chunks").into(),
    }
}

async fn handle_terminal_ws(
    mut socket: axum::extract::ws::WebSocket,
    term: std::sync::Arc<axocoatl_isolation::pty::PtyTerminal>,
) {
    use axum::extract::ws::Message;

    // Subscribe and snapshot under the PTY pump's append/publish gate. Bytes
    // before this cut appear only in the snapshot; bytes after it are queued
    // only in `output_rx` while the snapshot crosses the network.
    let (snapshot, mut output_rx) = term.attach_output();
    if !snapshot.is_empty() && socket.send(Message::Binary(snapshot.into())).await.is_err() {
        return;
    }

    let input_tx = term.input_tx.clone();
    let term_for_resize = term.clone();

    loop {
        tokio::select! {
            // PTY → WS
            chunk = output_rx.recv() => match terminal_output_event(chunk) {
                TerminalOutputEvent::Chunk(bytes) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                TerminalOutputEvent::Resync { missed } => {
                    // Continuing would make the visible terminal permanently
                    // incomplete. Close with an application reason so the
                    // client clears xterm and reattaches to a fresh exact cut.
                    let _ = socket
                        .send(Message::Close(Some(terminal_resync_close_frame(missed))))
                        .await;
                    break;
                }
                TerminalOutputEvent::Closed => break,
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
    require_ready_session(&state, &id).await?;
    let path = q
        .path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "path is required"))?;
    let file = state
        .read()
        .await
        .session_read_file(&id, path)
        .await
        .map_err(attempt_err)?;
    let lang = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    Ok(Json(FileResponse {
        path: q.path.unwrap_or_default(),
        content: file.content,
        lang,
        truncated: file.truncated,
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
    require_ready_session(&state, &id).await?;
    let path = q
        .path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "path is required"))?;
    let bytes = state
        .read()
        .await
        .session_write_file(&id, path, &body.content)
        .await
        .map_err(attempt_err)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "bytes": bytes,
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
    pub last_error: Option<String>,
    pub run_count: u64,
}

pub async fn list_proactive(State(state): State<AppState>) -> Json<Vec<ProactiveEntry>> {
    use axocoatl_config::{AutomationNodeKind, AutomationTrigger};
    use std::collections::HashMap;

    let (automations, schedule_table, proactive_table) = {
        let daemon = state.read().await;
        (
            daemon.list_automations().await,
            daemon.schedule_table.clone(),
            daemon.proactive_table.clone(),
        )
    };
    let schedule_observations: HashMap<String, axocoatl_daemon::ScheduleState> = schedule_table
        .lock()
        .map(|rows| {
            rows.iter()
                .cloned()
                .map(|row| (row.automation_id.clone(), row))
                .collect()
        })
        .unwrap_or_default();
    let proactive_observations: HashMap<String, axocoatl_daemon::ProactiveState> = proactive_table
        .lock()
        .map(|rows| {
            rows.iter()
                .cloned()
                .map(|row| (row.automation_id.clone(), row))
                .collect()
        })
        .unwrap_or_default();

    let mut entries: Vec<ProactiveEntry> = automations
        .into_iter()
        .filter_map(|automation| {
            let (trigger_kind, trigger_detail, input, schedule_observation, proactive_observation) =
                match &automation.trigger {
                    AutomationTrigger::Schedule { every, input }
                        if automation.id.starts_with("pro:") =>
                    {
                        (
                            "schedule".to_string(),
                            every.clone(),
                            input.clone().unwrap_or_default(),
                            schedule_observations.get(&automation.id),
                            None,
                        )
                    }
                    AutomationTrigger::OnEvent { event, input } => (
                        "event".to_string(),
                        event.clone(),
                        input.clone().unwrap_or_default(),
                        None,
                        proactive_observations.get(&automation.id),
                    ),
                    AutomationTrigger::OnSkill { skill_id } => (
                        "skill".to_string(),
                        skill_id.clone(),
                        String::new(),
                        None,
                        proactive_observations.get(&automation.id),
                    ),
                    _ => return None,
                };
            let agent = automation
                .nodes
                .iter()
                .find_map(|node| match &node.kind {
                    AutomationNodeKind::Agent { agent_id, .. } => Some(agent_id.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            Some(ProactiveEntry {
                id: automation
                    .id
                    .strip_prefix("pro:")
                    .unwrap_or(&automation.id)
                    .to_string(),
                name: automation.name,
                agent,
                trigger_kind,
                trigger_detail,
                input,
                enabled: automation.enabled,
                last_fired_unix: schedule_observation
                    .and_then(|row| row.last_fired_unix)
                    .or_else(|| proactive_observation.and_then(|row| row.last_fired_unix)),
                last_outcome: schedule_observation
                    .and_then(|row| row.last_outcome.clone())
                    .or_else(|| proactive_observation.and_then(|row| row.last_outcome.clone())),
                last_error: schedule_observation
                    .and_then(|row| row.last_error.clone())
                    .or_else(|| proactive_observation.and_then(|row| row.last_error.clone())),
                run_count: schedule_observation
                    .map(|row| row.run_count)
                    .or_else(|| proactive_observation.map(|row| row.run_count))
                    .unwrap_or(0),
            })
        })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
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

// --- Recent lattice events (retained integration/event-history API) ---

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
    pub every: Option<String>,
    pub input: Option<String>,
}

#[derive(Serialize)]
pub struct ScheduleActionResponse {
    pub schedule_id: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Serialize)]
pub struct ScheduleRunResponse {
    pub schedule_id: String,
    pub ok: bool,
    pub message: String,
    #[serde(flatten)]
    pub usage: ExecutionUsageResponse,
}

pub async fn patch_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SchedulePatch>,
) -> Result<Json<ScheduleActionResponse>, (StatusCode, Json<ErrorResponse>)> {
    use axocoatl_config::AutomationTrigger;

    let store = state.read().await.automation_store.clone();
    let mut store = store.write().await;
    let candidates = [id.clone(), format!("sched:{id}"), format!("pro:{id}")];
    let mut automation = candidates
        .iter()
        .find_map(|candidate| {
            store.get(candidate).filter(|automation| {
                matches!(&automation.trigger, AutomationTrigger::Schedule { .. })
            })
        })
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("schedule '{id}' not found")))?;
    if let Some(enabled) = body.enabled {
        automation.enabled = enabled;
    }
    if let AutomationTrigger::Schedule { every, input } = &mut automation.trigger {
        if let Some(updated_every) = body.every {
            let seconds = axocoatl_daemon::parse_interval(&updated_every)
                .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
            if seconds == 0 {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "schedule interval must be greater than zero",
                ));
            }
            *every = updated_every;
        }
        if let Some(updated_input) = body.input {
            *input = if updated_input.trim().is_empty() {
                None
            } else {
                Some(updated_input)
            };
        }
    }
    let automation = store
        .upsert(automation)
        .map_err(|error| err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Json(ScheduleActionResponse {
        schedule_id: id,
        ok: true,
        message: format!("enabled={}", automation.enabled),
    }))
}

pub async fn run_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ScheduleRunResponse>, (StatusCode, Json<MeasuredErrorResponse>)> {
    use axocoatl_config::AutomationTrigger;

    let context = {
        let daemon = state.read().await;
        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon)
    };
    let candidates = [id.clone(), format!("sched:{id}"), format!("pro:{id}")];
    let mut automation = None;
    for candidate in &candidates {
        if let Some(found) = context.get_automation(candidate).await {
            if matches!(&found.trigger, AutomationTrigger::Schedule { .. }) {
                automation = Some(found);
                break;
            }
        }
    }
    let automation = automation.ok_or_else(|| {
        measured_error_response(
            StatusCode::NOT_FOUND,
            format!("schedule '{id}' not found"),
            &axocoatl_core::TokenUsageStats::default(),
            true,
        )
    })?;
    let input = match &automation.trigger {
        AutomationTrigger::Schedule { input, .. } => input.clone().unwrap_or_default(),
        _ => unreachable!(),
    };
    let result = axocoatl_daemon::automation_executor::execute_automation_in_context(
        &context,
        &automation,
        &input,
    )
    .await;
    axocoatl_daemon::record_automation_outcome(&context, &automation, &result);
    match result {
        Ok(out) => {
            if let Some(error) = out.terminal_error() {
                return Err(workflow_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                ));
            }
            let usage = ExecutionUsageResponse::new(&out.total_token_usage, out.token_usage_known);
            Ok(Json(ScheduleRunResponse {
                schedule_id: id,
                ok: true,
                message: format!(
                    "ran Automation '{}' · {} agents · {}{} tokens{}",
                    automation.id,
                    out.completed_agents.len(),
                    if usage.token_usage_known { "" } else { "≥" },
                    usage.total_tokens,
                    if usage.token_usage_known {
                        ""
                    } else {
                        " known subtotal"
                    }
                ),
                usage,
            }))
        }
        Err(error) => Err(workflow_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            error,
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
// One bidirectional socket per app client — the live transport for session
// state, approvals, and agent output. It also retains workflow/chat commands
// and frames for compatibility clients after those peer browser surfaces
// retired.

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
    /// Cooperatively stop the active ChatTurn at its safe execution boundary.
    ChatStop {
        chat_id: String,
    },
    Session {
        id: String,
        input: String,
        /// User-visible composer text. `input` may additionally contain
        /// structured workspace/browser context used only for execution.
        #[serde(default)]
        display_input: Option<String>,
        /// Immutable snapshots for editor/Browser references. Uploaded files
        /// use `reference_ids` so their bytes remain server-authoritative.
        #[serde(default)]
        context_references: Vec<axocoatl_session::SessionTurnContextReference>,
        /// Stable browser-generated identity for durable retry and exact Stop.
        #[serde(default)]
        turn_id: Option<String>,
        /// Stable send-attempt identity. Defaults to `turn_id` when omitted.
        #[serde(default)]
        idempotency_key: Option<String>,
        /// Session-owned immutable context relations accepted with this turn.
        #[serde(default)]
        reference_ids: Vec<String>,
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
    /// Cooperatively stop exactly one active Session turn. The daemon rejects
    /// a stale turn id instead of stopping whichever turn is currently shown.
    SessionStop {
        id: String,
        turn_id: String,
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

/// Build one agent turn from the persisted chat transcript.
///
/// `ChatStore` owns this conversation, so every stored message is supplied as
/// authoritative history rather than allowing the configured agent actor's
/// lifetime session to replace it. The current user content stays separate:
/// the actor appends it exactly once when it executes the turn.
fn build_chat_agent_input(
    chat: &axocoatl_memory::chat::Chat,
    content: &str,
    attachments: Vec<axocoatl_core::AgentAttachment>,
) -> axocoatl_core::AgentInput {
    let history = chat
        .messages
        .iter()
        .map(|message| axocoatl_core::ChatMessage {
            role: message.role.clone(),
            content: axocoatl_core::MessageContent::Text(message.content.clone()),
            name: message.name.clone(),
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| axocoatl_core::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: serde_json::from_str(&call.arguments_json)
                        .unwrap_or(serde_json::Value::Null),
                    provider_metadata: call.provider_metadata.clone(),
                })
                .collect(),
            tool_call_id: message.tool_call_id.clone(),
        })
        .collect();

    axocoatl_core::AgentInput::text(content)
        .with_supplied_history(history)
        .with_system_override(chat.system_override.clone())
        .with_model_override(chat.model_override.clone())
        .with_attachments(attachments)
}

/// Attribute tool evidence to the logical child that produced it while
/// preserving the existing outer-agent label for standalone executions.
fn stream_evidence_agent(source_agent: Option<String>, outer_agent: &str) -> String {
    source_agent.unwrap_or_else(|| outer_agent.to_string())
}

/// Keep only live envelopes that were published after the reconnect snapshot.
/// Frames at or below the cursor are already folded into that snapshot.
fn stream_frame_after_cursor(
    cursor: u64,
    envelope: axocoatl_daemon::SequencedStreamFrame,
) -> Option<axocoatl_daemon::StreamFrame> {
    (envelope.sequence > cursor).then_some(envelope.frame)
}

fn workflow_error_frame(
    workflow: String,
    error: axocoatl_daemon::DaemonError,
) -> axocoatl_daemon::StreamFrame {
    let (usage, token_usage_known) = error
        .workflow_token_usage()
        .map(|(usage, known)| (usage.clone(), known))
        .unwrap_or_else(|| (axocoatl_core::TokenUsageStats::default(), true));
    axocoatl_daemon::StreamFrame::WorkflowError {
        workflow,
        error: error.to_string(),
        input_tokens: usage.input_tokens as u64,
        output_tokens: usage.output_tokens as u64,
        reasoning_tokens: usage.reasoning_tokens.unwrap_or(0) as u64,
        token_usage_known,
    }
}

fn chat_terminal_message(
    kind: &str,
    chat_id: &str,
    turn_id: &str,
    usage: &axocoatl_core::TokenUsageStats,
    token_usage_known: bool,
    error: Option<&str>,
) -> String {
    let mut message = serde_json::json!({
        "kind": kind,
        "chat_id": chat_id,
        "turn_id": turn_id,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "reasoning_tokens": usage.reasoning_tokens.unwrap_or(0),
        "total_tokens": usage.total(),
        "token_usage_known": token_usage_known,
    });
    if let Some(error) = error {
        message["error"] = serde_json::Value::String(error.to_string());
    }
    message.to_string()
}

fn clear_chat_control_if_owner(
    active: &mut std::collections::HashMap<String, axocoatl_actor::AgentRunControl>,
    chat_id: &str,
    expected: &axocoatl_actor::AgentRunControl,
) -> bool {
    if active
        .get(chat_id)
        .is_some_and(|active| active.id() == expected.id())
    {
        active.remove(chat_id);
        true
    } else {
        false
    }
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

    // Snapshot of in-flight runs and every parked MCP approval — lets a client
    // that reloaded mid-run restore both observation and decisions instead of
    // depending on one-shot frames it may have missed.
    let (snapshot_cursor, runs, environment_transitions, attempt_ownerships, approval_gate) = {
        let daemon = state.read().await;
        let (cursor, runs, environment_transitions, attempt_ownerships) =
            daemon.live_run_snapshot_with_cursor().await;
        (
            cursor,
            runs,
            environment_transitions,
            attempt_ownerships,
            daemon.mcp_approval_gate.clone(),
        )
    };
    let approvals = approval_gate
        .pending_contexts()
        .await
        .into_iter()
        .map(axocoatl_daemon::stream::PendingMcpApproval::from)
        .collect();
    let snapshot = axocoatl_daemon::StreamFrame::Snapshot {
        runs,
        approvals,
        environment_transitions,
        attempt_ownerships,
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
            frame = bus_rx.recv_sequenced() => {
                match frame {
                    Ok(envelope) => {
                        let Some(f) = stream_frame_after_cursor(snapshot_cursor, envelope) else {
                            continue;
                        };
                        if let Ok(j) = serde_json::to_string(&f) {
                            if socket.send(Message::Text(j.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    // A lagged socket may have missed Accepted or terminal.
                    // Disconnect so the browser's normal reconnect receives an
                    // authoritative Snapshot instead of staying silently stale.
                    Err(RecvError::Lagged(_)) => break,
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
                let (context, bus) = {
                    let daemon = state.read().await;
                    (
                        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(
                            &daemon,
                        ),
                        daemon.stream_bus.clone(),
                    )
                };
                let result = match context.get_automation(&id).await {
                    Some(automation)
                        if matches!(
                            &automation.trigger,
                            axocoatl_config::AutomationTrigger::Manual
                        ) =>
                    {
                        let result =
                            axocoatl_daemon::automation_executor::execute_automation_in_context(
                                &context,
                                &automation,
                                &input,
                            )
                            .await;
                        axocoatl_daemon::record_automation_outcome(&context, &automation, &result);
                        result
                    }
                    _ => Err(axocoatl_daemon::DaemonError::WorkflowNotFound(id.clone())),
                };
                let frame = match result {
                    Ok(output) => match output.terminal_error() {
                        Some(error) => workflow_error_frame(output.workflow_id, error),
                        None => axocoatl_daemon::StreamFrame::WorkflowDone {
                            workflow: output.workflow_id,
                            output: output.final_content,
                            completed: output.completed_agents,
                            tokens: output.total_token_usage.total() as u64,
                            token_usage_known: output.token_usage_known,
                        },
                    },
                    Err(error) => workflow_error_frame(id, error),
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
                let control = axocoatl_actor::AgentRunControl::new(
                    axocoatl_actor::AgentRunId::new(format!("chat-{}", uuid::Uuid::new_v4())),
                );
                let turn_id = control.id().to_string();

                // Load the chat.
                let chat = {
                    let daemon = state.read().await;
                    daemon.get_chat(&chat_id).await
                };
                let chat = match chat {
                    Some(c) => c,
                    None => {
                        let error = format!("chat {chat_id} not found");
                        let _ = out.send(chat_terminal_message(
                            "chat-error",
                            &chat_id,
                            &turn_id,
                            &axocoatl_core::TokenUsageStats::default(),
                            true,
                            Some(&error),
                        ));
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
                        let error = format!("agent '{}' not found", chat.agent_id);
                        let _ = out.send(chat_terminal_message(
                            "chat-error",
                            &chat_id,
                            &turn_id,
                            &axocoatl_core::TokenUsageStats::default(),
                            true,
                            Some(&error),
                        ));
                        return;
                    }
                };

                // Make this exact run the active owner before doing any
                // attachment work. Replacing a prior turn requests its
                // cooperative stop; only the task holding the same run id may
                // later clear this slot.
                let previous = {
                    let active = state.read().await.active_chat_turns.clone();
                    let mut active = active.lock().await;
                    active.insert(chat_id.clone(), control.clone())
                };
                if let Some(previous) = previous {
                    previous.cancel();
                }

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
                        "turn_id": turn_id,
                        "agent": chat.agent_id,
                    })
                    .to_string(),
                );
                // …and on the shared bus, keyed by chat id. Chat used to stream
                // only down the socket that asked for it, which meant a reload
                // lost the reply, a second tab saw nothing, and a chat could
                // never appear beside other runs. Publishing here makes a chat a
                // run like any other, which is what a unified surface needs.
                let chat_bus = { state.read().await.stream_bus.clone() };
                let _ = chat_bus.send(axocoatl_daemon::StreamFrame::SessionStart {
                    session: chat_id.clone(),
                    turn_id: Some(turn_id.clone()),
                });

                // Token sink → chat-* frames. Also accumulates the text so we
                // can persist the partial assistant message on cancel.
                // Mirrors of the bus handle + agent id for the sink task, which
                // republishes each delta so the chat is visible to any client,
                // not just the socket that started it.
                let bus_for_chat = chat_bus.clone();
                let agent_for_chat = chat.agent_id.clone();
                let accumulated = Arc::new(tokio::sync::Mutex::new(String::new()));
                let (sink_tx, mut sink_rx) =
                    tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
                let sink_forwarder = {
                    let out = out.clone();
                    let chat_id = chat_id.clone();
                    let turn_id = turn_id.clone();
                    let accumulated = accumulated.clone();
                    tokio::spawn(async move {
                        while let Some(chunk) = sink_rx.recv().await {
                            let f = match chunk {
                                axocoatl_actor::AgentStreamChunk::Text(d) => {
                                    accumulated.lock().await.push_str(&d);
                                    let _ =
                                        bus_for_chat.send(axocoatl_daemon::StreamFrame::Token {
                                            workflow: chat_id.clone(),
                                            agent: agent_for_chat.clone(),
                                            turn_id: Some(turn_id.clone()),
                                            delta: d.clone(),
                                        });
                                    serde_json::json!({
                                        "kind": "chat-token", "chat_id": chat_id,
                                        "turn_id": turn_id, "delta": d,
                                    })
                                }
                                axocoatl_actor::AgentStreamChunk::Reasoning(d) => {
                                    let _ = bus_for_chat.send(
                                        axocoatl_daemon::StreamFrame::Reasoning {
                                            workflow: chat_id.clone(),
                                            agent: agent_for_chat.clone(),
                                            turn_id: Some(turn_id.clone()),
                                            delta: d.clone(),
                                        },
                                    );
                                    serde_json::json!({
                                        "kind": "chat-reasoning", "chat_id": chat_id,
                                        "turn_id": turn_id, "delta": d,
                                    })
                                }
                                axocoatl_actor::AgentStreamChunk::ToolCallStarted {
                                    source_agent,
                                    id,
                                    name,
                                    arguments,
                                    ..
                                } => serde_json::json!({
                                    "kind": "chat-tool-start", "chat_id": chat_id,
                                    "turn_id": turn_id,
                                    "agent": stream_evidence_agent(source_agent, &agent_for_chat),
                                    "call_id": id, "name": name, "arguments": arguments,
                                }),
                                axocoatl_actor::AgentStreamChunk::ToolCallResult {
                                    source_agent,
                                    id,
                                    name,
                                    result,
                                    is_error,
                                } => serde_json::json!({
                                    "kind": "chat-tool-result", "chat_id": chat_id,
                                    "turn_id": turn_id,
                                    "agent": stream_evidence_agent(source_agent, &agent_for_chat),
                                    "call_id": id, "name": name,
                                    "result": result, "is_error": is_error,
                                }),
                            };
                            let _ = out.send(f.to_string());
                        }
                    })
                };

                // Resolve FileStore entries to AgentAttachments while the
                // retained store capability is locked. The executor never
                // reopens an ambient control-plane path after this boundary.
                // Extracted text (PDF/CSV/OCR) is carried alongside the exact
                // authenticated blob bytes.
                let core_attachments: Vec<axocoatl_core::AgentAttachment> = {
                    let daemon = state.read().await;
                    let fs = daemon.file_store.lock().await;
                    attachments_for_turn
                        .iter()
                        .filter_map(|e| {
                            let bytes = fs.read_bytes(&e.id).ok()?;
                            Some(axocoatl_core::AgentAttachment {
                                id: e.id.clone(),
                                name: e.name.clone(),
                                mime: e.mime.clone(),
                                bytes,
                                size: e.size,
                                extracted_text: e
                                    .extracted_text
                                    .clone()
                                    .or_else(|| e.ocr_text.clone()),
                            })
                        })
                        .collect()
                };
                let agent_input = build_chat_agent_input(&chat, &content, core_attachments);

                // Cancellation reaches the behavior itself. Await the safe
                // outcome, then drain every queued delta before publishing a
                // terminal frame so no token can arrive after done/stopped/error.
                let outcome = axocoatl_actor::execute_agent_streaming_controlled_measured(
                    &actor,
                    agent_input,
                    sink_tx,
                    control.clone(),
                )
                .await;
                let _ = sink_forwarder.await;

                // A newer turn may already own this Chat. Never let the older
                // completion erase that newer run's Stop handle.
                {
                    let daemon = state.read().await;
                    let mut active = daemon.active_chat_turns.lock().await;
                    clear_chat_control_if_owner(&mut active, &chat_id, &control);
                }

                match outcome {
                    Ok(measured) => match measured.outcome {
                        axocoatl_actor::AgentRunOutcome::Completed(mut output) => {
                            output.token_usage = measured.token_usage.usage;
                            // Persist the assistant reply.
                            let daemon = state.read().await;
                            let store = daemon.chat_store.clone();
                            let final_text = if !output.content.is_empty() {
                                output.content.clone()
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
                                    token_count: output.token_usage.output_tokens,
                                    name: None,
                                    tool_calls: Vec::new(),
                                    tool_call_id: None,
                                },
                            );
                            let _ = out.send(chat_terminal_message(
                                "chat-done",
                                &chat_id,
                                &turn_id,
                                &output.token_usage,
                                measured.token_usage.complete,
                                None,
                            ));
                            let _ = chat_bus.send(axocoatl_daemon::StreamFrame::SessionDone {
                                session: chat_id.clone(),
                                turn_id: Some(turn_id.clone()),
                                input_tokens: output.token_usage.input_tokens as u64,
                                output_tokens: output.token_usage.output_tokens as u64,
                                reasoning_tokens: output.token_usage.reasoning_tokens.unwrap_or(0)
                                    as u64,
                                token_usage_known: measured.token_usage.complete,
                            });
                        }
                        axocoatl_actor::AgentRunOutcome::Cancelled {
                            mut partial_output, ..
                        } => {
                            partial_output.token_usage = measured.token_usage.usage;
                            let partial = if partial_output.content.is_empty() {
                                accumulated.lock().await.clone()
                            } else {
                                partial_output.content.clone()
                            };
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
                                        token_count: partial_output.token_usage.output_tokens,
                                        name: None,
                                        tool_calls: Vec::new(),
                                        tool_call_id: None,
                                    },
                                );
                            }
                            let _ = out.send(chat_terminal_message(
                                "chat-stopped",
                                &chat_id,
                                &turn_id,
                                &partial_output.token_usage,
                                measured.token_usage.complete,
                                None,
                            ));
                            let _ = chat_bus.send(axocoatl_daemon::StreamFrame::SessionCancelled {
                                session: chat_id.clone(),
                                turn_id: turn_id.clone(),
                                input_tokens: partial_output.token_usage.input_tokens as u64,
                                output_tokens: partial_output.token_usage.output_tokens as u64,
                                reasoning_tokens: partial_output
                                    .token_usage
                                    .reasoning_tokens
                                    .unwrap_or(0)
                                    as u64,
                                token_usage_known: measured.token_usage.complete,
                            });
                        }
                    },
                    Err(failure) => {
                        let error = failure.to_string();
                        let usage = failure.token_usage;
                        let _ = out.send(chat_terminal_message(
                            "chat-error",
                            &chat_id,
                            &turn_id,
                            &usage.usage,
                            usage.complete,
                            Some(&error),
                        ));
                        let _ = chat_bus.send(axocoatl_daemon::StreamFrame::SessionError {
                            session: chat_id.clone(),
                            turn_id: Some(turn_id.clone()),
                            error,
                            input_tokens: usage.usage.input_tokens as u64,
                            output_tokens: usage.usage.output_tokens as u64,
                            reasoning_tokens: usage.usage.reasoning_tokens.unwrap_or(0) as u64,
                            token_usage_known: usage.complete,
                        });
                    }
                }
            });
        }

        WsCommand::ChatStop { chat_id } => {
            let active = {
                let daemon = state.read().await;
                daemon.active_chat_turns.clone()
            };
            let control = active.lock().await.get(&chat_id).cloned();
            if let Some(control) = control {
                control.cancel();
            }
        }

        // Session — stream the agent's work (tokens, reasoning, tool calls)
        // onto the bus, so the cockpit + lattice panel see it and the run is
        // reconnectable.
        WsCommand::Session {
            id,
            input,
            display_input,
            context_references,
            turn_id,
            idempotency_key,
            reference_ids,
            model_override,
            target_agent,
        } => {
            let state = state.clone();
            tokio::spawn(async move {
                let turn_id = turn_id.unwrap_or_else(|| format!("turn-{}", uuid::Uuid::new_v4()));
                let idempotency_key = idempotency_key.or_else(|| Some(turn_id.clone()));

                // The daemon is the sole publisher of Session stream frames.
                // This compatibility sink is deliberately disconnected so
                // the route cannot publish a second copy of each chunk.
                let (sink_tx, sink_rx) =
                    tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
                drop(sink_rx);

                // The daemon publishes Accepted/Start and the matching
                // terminal frame in the same atomic lifecycle boundary used
                // by reconnect snapshots. The route must not publish another
                // terminal copy.
                let _ = {
                    let daemon = state.read().await;
                    daemon
                        .execute_session_turn_streaming(
                            &id,
                            &turn_id,
                            idempotency_key,
                            display_input.as_deref(),
                            &input,
                            reference_ids,
                            context_references,
                            model_override,
                            target_agent,
                            sink_tx,
                        )
                        .await
                };
            });
        }

        WsCommand::SessionStop { id, turn_id } => {
            let result = state.read().await.stop_session_turn(&id, &turn_id).await;
            if let Err(error) = result {
                let bus = { state.read().await.stream_bus.clone() };
                let _ = bus.send(axocoatl_daemon::StreamFrame::SessionStopRejected {
                    session: id,
                    turn_id,
                    error: error.to_string(),
                });
            }
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

/// Start a fresh whole-graph run using a prior run's inputs. The compatibility
/// path still says `fork`, but this is deliberately a rerun from the beginning,
/// not a checkpoint continuation. Its ancestry, trigger input, and TextInput
/// values are persisted before the endpoint reports success.
pub async fn fork_run(
    State(state): State<AppState>,
    Path((automation_id, run_id)): Path<(String, String)>,
    Json(body): Json<ForkRunBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let (source, context) = {
        let daemon = state.read().await;
        let source = daemon
            .get_run(&automation_id, &run_id)
            .map_err(|e| err(StatusCode::NOT_FOUND, e.to_string()))?;
        let context =
            axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon);
        (source, context)
    };
    let automation = context
        .get_automation(&automation_id)
        .await
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                format!("automation '{automation_id}' not found"),
            )
        })?;
    let input = body.input.unwrap_or(source.trigger_input);
    let text_inputs = source.text_inputs;
    let forked_from = axocoatl_daemon::automation_runs::ForkSource {
        source_run_id: run_id.clone(),
        from_start: true,
        from_step: 0,
    };
    let new_run_id = axocoatl_daemon::automation_executor::start_automation_run_in_context(
        &context,
        &automation,
        &input,
        &text_inputs,
        Some(forked_from),
    )
    .await
    .map_err(|error| err(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;

    let spawned_run_id = new_run_id.clone();
    tokio::spawn(async move {
        let result =
            axocoatl_daemon::automation_executor::execute_started_automation_run_in_context(
                &context,
                &automation,
                &input,
                &text_inputs,
                &spawned_run_id,
            )
            .await;
        axocoatl_daemon::record_automation_outcome(&context, &automation, &result);
        if let Err(e) = result {
            tracing::warn!("forked run failed: {e}");
        }
    });
    Ok(Json(serde_json::json!({
        "ok": true,
        "run_id": new_run_id,
        "forked_from": run_id,
        "from_start": true,
    })))
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

fn interrupt_resolution_err(
    error: axocoatl_daemon::interrupt::InterruptResolutionError,
) -> (StatusCode, Json<ErrorResponse>) {
    let status = match &error {
        axocoatl_daemon::interrupt::InterruptResolutionError::NotFound(_) => StatusCode::NOT_FOUND,
        axocoatl_daemon::interrupt::InterruptResolutionError::Recovery { .. } => {
            StatusCode::CONFLICT
        }
    };
    err(status, error.to_string())
}

/// Resume a parked interrupt by `{automation_id}:{run_id}:{node_id}`.
/// The executor wakes and the automation continues from there.
pub async fn resume_interrupt(
    State(state): State<AppState>,
    Path((automation_id, run_id, node_id)): Path<(String, String, String)>,
    Json(body): Json<ResumeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let key = format!("{automation_id}:{run_id}:{node_id}");
    let context = {
        let daemon = state.read().await;
        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon)
    };
    axocoatl_daemon::automation_executor::resolve_pending_interrupt(
        &context,
        &automation_id,
        &run_id,
        &node_id,
        body.value,
        false,
    )
    .await
    .map_err(interrupt_resolution_err)?;
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
    let context = {
        let daemon = state.read().await;
        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon)
    };
    axocoatl_daemon::automation_executor::resolve_pending_interrupt(
        &context,
        &automation_id,
        &run_id,
        &node_id,
        String::new(),
        true,
    )
    .await
    .map_err(interrupt_resolution_err)?;
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
// One concept = one endpoint. `AutomationStore` is authoritative; legacy
// workflow, schedule, and proactive YAML only seeds it on first boot.

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
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))
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
    // Fail synchronously for an unknown id. The background task re-reads the
    // record so an edit made between click and execution takes precedence.
    let context = {
        let daemon = state.read().await;
        axocoatl_daemon::automation_executor::AutomationExecutionContext::from_daemon(&daemon)
    };
    if context.get_automation(&id).await.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("automation '{id}' not found"),
        ));
    }
    let input = body.input.clone();
    let inputs = body.inputs.clone();
    let id_clone = id.clone();
    tokio::spawn(async move {
        let Some(automation) = context.get_automation(&id_clone).await else {
            return;
        };
        // The Automation is cloned before provider/tool execution, so the
        // canonical store lock is never held across a run.
        let result =
            axocoatl_daemon::automation_executor::execute_automation_with_inputs_in_context(
                &context,
                &automation,
                &input,
                &inputs,
            )
            .await;
        axocoatl_daemon::record_automation_outcome(&context, &automation, &result);
        if let Err(e) = result {
            tracing::warn!(automation = %id_clone, error = %e, "automation run failed");
        }
    });
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

// --- Browser-pane proxy (DOM-picker enabler) ---

fn session_preview_upstream(host_port: u16, tail: &str, query: Option<&str>) -> String {
    let query = query.map(|value| format!("?{value}")).unwrap_or_default();
    format!("http://127.0.0.1:{host_port}/{tail}{query}")
}

fn session_preview_websocket_upstream(host_port: u16, tail: &str, query: Option<&str>) -> String {
    let query = query.map(|value| format!("?{value}")).unwrap_or_default();
    format!("ws://127.0.0.1:{host_port}/{tail}{query}")
}

const LEGACY_PREVIEW_SANDBOX_CSP: &str =
    "sandbox allow-scripts allow-forms allow-popups allow-modals";
const PREVIEW_HOST_SUFFIX: &str = ".localhost";
const PREVIEW_TAP_PATH: &str = "/.axocoatl/preview-picker.js";
const MAX_PREVIEW_REQUEST_BODY: usize = 64 * 1024 * 1024;
const MAX_PREVIEW_HTML_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewHostTarget {
    session_id: String,
    logical_port: u16,
}

/// Parse `<session>-p<port>.localhost[:listener-port]`. Session ids are
/// generated as DNS-safe `ses-<uuid>` labels; accepting nothing else keeps the
/// Host boundary deterministic and prevents an invalid Preview host from ever
/// falling through to the workbench router.
fn preview_target_from_headers(headers: &HeaderMap) -> Result<Option<PreviewHostTarget>, ()> {
    let Some(raw_host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(None);
    };
    let authority = Authority::from_str(raw_host).map_err(|_| ())?;
    let hostname = authority.host().to_ascii_lowercase();
    let Some(label) = hostname.strip_suffix(PREVIEW_HOST_SUFFIX) else {
        return Ok(None);
    };
    let Some((session_id, port)) = label.rsplit_once("-p") else {
        return Err(());
    };
    if session_id.len() > 63
        || !session_id.starts_with("ses-")
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(());
    }
    let logical_port = port.parse::<u16>().map_err(|_| ())?;
    if logical_port == 0 {
        return Err(());
    }
    Ok(Some(PreviewHostTarget {
        session_id: session_id.to_string(),
        logical_port,
    }))
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && headers
            .get(header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn preview_request_has_disallowed_origin(
    method: &Method,
    is_websocket: bool,
    headers: &HeaderMap,
) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) && !is_websocket {
        return false;
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // A browser that omits Origin still advertises a cross-site request via
        // Fetch Metadata. Origin-less CLI requests remain useful for diagnosis.
        return headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| matches!(value, "cross-site" | "same-site"));
    };
    if origin == "null" {
        return true;
    }
    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return true;
    };
    let Some(origin_authority) = uri.authority() else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    !uri.scheme_str()
        .is_some_and(|scheme| matches!(scheme, "http" | "https"))
        || !origin_authority.as_str().eq_ignore_ascii_case(host)
}

/// The virtual Preview host is an origin boundary, not an alias for the
/// Axocoatl router. Every path (including `/`, `/api/*`, `/ws`, and static
/// paths) maps to exactly one Session/port upstream. Invalid Preview hosts are
/// rejected and requests from non-loopback peers never enter the boundary.
pub async fn preview_host_boundary(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let target = match preview_target_from_headers(request.headers()) {
        Ok(None) => return next.run(request).await,
        Ok(Some(target)) => target,
        Err(()) => return StatusCode::MISDIRECTED_REQUEST.into_response(),
    };

    if request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| !peer.0.ip().is_loopback())
    {
        return StatusCode::FORBIDDEN.into_response();
    }

    let websocket = is_websocket_upgrade(request.headers());
    if preview_request_has_disallowed_origin(request.method(), websocket, request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }

    if request.uri().path() == PREVIEW_TAP_PATH {
        if !matches!(*request.method(), Method::GET | Method::HEAD) {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let mut response = axo_tap_script().await;
        response.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if request.method() == Method::HEAD {
            *response.body_mut() = axum::body::Body::empty();
        }
        return response;
    }

    if websocket {
        preview_websocket_proxy(state, target, request).await
    } else {
        let tail = request.uri().path().trim_start_matches('/').to_string();
        proxy_preview_http(state, target, tail, request, PreviewProxyMode::VirtualHost).await
    }
}

#[derive(Debug, Clone)]
enum PreviewProxyMode {
    VirtualHost,
    LegacyOpaque { base: String },
}

fn request_header_is_forwardable(name: &axum::http::HeaderName, mode: &PreviewProxyMode) -> bool {
    let transport_header = matches!(
        name.as_str(),
        "host"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "content-length"
            | "accept-encoding"
            | "proxy-authorization"
    );
    if transport_header {
        return false;
    }
    // A validated Preview Host is the application origin, so application
    // Authorization/x-api-key headers belong to that upstream. The legacy
    // same-workbench path strips them to avoid leaking Axocoatl credentials.
    !matches!(
        (mode, name.as_str()),
        (
            PreviewProxyMode::LegacyOpaque { .. },
            "authorization" | "x-api-key"
        )
    )
}

fn response_header_is_forwardable(name: &axum::http::HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "content-length"
            | "content-security-policy"
            | "content-security-policy-report-only"
            | "x-frame-options"
    )
}

fn preview_http_upstream_host(
    mode: &PreviewProxyMode,
    headers: &HeaderMap,
    logical_port: u16,
) -> Option<HeaderValue> {
    match mode {
        PreviewProxyMode::VirtualHost => headers.get(header::HOST).cloned(),
        PreviewProxyMode::LegacyOpaque { .. } => {
            HeaderValue::from_str(&format!("localhost:{logical_port}")).ok()
        }
    }
}

fn preview_upstream_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    upstream: &str,
    request_headers: &HeaderMap,
    mode: &PreviewProxyMode,
    logical_port: u16,
) -> reqwest::RequestBuilder {
    let mut request = client.request(method, upstream);
    for (name, value) in request_headers {
        if request_header_is_forwardable(name, mode) {
            request = request.header(name, value);
        }
    }
    if let Some(host) = preview_http_upstream_host(mode, request_headers, logical_port) {
        // TCP still targets the resolved loopback transport. Keeping the
        // validated virtual authority lets app Origin↔Host checks and absolute
        // URL generation continue to point at the Preview boundary.
        request = request.header(header::HOST, host);
    }
    // The proxy injects the Pick bridge into HTML. Force an identity response
    // so those bytes are never mistaken for decoded content. Non-HTML
    // responses still preserve an upstream Content-Encoding if one is sent.
    request.header(header::ACCEPT_ENCODING, "identity")
}

fn preview_response_needs_html_injection(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/html"))
}

fn preview_response_allows_body(method: &Method, status: StatusCode) -> bool {
    *method != Method::HEAD
        && !status.is_informational()
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
}

fn preview_html_size_exceeds(current: usize, next: usize) -> bool {
    current.saturating_add(next) > MAX_PREVIEW_HTML_BODY
}

fn rewrite_preview_location(value: &HeaderValue) -> HeaderValue {
    let Ok(raw) = value.to_str() else {
        return value.clone();
    };
    let Ok(location) = reqwest::Url::parse(raw) else {
        return value.clone();
    };
    if !location
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0"))
    {
        return value.clone();
    }
    let mut relative = location.path().to_string();
    if let Some(query) = location.query() {
        relative.push('?');
        relative.push_str(query);
    }
    if let Some(fragment) = location.fragment() {
        relative.push('#');
        relative.push_str(fragment);
    }
    HeaderValue::from_str(&relative).unwrap_or_else(|_| value.clone())
}

fn inject_preview_bridge(mut html: String, mode: &PreviewProxyMode) -> String {
    if let PreviewProxyMode::LegacyOpaque { base } = mode {
        let base = format!(r#"<base href="{base}">"#);
        if let Some(index) = html.to_lowercase().find("<head>") {
            html.insert_str(index + "<head>".len(), &base);
        } else {
            html.insert_str(0, &base);
        }
    }
    let tap = match mode {
        PreviewProxyMode::VirtualHost => {
            format!(r#"<script src="{PREVIEW_TAP_PATH}"></script>"#)
        }
        PreviewProxyMode::LegacyOpaque { .. } => {
            r#"<script src="/axo-tap.js"></script>"#.to_string()
        }
    };
    if let Some(index) = html.to_lowercase().rfind("</body>") {
        html.insert_str(index, &tap);
    } else {
        html.push_str(&tap);
    }
    html
}

async fn resolve_preview_host_port(
    state: &AppState,
    target: &PreviewHostTarget,
) -> Result<u16, Response> {
    if let Err(error) = require_ready_session(state, &target.session_id).await {
        return Err(error.into_response());
    }
    state
        .read()
        .await
        .session_preview_host_port(&target.session_id, target.logical_port)
        .await
        .map_err(|error| {
            let status = if matches!(
                &error,
                axocoatl_daemon::DaemonError::AttemptConflict(_)
                    | axocoatl_daemon::DaemonError::SessionConflict(_)
            ) {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_GATEWAY
            };
            (
                status,
                format!(
                    "Preview is unavailable for Session '{}' on port {}: {error}",
                    target.session_id, target.logical_port
                ),
            )
                .into_response()
        })
}

async fn proxy_preview_http(
    state: AppState,
    target: PreviewHostTarget,
    tail: String,
    request: axum::http::Request<axum::body::Body>,
    mode: PreviewProxyMode,
) -> Response {
    let host_port = match resolve_preview_host_port(&state, &target).await {
        Ok(host_port) => host_port,
        Err(response) => return response,
    };
    proxy_preview_http_to_port(target, tail, request, mode, host_port).await
}

async fn proxy_preview_http_to_port(
    target: PreviewHostTarget,
    tail: String,
    request: axum::http::Request<axum::body::Body>,
    mode: PreviewProxyMode,
    host_port: u16,
) -> Response {
    let method = request.method().clone();
    let query = request.uri().query().map(str::to_string);
    let request_headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), MAX_PREVIEW_REQUEST_BODY).await {
        Ok(body) => body,
        Err(error) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("Preview request body is too large: {error}"),
            )
                .into_response();
        }
    };
    let upstream = session_preview_upstream(host_port, &tail, query.as_deref());
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Preview proxy client could not start: {error}"),
            )
                .into_response();
        }
    };
    let reqwest_method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(method) => method,
        Err(error) => {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                format!("Unsupported Preview request method: {error}"),
            )
                .into_response();
        }
    };
    let mut upstream_request = preview_upstream_request(
        &client,
        reqwest_method,
        &upstream,
        &request_headers,
        &mode,
        target.logical_port,
    );
    if !body.is_empty() {
        upstream_request = upstream_request.body(body);
    }
    let upstream_response = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        upstream_request.send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!(
                    "couldn't reach {upstream}: {error}. Is a dev server running on port {} inside the Session sandbox?",
                    target.logical_port
                ),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "Preview app on port {} did not send response headers within 15 seconds",
                    target.logical_port
                ),
            )
                .into_response();
        }
    };
    let status = upstream_response.status();
    let upstream_headers = upstream_response.headers().clone();
    let content_type = upstream_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_html = preview_response_needs_html_injection(&content_type);
    if is_html
        && upstream_headers
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        return (
            StatusCode::BAD_GATEWAY,
            "Preview app ignored Accept-Encoding: identity for an HTML response",
        )
            .into_response();
    }
    let body = if !preview_response_allows_body(&method, status) {
        axum::body::Body::empty()
    } else if is_html {
        if upstream_response
            .content_length()
            .is_some_and(|length| length > MAX_PREVIEW_HTML_BODY as u64)
        {
            return (
                StatusCode::BAD_GATEWAY,
                "Preview HTML exceeds the 8 MiB inspection limit",
            )
                .into_response();
        }
        let mut bytes = bytes::BytesMut::new();
        let mut stream = upstream_response.bytes_stream();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    "Preview HTML did not finish within 30 seconds",
                )
                    .into_response();
            }
            let wait = remaining.min(std::time::Duration::from_secs(10));
            let next = match tokio::time::timeout(wait, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        "Preview HTML was idle for more than 10 seconds",
                    )
                        .into_response();
                }
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        format!("Preview HTML could not be read: {error}"),
                    )
                        .into_response();
                }
            };
            if preview_html_size_exceeds(bytes.len(), chunk.len()) {
                return (
                    StatusCode::BAD_GATEWAY,
                    "Preview HTML exceeds the 8 MiB inspection limit",
                )
                    .into_response();
            }
            bytes.extend_from_slice(&chunk);
        }
        axum::body::Body::from(inject_preview_bridge(
            String::from_utf8_lossy(&bytes).into_owned(),
            &mode,
        ))
    } else {
        // Binary assets and API streams stay streaming; only HTML is buffered
        // because it needs the bounded Pick-bridge injection above.
        axum::body::Body::from_stream(upstream_response.bytes_stream())
    };

    let mut response_headers = HeaderMap::new();
    for (name, value) in &upstream_headers {
        if !response_header_is_forwardable(name) {
            continue;
        }
        let value = if name == header::LOCATION {
            rewrite_preview_location(value)
        } else {
            value.clone()
        };
        response_headers.append(name, value);
    }
    response_headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if matches!(mode, PreviewProxyMode::LegacyOpaque { .. }) {
        response_headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(LEGACY_PREVIEW_SANDBOX_CSP),
        );
        response_headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
    }
    let mut response = axum::response::Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
}

async fn preview_websocket_proxy(
    state: AppState,
    target: PreviewHostTarget,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let host_port = match resolve_preview_host_port(&state, &target).await {
        Ok(host_port) => host_port,
        Err(response) => return response,
    };
    preview_websocket_proxy_to_port(target, request, host_port).await
}

async fn preview_websocket_proxy_to_port(
    target: PreviewHostTarget,
    request: axum::http::Request<axum::body::Body>,
    host_port: u16,
) -> Response {
    let tail = request.uri().path().trim_start_matches('/');
    let upstream = session_preview_websocket_upstream(host_port, tail, request.uri().query());
    let (mut parts, _) = request.into_parts();
    let requested_protocol = parts
        .headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let preview_host = parts.headers.get(header::HOST).cloned();
    let origin = parts.headers.get(header::ORIGIN).cloned();
    let cookie = parts.headers.get(header::COOKIE).cloned();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(error) => return error.into_response(),
    };
    let mut upstream_request = match upstream.clone().into_client_request() {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Invalid Preview WebSocket upstream {upstream}: {error}"),
            )
                .into_response();
        }
    };
    if let Some(host) = preview_host {
        upstream_request.headers_mut().insert(header::HOST, host);
    }
    if let Some(origin) = origin {
        upstream_request
            .headers_mut()
            .insert(header::ORIGIN, origin);
    }
    if let Some(cookie) = cookie {
        upstream_request
            .headers_mut()
            .insert(header::COOKIE, cookie);
    }
    if let Some(protocol) = requested_protocol.as_deref() {
        if let Ok(value) = HeaderValue::from_str(protocol) {
            upstream_request
                .headers_mut()
                .insert(header::SEC_WEBSOCKET_PROTOCOL, value);
        }
    }
    let (upstream_socket, upstream_handshake) = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio_tungstenite::connect_async(upstream_request),
    )
    .await
    {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Preview WebSocket could not reach {upstream}: {error}"),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "Preview WebSocket on port {} did not complete its handshake within 15 seconds",
                    target.logical_port
                ),
            )
                .into_response();
        }
    };
    let selected_protocol = upstream_handshake
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let upgrade = if let Some(protocol) = selected_protocol {
        upgrade.protocols([protocol])
    } else {
        upgrade
    };
    upgrade
        .on_upgrade(move |client_socket| bridge_preview_websocket(client_socket, upstream_socket))
        .into_response()
}

async fn bridge_preview_websocket(
    mut client: AxumWebSocket,
    mut upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    loop {
        tokio::select! {
            client_message = client.recv() => {
                let Some(Ok(message)) = client_message else { break; };
                let message = match message {
                    AxumWsMessage::Text(text) => UpstreamWsMessage::Text(text.to_string().into()),
                    AxumWsMessage::Binary(bytes) => UpstreamWsMessage::Binary(bytes),
                    AxumWsMessage::Ping(bytes) => UpstreamWsMessage::Ping(bytes),
                    AxumWsMessage::Pong(bytes) => UpstreamWsMessage::Pong(bytes),
                    AxumWsMessage::Close(_) => UpstreamWsMessage::Close(None),
                };
                let closing = matches!(message, UpstreamWsMessage::Close(_));
                if upstream.send(message).await.is_err() || closing { break; }
            }
            upstream_message = upstream.next() => {
                let Some(Ok(message)) = upstream_message else { break; };
                let message = match message {
                    UpstreamWsMessage::Text(text) => AxumWsMessage::Text(text.to_string().into()),
                    UpstreamWsMessage::Binary(bytes) => AxumWsMessage::Binary(bytes),
                    UpstreamWsMessage::Ping(bytes) => AxumWsMessage::Ping(bytes),
                    UpstreamWsMessage::Pong(bytes) => AxumWsMessage::Pong(bytes),
                    UpstreamWsMessage::Close(_) => AxumWsMessage::Close(None),
                    UpstreamWsMessage::Frame(_) => continue,
                };
                let closing = matches!(message, AxumWsMessage::Close(_));
                if client.send(message).await.is_err() || closing { break; }
            }
        }
    }
}

/// Compatibility proxy URL retained for older callers. The product iframe uses
/// the per-Session/per-port Preview host; this path remains response-sandboxed
/// because it shares the workbench origin.
pub async fn session_browser_proxy(
    State(state): State<AppState>,
    Path((session_id, port, tail)): Path<(String, u16, String)>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    if let Err(error) = require_ready_session(&state, &session_id).await {
        return error.into_response();
    }
    let base = format!("/api/sessions/{session_id}/proxy/{port}/");
    proxy_preview_http(
        state,
        PreviewHostTarget {
            session_id,
            logical_port: port,
        },
        tail,
        req,
        PreviewProxyMode::LegacyOpaque { base },
    )
    .await
}

/// Same handler as above but for the proxy root (no `tail`), e.g. when
/// the user types `localhost:8765` and the iframe hits the bare port.
pub async fn session_browser_proxy_root(
    State(state): State<AppState>,
    Path((session_id, port)): Path<(String, u16)>,
    req: axum::http::Request<axum::body::Body>,
) -> Response {
    if let Err(error) = require_ready_session(&state, &session_id).await {
        return error.into_response();
    }
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
    fn measured_projection_serializes_numeric_lower_bound_with_reasoning() {
        let response = ExecuteResponse {
            output: "partial".to_string(),
            usage: ExecutionUsageResponse::new(
                &axocoatl_core::TokenUsageStats::new(13, 8).with_reasoning(3),
                false,
            ),
        };
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["output"], "partial");
        assert_eq!(value["input_tokens"], 13);
        assert_eq!(value["output_tokens"], 8);
        assert_eq!(value["reasoning_tokens"], 3);
        assert_eq!(value["total_tokens"], 24);
        assert_eq!(value["token_usage_known"], false);
    }

    #[test]
    fn measured_projection_maps_handled_workflow_failure_to_error_frame() {
        let output = axocoatl_daemon::WorkflowOutput {
            workflow_id: "review".to_string(),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            final_content: "partial".to_string(),
            total_token_usage: axocoatl_core::TokenUsageStats::new(13, 8).with_reasoning(3),
            token_usage_known: false,
            completed_agents: vec!["reader".to_string()],
            failed_agents: vec![("writer".to_string(), "provider timeout".to_string())],
        };
        let frame = workflow_error_frame(
            output.workflow_id.clone(),
            output.terminal_error().expect("handled failure"),
        );
        match frame {
            axocoatl_daemon::StreamFrame::WorkflowError {
                workflow,
                error,
                input_tokens,
                output_tokens,
                reasoning_tokens,
                token_usage_known,
            } => {
                assert_eq!(workflow, "review");
                assert!(error.contains("writer: provider timeout"));
                assert_eq!(input_tokens, 13);
                assert_eq!(output_tokens, 8);
                assert_eq!(reasoning_tokens, 3);
                assert!(!token_usage_known);
            }
            other => panic!("expected workflow-error, got {other:?}"),
        }
    }

    #[test]
    fn measured_projection_chat_terminal_carries_turn_and_lower_bound() {
        let value: serde_json::Value = serde_json::from_str(&chat_terminal_message(
            "chat-stopped",
            "chat-a",
            "chat-run-a",
            &axocoatl_core::TokenUsageStats::new(13, 8).with_reasoning(3),
            false,
            None,
        ))
        .unwrap();
        assert_eq!(value["kind"], "chat-stopped");
        assert_eq!(value["chat_id"], "chat-a");
        assert_eq!(value["turn_id"], "chat-run-a");
        assert_eq!(value["input_tokens"], 13);
        assert_eq!(value["output_tokens"], 8);
        assert_eq!(value["reasoning_tokens"], 3);
        assert_eq!(value["total_tokens"], 24);
        assert_eq!(value["token_usage_known"], false);
    }

    #[test]
    fn measured_projection_older_chat_cannot_clear_newer_control() {
        let older = axocoatl_actor::AgentRunControl::new(axocoatl_actor::AgentRunId::new("old"));
        let newer = axocoatl_actor::AgentRunControl::new(axocoatl_actor::AgentRunId::new("new"));
        let mut active = std::collections::HashMap::from([("chat-a".to_string(), newer.clone())]);

        assert!(!clear_chat_control_if_owner(&mut active, "chat-a", &older));
        assert_eq!(active["chat-a"].id(), newer.id());
        assert!(clear_chat_control_if_owner(&mut active, "chat-a", &newer));
        assert!(!active.contains_key("chat-a"));
    }

    #[test]
    fn retained_chat_download_forces_active_types_to_opaque_attachments() {
        for declared in ["text/html", "image/svg+xml"] {
            let response =
                safe_attachment_response(b"<active-content>".to_vec(), declared, "page.html");
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/octet-stream"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
                "attachment; filename=\"page.html\""
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::X_CONTENT_TYPE_OPTIONS)
                    .unwrap(),
                "nosniff"
            );
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "private, no-store"
            );
            assert!(!response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY));
        }
    }

    #[test]
    fn retained_chat_download_keeps_inert_media_inline_and_export_name_ascii() {
        let image = safe_attachment_response(Vec::new(), "image/png", "plot.png");
        assert_eq!(
            image.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(
            image.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "inline; filename=\"plot.png\""
        );
        assert!(!image
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY));

        let pdf = safe_attachment_response(Vec::new(), "application/pdf", "report.pdf");
        assert_eq!(
            pdf.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        assert_eq!(
            pdf.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "inline; filename=\"report.pdf\""
        );
        assert_eq!(
            pdf.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
            "sandbox; default-src 'none'"
        );

        assert_eq!(
            export_content_disposition("Résumé", "json"),
            "attachment; filename=\"R_sum_.json\""
        );
    }

    #[test]
    fn token_report_counts_reasoning_and_maps_only_exact_live_scopes() {
        let configured = ["coder".to_string(), "reviewer".to_string()]
            .into_iter()
            .collect();
        let sessions = ["ses-1".to_string()].into_iter().collect();
        let report = build_token_report(
            vec![
                (
                    "coder".to_string(),
                    axocoatl_core::MeasuredTokenUsage::known(
                        axocoatl_core::TokenUsageStats::new(10, 5).with_reasoning(7),
                    ),
                ),
                (
                    "ses-1:coder".to_string(),
                    axocoatl_core::MeasuredTokenUsage::known(
                        axocoatl_core::TokenUsageStats::new(3, 2).with_reasoning(1),
                    ),
                ),
                (
                    "not-a-session:coder".to_string(),
                    axocoatl_core::MeasuredTokenUsage::lower_bound(
                        axocoatl_core::TokenUsageStats::new(1, 1),
                    ),
                ),
            ],
            &configured,
            &sessions,
        );

        assert_eq!(report.total_input, 14);
        assert_eq!(report.total_output, 8);
        assert_eq!(report.total_reasoning, 8);
        assert_eq!(report.total, 30);
        assert!(!report.token_usage_known);
        assert_eq!(report.per_agent[0].total_tokens, 22);
        assert!(report.per_agent[0].token_usage_known);
        assert_eq!(
            report.per_agent[0].template_agent_id.as_deref(),
            Some("coder")
        );
        assert_eq!(report.per_agent[0].scope, "global");
        assert_eq!(
            report.per_agent[1].template_agent_id.as_deref(),
            Some("coder")
        );
        assert_eq!(report.per_agent[1].scope, "session");
        assert_eq!(report.per_agent[2].template_agent_id, None);
        assert_eq!(report.per_agent[2].scope, "other");
        assert!(!report.per_agent[2].token_usage_known);
    }

    #[test]
    fn streamed_tool_evidence_prefers_child_agent_and_falls_back_to_outer_agent() {
        assert_eq!(
            stream_evidence_agent(
                Some("session:coordinator:worker:builder".to_string()),
                "coordinator"
            ),
            "session:coordinator:worker:builder"
        );
        assert_eq!(stream_evidence_agent(None, "coordinator"), "coordinator");
    }

    #[test]
    fn websocket_reconnect_forwards_only_frames_after_the_snapshot_cursor() {
        let represented = axocoatl_daemon::SequencedStreamFrame {
            sequence: 7,
            frame: axocoatl_daemon::StreamFrame::Token {
                workflow: "session-a".to_string(),
                agent: "coder".to_string(),
                turn_id: Some("turn-a".to_string()),
                delta: "already in snapshot".to_string(),
            },
        };
        assert!(stream_frame_after_cursor(7, represented).is_none());

        let live = axocoatl_daemon::SequencedStreamFrame {
            sequence: 8,
            frame: axocoatl_daemon::StreamFrame::Token {
                workflow: "session-a".to_string(),
                agent: "coder".to_string(),
                turn_id: Some("turn-a".to_string()),
                delta: "live only".to_string(),
            },
        };
        let forwarded = stream_frame_after_cursor(7, live).expect("post-cursor frame");
        assert!(matches!(
            forwarded,
            axocoatl_daemon::StreamFrame::Token { delta, .. } if delta == "live only"
        ));
    }

    #[tokio::test]
    async fn lagged_terminal_output_requires_an_explicit_resync_close() {
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut receiver = sender.subscribe();
        sender.send(b"first".to_vec()).unwrap();
        sender.send(b"second".to_vec()).unwrap();

        let TerminalOutputEvent::Resync { missed } = terminal_output_event(receiver.recv().await)
        else {
            panic!("a lagged terminal receiver must not continue with a later chunk");
        };
        assert_eq!(missed, 1);
        let close = terminal_resync_close_frame(missed);
        assert_eq!(close.code, TERMINAL_RESYNC_CLOSE_CODE);
        assert_eq!(
            close.reason.as_str(),
            "terminal-resync-required: missed 1 output chunks"
        );
    }

    fn session_with_environment_state(
        state: axocoatl_session::SessionEnvironmentState,
    ) -> axocoatl_session::Session {
        axocoatl_session::Session {
            id: "session-guard-test".to_string(),
            name: "Guard test".to_string(),
            workspace_id: "workspace-guard-test".to_string(),
            working_dir: std::path::PathBuf::from("/tmp/session-guard-test"),
            mode: axocoatl_session::SessionMode::SingleAgent {
                agent_id: "coder".to_string(),
            },
            status: axocoatl_session::SessionStatus::Active,
            enabled_skills: Vec::new(),
            exposed_ports: vec![3000],
            image: Some("node:20-slim".to_string()),
            post_create_commands: Vec::new(),
            check_command: None,
            environment: axocoatl_session::SessionEnvironment {
                state,
                error: (state == axocoatl_session::SessionEnvironmentState::Failed)
                    .then(|| "exact setup failed".to_string()),
                ..axocoatl_session::SessionEnvironment::default()
            },
            created_at: 1,
            last_active: 1,
        }
    }

    #[test]
    fn session_route_guard_is_404_for_missing_and_409_for_every_non_ready_state() {
        let (status, Json(body)) =
            session_environment_route_error("missing", None).expect("missing must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.error, "session 'missing' not found");

        for state in [
            axocoatl_session::SessionEnvironmentState::Unprepared,
            axocoatl_session::SessionEnvironmentState::AwaitingApproval,
            axocoatl_session::SessionEnvironmentState::Preparing,
            axocoatl_session::SessionEnvironmentState::Failed,
        ] {
            let session = session_with_environment_state(state);
            let (status, Json(body)) = session_environment_route_error(&session.id, Some(&session))
                .expect("every non-Ready state must fail closed");
            assert_eq!(status, StatusCode::CONFLICT, "wrong status for {state:?}");
            assert!(body.error.contains("Session 'Guard test'"));
            assert!(body.error.contains("environment"));
            if state == axocoatl_session::SessionEnvironmentState::Failed {
                assert!(body.error.contains("exact setup failed"));
            }
        }

        let ready =
            session_with_environment_state(axocoatl_session::SessionEnvironmentState::Ready);
        assert!(session_environment_route_error(&ready.id, Some(&ready)).is_none());
    }

    #[test]
    fn live_checkout_http_handlers_call_the_ready_guard_before_daemon_work() {
        let source = include_str!("routes.rs");
        let handler_source = |handler: &str| {
            let marker = format!("pub async fn {handler}(");
            let start = source
                .find(&marker)
                .unwrap_or_else(|| panic!("missing handler {handler}"));
            let rest = &source[start + marker.len()..];
            let end = rest.find("\npub async fn ").unwrap_or(rest.len());
            &rest[..end]
        };
        for handler in [
            "git_status",
            "git_diff",
            "git_branches",
            "git_commit",
            "git_hunks",
            "git_apply_hunk",
            "git_stage",
            "git_unstage",
            "git_discard",
            "git_revert_hunk",
            "git_checkout",
            "session_variants",
            "session_variants_status",
            "session_variants_trajectories",
            "session_variants_verify",
            "session_variant_diff",
            "session_variants_judge",
            "session_variants_plan",
            "execute_session",
            "session_tree",
            "session_tasks",
            "session_task_spawn",
            "session_terminal_ws",
            "session_file",
            "session_file_write",
            "session_browser_proxy",
            "session_browser_proxy_root",
        ] {
            assert!(
                handler_source(handler).contains("require_ready_session(&state,"),
                "{handler} can reach live Session state without the centralized Ready guard"
            );
        }

        // These reads and resolution actions exist precisely so a user can
        // understand or clean up a Session after a failed preparation or a
        // daemon restart. Blocking them would turn the safety gate into a
        // dead-end.
        for handler in [
            "session_variants_results",
            "session_variants_cost",
            "session_variant_adopt",
            "session_variants_discard",
        ] {
            assert!(
                !handler_source(handler).contains("require_ready_session(&state,"),
                "{handler} must remain available for non-Ready attempt resolution"
            );
        }
    }

    #[test]
    fn dashboard_files_layout_tolerates_editor_before_custom_element_upgrade() {
        assert!(DASHBOARD_HTML
            .contains("const hasContent = (Array.isArray(files) && files.length > 0)"));
        assert!(!DASHBOARD_HTML.contains("axEditor().files.length"));
    }

    #[test]
    fn one_app_visible_controls_have_reverse_transitions() {
        // Product-seam contract: every focused/docked/sheet surface has a
        // visible way back. These assertions intentionally read the embedded
        // artifact rather than a helper below the browser boundary.
        for required_control in [
            "id=\"mobile-home-rail-toggle\"",
            "id=\"mobile-rail-toggle\"",
            "id=\"attempts-dock-close\"",
            "id=\"term-drawer-close-btn\"",
            "id=\"session-history-btn\"",
            "id=\"cockpit-back\"",
            "data-act=\"conversation\"",
            "id=\"session-run-action\"",
        ] {
            assert!(
                DASHBOARD_HTML.contains(required_control),
                "missing visible reverse/control path: {required_control}"
            );
        }

        for required_handler in [
            "$('#mobile-home-rail-toggle')?.addEventListener('click', openMobileRail)",
            "$('#attempts-dock-close')?.addEventListener('click'",
            "setAttemptsDock(false)",
            "toggleTerminalsDrawer(false)",
            "openModule('stream')",
            "addEventListener('click', stopSessionTurn)",
            "if (e.key === 'Escape' && document.body.classList.contains('mobile-rail-open'))",
            "rail?.toggleAttribute('inert', concealed)",
            "if (concealed) rail?.setAttribute('aria-hidden', 'true')",
            "scrim.hidden = !open",
            "closeMobileRail(true)",
        ] {
            assert!(
                DASHBOARD_HTML.contains(required_handler),
                "visible control is not wired at the product seam: {required_handler}"
            );
        }
    }

    #[test]
    fn one_app_custom_controls_and_dialogs_are_keyboard_operable() {
        const SOURCE_CONTROL_JS: &str = include_str!("../static/ui/source-control.js");
        const SETTINGS_JS: &str = include_str!("../static/ui/settings.js");
        for contract in [
            "modal.setAttribute('role', 'dialog')",
            "modal.setAttribute('aria-modal', 'true')",
            "else if (e.key === 'Tab')",
            "queueMicrotask(() => returnFocus?.isConnected && returnFocus.focus?.())",
            "this.setAttribute('role', 'switch')",
            "this.setAttribute('aria-checked', String(this.checked))",
            "this._trig.setAttribute('role', 'combobox')",
            "this._menu.setAttribute('role', 'listbox')",
            "event.key === 'ArrowDown' || event.key === 'ArrowUp'",
            "row.setAttribute('role', 'menuitemradio')",
            "reset.setAttribute('role', 'menuitem')",
            "id=\"file-tabs\" role=\"tablist\"",
            "main.setAttribute('role', 'tab')",
            "row.setAttribute('aria-pressed', String(active))",
            "x.setAttribute('aria-label', `Remove ${label} from this turn`)",
            "card.setAttribute('role', 'dialog')",
            "input.setAttribute('aria-activedescendant'",
            "head.setAttribute('aria-expanded', 'false')",
            "live connection reconnecting…",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "missing keyboard/dialog contract: {contract}"
            );
        }
        assert!(SOURCE_CONTROL_JS.contains("main.className = 'file-main'"));
        assert!(SOURCE_CONTROL_JS.contains("main.setAttribute('aria-label', `Open changes for"));
        assert!(!SOURCE_CONTROL_JS.contains("row.setAttribute('role', 'button')"));
        assert!(SETTINGS_JS.contains("while (active?.shadowRoot?.activeElement)"));
        assert!(SETTINGS_JS.contains("this.#returnFocus = deepActiveElement()"));
    }

    #[test]
    fn one_app_review_surfaces_preserve_usable_layout_and_git_hierarchy() {
        const SOURCE_CONTROL_JS: &str = include_str!("../static/ui/source-control.js");
        const SHELL_CSS: &str = include_str!("../static/ui/shell.css");

        // The shell state can represent one center surface and, at most, one
        // explicit Conversation companion. The retired arbitrary pane array
        // was what allowed five unusable columns to be requested at once.
        for contract in [
            "center: 'stream'",
            "pinned: null",
            "const CENTER_SURFACES = Object.freeze(Object.keys(SURFACES))",
            "if (center !== 'stream') return [center]",
            "return canPairSurface(pinned) ? ['stream', pinned] : ['stream']",
            "const FILES_EXPLORER_DEFAULTS = { files: 280, git: 360 }",
            "const FILES_EXPLORER_MIN = { files: 220, git: 300 }",
            "const FILES_MASTER_DETAIL_BREAKPOINT = 820",
            "sourceControl.scope = 'lastTurn'",
            "id=\"files-detail-path\"",
            "id=\"git-diff-mode\"",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "missing focused-review layout contract: {contract}"
            );
        }
        assert!(!DASHBOARD_HTML.contains("paneOrder:"));

        // Source Control adapts to its own container, keeps provenance and
        // staging as separate dimensions, and makes a deliberate commit the
        // primary terminal action.
        for contract in [
            "container-type: inline-size",
            "<span>Last turn</span>",
            "<span>All changes</span>",
            "this.#renderSection(host, 'Staged'",
            "this.#renderSection(host, 'Changes'",
            "Commit ${staged} staged",
            "Commit message required",
            "Stage all and commit",
            "role=\"menu\"",
        ] {
            assert!(
                SOURCE_CONTROL_JS.contains(contract),
                "missing Source Control hierarchy contract: {contract}"
            );
        }
        assert!(SHELL_CSS.contains(".files-inner-resizer::before"));
        assert!(SHELL_CSS.contains(".files-pane-inner.files-compact.show-detail"));
    }

    #[test]
    fn one_app_composer_keeps_writing_primary_and_answer_mode_secondary() {
        const SHELL_CSS: &str = include_str!("../static/ui/shell.css");
        const FANOUT_JS: &str = include_str!("../static/ui/fanout.js");

        // The request owns its own full-width row. Per-turn configuration lives
        // in a separate wrapping footer, and Send is the only primary action.
        for contract in [
            "<div class=\"session-input-row\">\n                <textarea",
            "<div class=\"session-input-footer\">",
            "<span class=\"fanout-label\">One answer</span>",
            "aria-haspopup=\"dialog\"",
            "label.textContent = on ? `${n} ways` : 'One answer'",
            "Answer mode: one answer. Configure",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "missing composer hierarchy contract: {contract}"
            );
        }
        assert!(SHELL_CSS.contains(".session-input textarea { display: block; width: 100%;"));
        assert!(SHELL_CSS.contains(".session-input-footer { display: flex; min-width: 0;"));
        assert!(!DASHBOARD_HTML.contains(">Explore ways</span>"));

        let toggle = DASHBOARD_HTML
            .split("function toggleFanoutPop()")
            .nth(1)
            .and_then(|tail| tail.split("function closeFanoutPop()").next())
            .expect("answer-mode toggle function should remain embedded");
        assert!(!toggle.contains("setAttemptsDock(true)"));
        assert!(FANOUT_JS.contains("position: fixed; left: var(--fanout-left"));
        assert!(FANOUT_JS.contains("box-sizing: border-box; width: min(420px"));
        assert!(FANOUT_JS.contains("aria-label=\"Close answer mode configuration\""));
        assert!(FANOUT_JS.contains("`Remove attempt ${i + 1}`"));
    }

    #[test]
    fn one_app_agent_graph_uses_configured_session_dependencies() {
        // Product-seam contract: the API exposes the canonical configured
        // relation and the embedded app turns only active Full/Custom
        // dependencies into dependency -> dependent lattice edges.
        let agent = AgentInfo {
            id: "reviewer".to_string(),
            name: "Reviewer".to_string(),
            provider: "ollama".to_string(),
            model: "qwen".to_string(),
            depends_on: vec!["architect".to_string()],
            team: "Engineering".to_string(),
            role: "autonomous".to_string(),
            system_prompt: None,
            per_call_budget: None,
            per_execution_budget: None,
            overflow_policy: None,
        };
        let payload = serde_json::to_value(agent).expect("AgentInfo should serialize");
        assert_eq!(payload["depends_on"], serde_json::json!(["architect"]));

        for contract in [
            "function sessionAgentDependencyEdges(session, agentIds)",
            "session.mode.kind !== 'lattice' && session.mode.kind !== 'custom'",
            "Array.isArray(agent?.depends_on) ? agent.depends_on : []",
            "if (activeAgentIds.has(dependencyId))",
            "edge.setAttribute('from', 'sl-' + dependency.from + ':out')",
            "edge.setAttribute('to', 'sl-' + dependency.to + ':in')",
            "dependency.to + ' depends on ' + dependency.from",
            "lat.setNodeStatus('sl-' + agent, status)",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "Session Agent graph lost its configured-dependency seam: {contract}"
            );
        }
    }

    #[test]
    fn rail_and_session_canvas_preserve_reopen_and_overflow_invariants() {
        const RAIL_JS: &str = include_str!("../static/ui/rail.js");
        const SESSION_HOME_JS: &str = include_str!("../static/ui/session-home.js");
        const SHELL_CSS: &str = include_str!("../static/ui/shell.css");

        assert!(RAIL_JS.contains("collapse.addEventListener('click', toggleCollapsed)"));
        assert!(!RAIL_JS.contains("collapse.addEventListener('pointerdown'"));
        assert!(RAIL_JS.contains("display: inline-flex; width: 32px; height: 32px;"));
        assert!(RAIL_JS.contains(":host([collapsed]) .empty,"));
        assert!(RAIL_JS.contains(":host([collapsed]) .load-error { display: none; }"));
        assert!(RAIL_JS.contains("session.workspace_id === workspace.id"));
        assert!(RAIL_JS.contains("session.status !== 'closed' || session.id === this.current"));
        assert!(RAIL_JS.contains("fetch('/api/workspaces')"));
        assert!(RAIL_JS.contains("get workspace()"));
        assert!(RAIL_JS.contains("focusFirstControl()"));
        assert!(
            RAIL_JS.contains("const label = collapsed ? 'Expand the rail' : 'Collapse the rail'")
        );
        // Every desktop entry point must control the actual <ax-rail>. The
        // deleted legacy sidebar classes made the keyboard path a silent no-op,
        // while an unconditional resize policy could undo a user's expansion.
        for contract in [
            "function setRailCollapsed(collapsed, { persist = true } = {})",
            "function toggleRail()",
            "setRailCollapsed(!rail.collapsed)",
            "const preferred = readRailCollapsedPreference();",
            "setRailCollapsed(preferred ?? window.innerWidth < 1100, { persist: false })",
            "setRailCollapsed(Boolean(e.detail.collapsed));",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "rail lost its unified reversible state contract: {contract}"
            );
        }
        for retired in [
            "S.sidebarMode",
            "function cycleSidebar()",
            "function applySidebar()",
        ] {
            assert!(
                !DASHBOARD_HTML.contains(retired),
                "retired sidebar state returned: {retired}"
            );
        }
        assert!(!SHELL_CSS.contains("body.side-mini"));
        assert!(!SHELL_CSS.contains("body.side-hidden"));
        assert!(SHELL_CSS.contains(".session-surface { flex: 1; display: flex; flex-direction: column; min-height: 0; min-width: 0; overflow: hidden;"));
        assert!(SHELL_CSS.contains(".toolcard { flex: 0 0 auto;"));
        assert!(SHELL_CSS.contains(
            "#session-surface.session-open > .mobile-home-rail-toggle { display: none; }"
        ));
        assert!(SESSION_HOME_JS
            .contains("<div class=\"rows\" role=\"tree\" aria-label=\"All Sessions\">"));
        assert!(SESSION_HOME_JS.contains("row.setAttribute('role', 'treeitem')"));
        assert!(SESSION_HOME_JS
            .contains("const openVerb = session.status === 'closed' ? 'Review' : 'Open';"));
        assert!(SESSION_HOME_JS.contains(
            "row.setAttribute('aria-label', `${openVerb}${closedQualifier} Session ${sessionName}"
        ));
        assert!(SESSION_HOME_JS
            .contains("this.#openPicker('session', workspaceDirectory(workspace), workspace.id)"));
        assert!(SESSION_HOME_JS
            .contains("/api/workspaces/${encodeURIComponent(workspace.id)}/sessions"));
        assert!(!SESSION_HOME_JS.contains("working_dir: picker.path"));
        assert!(DASHBOARD_HTML.contains("id=\"bx-pick\" type=\"button\" aria-pressed=\"false\""));
        assert!(DASHBOARD_HTML.contains("btn.setAttribute('aria-pressed', String(!!on))"));
        assert!(
            DASHBOARD_HTML.contains("btn.textContent = on ? 'Stop inspecting' : 'Inspect element'")
        );
    }

    #[test]
    fn workspace_navigation_is_last_action_wins_and_legacy_import_retries_only_failures() {
        const SESSION_HOME_JS: &str = include_str!("../static/ui/session-home.js");

        for contract in [
            "let _workspaceNavigationGeneration = 0;",
            "function openCockpit(s) {\n  _workspaceNavigationGeneration += 1;",
            "function closeCockpit(options = {}) {\n  _workspaceNavigationGeneration += 1;",
            "const navigationGeneration = ++_workspaceNavigationGeneration;",
            "navigationGeneration !== _workspaceNavigationGeneration",
            "S.selectedWorkspaceId !== workspace.id",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "Workspace navigation lost its stale-activation guard: {contract}"
            );
        }
        let open_cockpit_start = DASHBOARD_HTML.find("function openCockpit(s) {").unwrap();
        let open_cockpit_end = DASHBOARD_HTML[open_cockpit_start..]
            .find("function bindSessionComponents(s)")
            .map(|offset| open_cockpit_start + offset)
            .unwrap();
        let open_cockpit = &DASHBOARD_HTML[open_cockpit_start..open_cockpit_end];
        assert!(open_cockpit.contains("if (S.session.status !== 'closed')"));
        assert!(
            !open_cockpit.contains("/reopen"),
            "selecting a Closed Session must mount recovery evidence without implicit Reopen"
        );

        for contract in [
            "const pending = [];",
            "} catch { pending.push(favorite); }",
            "this.#legacyFavorites = pending;",
            "localStorage.setItem(LEGACY_FAVORITES_KEY, JSON.stringify(pending))",
            "else localStorage.removeItem(LEGACY_FAVORITES_KEY);",
        ] {
            assert!(
                SESSION_HOME_JS.contains(contract),
                "legacy Favorites migration lost its per-folder retry ledger: {contract}"
            );
        }
        assert!(!SESSION_HOME_JS.contains("let failed = false;"));
    }

    #[test]
    fn environment_request_binds_approval_to_an_exact_reviewed_command() {
        let approved: ConfigureSessionEnvironmentBody = serde_json::from_str(
            r#"{"image":"docker.io/library/node:20-slim","setup_command":"npm ci","setup_approved":true,"setup_reviewed":true}"#,
        )
        .unwrap();
        assert!(approved.validate().is_ok());

        let explicit_skip: ConfigureSessionEnvironmentBody = serde_json::from_str(
            r#"{"image":null,"setup_command":null,"setup_approved":false,"setup_reviewed":true}"#,
        )
        .unwrap();
        assert!(explicit_skip.validate().is_ok());

        let bare_approval: ConfigureSessionEnvironmentBody =
            serde_json::from_str(r#"{"setup_approved":true,"setup_reviewed":true}"#).unwrap();
        assert!(bare_approval.validate().unwrap_err().contains("exact"));

        let unreviewed: ConfigureSessionEnvironmentBody =
            serde_json::from_str(r#"{"setup_command":"npm ci","setup_reviewed":false}"#).unwrap();
        assert!(unreviewed
            .validate()
            .unwrap_err()
            .contains("explicit setup decision"));

        assert!(serde_json::from_str::<ConfigureSessionEnvironmentBody>(
            r#"{"setup_reviewed":true,"implicit_setup":true}"#
        )
        .is_err());
    }

    #[test]
    fn e2b_rejects_per_session_oci_images_before_provisioning() {
        assert!(session_image_backend_conflict("podman", None, Some("node:20-slim")).is_none());
        assert!(session_image_backend_conflict("e2b", Some("base"), None).is_none());
        let error = session_image_backend_conflict(
            "e2b",
            Some("team-template"),
            Some("docker.io/library/node:20-slim"),
        )
        .expect("E2B cannot honor a Session OCI image");
        assert!(error.contains("team-template"));
        assert!(error.contains("docker.io/library/node:20-slim"));
        assert!(error.contains("Clear the Base image"));
    }

    #[test]
    fn session_home_makes_setup_consent_runtime_failure_and_rebuild_visible() {
        const SESSION_HOME_JS: &str = include_str!("../static/ui/session-home.js");

        for contract in [
            "probe?.suggested_setup?.source === 'package-lock'",
            "this.#setPickerImage('docker.io/library/node:20-slim', false)",
            "candidate.replace('docker.io/library/', '')",
            "void this.#probePickerProject(this.#picker, sessionDirectory(session), 0)",
            "picker.form.setupApproved = false",
            "Run this exact command before Ready",
            "setup_approved: Boolean(form.setupCommand.trim() && form.setupApproved)",
            "setup_reviewed: true",
            "/environment/rebuild",
            "configureEnvironment(sessionId)",
            "rebuildEnvironment(sessionId)",
            "session-environment-change",
            "session-environment-changing",
            "Environment preparation failed",
            "environment?.setup_results",
            "Setup evidence",
            "Setup command ${index + 1}",
            "supports_session_image",
            "auto_approve_devcontainer_setup",
            "Daemon policy defaults this exact devcontainer setup to approved",
            "E2B template",
            "Incompatible OCI image",
        ] {
            assert!(
                SESSION_HOME_JS.contains(contract),
                "Session environment workflow lost its visible product seam: {contract}"
            );
        }
        assert!(SESSION_HOME_JS.contains("devcontainer.json could not be read"));
        assert!(SESSION_HOME_JS.contains("malformedDevcontainerBlocksCreation"));
        assert!(SESSION_HOME_JS.contains("malformedDevcontainerNeedsImageDecision"));
        assert!(SESSION_HOME_JS.contains("projectProbeFailureBlocksCreation"));
        assert!(SESSION_HOME_JS.contains("!picker.form.imageTouched"));
    }

    #[test]
    fn workbench_retains_cross_tab_runtime_lifecycle_and_reconnect_gates() {
        for contract in [
            "const _sessionEnvironmentTransitions = new Map();",
            "function beginRemoteSessionEnvironmentChange(frame)",
            "function settleRemoteSessionEnvironmentChange(frame)",
            "function replaceSessionEnvironmentTransitions(items)",
            "case 'session-environment-changing':",
            "case 'session-environment-settled':",
            "replaceSessionEnvironmentTransitions(d.environment_transitions);",
            "_sessionEnvironmentTransitions.has(expectedSessionId)",
            "reconcileSessionEnvironmentLifecycle(sessionId, { invalidatesRuntime: true })",
            "const _workspaceAttemptOwnerships = new Map();",
            "function beginRemoteWorkspaceAttemptChange(frame)",
            "function settleRemoteWorkspaceAttemptChange(frame)",
            "function replaceWorkspaceAttemptOwnerships(items)",
            "case 'workspace-attempt-changing':",
            "case 'workspace-attempt-settled':",
            "replaceWorkspaceAttemptOwnerships(d.attempt_ownerships);",
            "workspaceAttemptOwnsPrimaryRuntime(sessionState)",
        ] {
            assert!(
                DASHBOARD_HTML.contains(contract),
                "Session environment lifecycle lost its cross-tab gate: {contract}"
            );
        }
    }

    fn stored_message(
        role: axocoatl_core::MessageRole,
        content: &str,
    ) -> axocoatl_memory::session::StoredMessage {
        axocoatl_memory::session::StoredMessage {
            role,
            content: content.to_string(),
            timestamp: 1,
            token_count: 0,
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn persisted_chat(
        messages: Vec<axocoatl_memory::session::StoredMessage>,
    ) -> axocoatl_memory::chat::Chat {
        axocoatl_memory::chat::Chat {
            id: "chat-test".to_string(),
            name: "Test chat".to_string(),
            agent_id: "assistant".to_string(),
            system_override: None,
            model_override: None,
            starred: false,
            parent_id: None,
            forked_at_message: None,
            messages,
            attachments: Vec::new(),
            created_at: 1,
            last_active: 1,
        }
    }

    #[test]
    fn chat_agent_input_preserves_history_order_without_duplicating_current_content() {
        let chat = persisted_chat(vec![
            stored_message(axocoatl_core::MessageRole::System, "chat system"),
            stored_message(axocoatl_core::MessageRole::User, "first question"),
            stored_message(axocoatl_core::MessageRole::Assistant, "first answer"),
        ]);

        let input = build_chat_agent_input(&chat, "current question", Vec::new());

        assert_eq!(input.content, "current question");
        assert_eq!(input.history.len(), 3);
        assert_eq!(
            input
                .history
                .iter()
                .map(|message| message.role.clone())
                .collect::<Vec<_>>(),
            vec![
                axocoatl_core::MessageRole::System,
                axocoatl_core::MessageRole::User,
                axocoatl_core::MessageRole::Assistant,
            ]
        );
        assert_eq!(input.history[0].text_content(), Some("chat system"));
        assert_eq!(input.history[1].text_content(), Some("first question"));
        assert_eq!(input.history[2].text_content(), Some("first answer"));
        assert!(input
            .history
            .iter()
            .all(|message| message.text_content() != Some("current question")));
    }

    #[test]
    fn chat_agent_input_preserves_tool_correlation_and_argument_shape() {
        let mut assistant = stored_message(axocoatl_core::MessageRole::Assistant, "calling");
        assistant.name = Some("assistant-name".to_string());
        assistant.tool_calls = vec![
            axocoatl_memory::session::StoredToolCall {
                id: "call-valid".to_string(),
                name: "search".to_string(),
                arguments_json: r#"{"q":"axocoatl"}"#.to_string(),
                provider_metadata: axocoatl_core::ProviderMetadata::from([(
                    "gemini.thought_signature".to_string(),
                    "exact-signature".to_string(),
                )]),
            },
            axocoatl_memory::session::StoredToolCall {
                id: "call-invalid".to_string(),
                name: "broken".to_string(),
                arguments_json: "not-json".to_string(),
                provider_metadata: Default::default(),
            },
        ];
        let mut tool = stored_message(axocoatl_core::MessageRole::Tool, "search result");
        tool.name = Some("search".to_string());
        tool.tool_call_id = Some("call-valid".to_string());
        let chat = persisted_chat(vec![assistant, tool]);

        let input = build_chat_agent_input(&chat, "continue", Vec::new());

        assert_eq!(input.history[0].name.as_deref(), Some("assistant-name"));
        assert_eq!(input.history[0].tool_calls.len(), 2);
        assert_eq!(
            input.history[0].tool_calls[0].arguments,
            serde_json::json!({ "q": "axocoatl" })
        );
        assert_eq!(
            input.history[0].tool_calls[0]
                .provider_metadata
                .get("gemini.thought_signature")
                .map(String::as_str),
            Some("exact-signature")
        );
        assert_eq!(
            input.history[0].tool_calls[1].arguments,
            serde_json::Value::Null
        );
        assert_eq!(input.history[1].name.as_deref(), Some("search"));
        assert_eq!(input.history[1].tool_call_id.as_deref(), Some("call-valid"));
    }

    #[test]
    fn chat_agent_input_carries_chat_overrides_and_turn_attachments() {
        let mut chat = persisted_chat(Vec::new());
        chat.system_override = Some("Use the chat rules".to_string());
        chat.model_override = Some("qwen3:32b".to_string());
        let attachment = axocoatl_core::AgentAttachment {
            id: "sha256".to_string(),
            name: "spec.md".to_string(),
            mime: "text/markdown".to_string(),
            bytes: b"the spec".to_vec(),
            size: 42,
            extracted_text: Some("the spec".to_string()),
        };

        let input = build_chat_agent_input(&chat, "review this", vec![attachment]);

        assert_eq!(input.system_override.as_deref(), Some("Use the chat rules"));
        assert_eq!(input.model_override.as_deref(), Some("qwen3:32b"));
        assert_eq!(input.attachments.len(), 1);
        assert_eq!(input.attachments[0].id, "sha256");
        assert_eq!(input.attachments[0].name, "spec.md");
        assert_eq!(input.attachments[0].mime, "text/markdown");
        assert_eq!(input.attachments[0].bytes, b"the spec");
        assert_eq!(input.attachments[0].size, 42);
        assert_eq!(
            input.attachments[0].extracted_text.as_deref(),
            Some("the spec")
        );
    }

    #[test]
    fn chat_agent_input_empty_chat_still_selects_supplied_history_mode() {
        let chat = persisted_chat(Vec::new());

        let input = build_chat_agent_input(&chat, "first turn", Vec::new());

        assert!(input.history.is_empty());
        assert_eq!(
            input.conversation_mode,
            axocoatl_core::ConversationMode::SuppliedHistory
        );
    }

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

    #[test]
    fn workspace_route_contract_separates_open_workspace_from_new_session() {
        let open: CreateWorkspaceBody =
            serde_json::from_str(r#"{"path":"/projects/serpent-slider","name":"Serpent Slider"}"#)
                .expect("Open Workspace accepts a path and optional durable label");
        assert_eq!(open.path, "/projects/serpent-slider");
        assert_eq!(open.name.as_deref(), Some("Serpent Slider"));

        let reopen: CreateWorkspaceBody =
            serde_json::from_str(r#"{"path":"/projects/serpent-slider"}"#)
                .expect("omitting name preserves an existing custom label");
        assert!(reopen.name.is_none());

        let session: CreateWorkspaceSessionBody = serde_json::from_str(
            r#"{"name":"Tune movement","mode":{"kind":"single_agent","agent_id":"coder"}}"#,
        )
        .expect("Workspace-owned Session creation has no directory choice");
        assert_eq!(session.name, "Tune movement");
        assert!(matches!(
            session.mode,
            axocoatl_session::SessionMode::SingleAgent { ref agent_id }
                if agent_id == "coder"
        ));
        assert!(serde_json::from_str::<CreateWorkspaceSessionBody>(
            r#"{"name":"Wrong owner","working_dir":"/tmp/other","mode":{"kind":"single_agent","agent_id":"coder"}}"#
        )
        .is_err());

        let legacy: CreateSessionBody = serde_json::from_str(
            r#"{"name":"Compatibility client","working_dir":"/tmp/project","mode":{"kind":"single_agent","agent_id":"coder"}}"#,
        )
        .expect("the legacy path-owned request remains accepted");
        assert_eq!(legacy.working_dir, "/tmp/project");
    }

    #[test]
    fn preview_proxy_uses_strict_virtual_origin_and_resolved_transport() {
        const BROWSER_JS: &str = include_str!("../static/ui/browser.js");
        const AXO_TAP_JS: &str = include_str!("../static/axo-tap.js");

        assert_eq!(
            session_preview_upstream(43117, "assets/app.js", Some("v=2")),
            "http://127.0.0.1:43117/assets/app.js?v=2"
        );
        assert_eq!(
            session_preview_websocket_upstream(43117, "@vite/client", Some("token=2")),
            "ws://127.0.0.1:43117/@vite/client?token=2"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            "ses-123e4567-e89b-12d3-a456-426614174000-p5173.localhost:18080"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            preview_target_from_headers(&headers),
            Ok(Some(PreviewHostTarget {
                session_id: "ses-123e4567-e89b-12d3-a456-426614174000".into(),
                logical_port: 5173,
            }))
        );
        headers.insert(header::HOST, "ses-123-p0.localhost:18080".parse().unwrap());
        assert_eq!(preview_target_from_headers(&headers), Err(()));
        headers.insert(header::HOST, "attacker.localhost:18080".parse().unwrap());
        assert_eq!(preview_target_from_headers(&headers), Err(()));
        headers.insert(
            header::HOST,
            "ses-123-p5173.preview.localhost:18080".parse().unwrap(),
        );
        assert_eq!(preview_target_from_headers(&headers), Err(()));

        headers.insert(
            header::HOST,
            "ses-123-p5173.localhost:18080".parse().unwrap(),
        );
        assert_eq!(
            preview_http_upstream_host(&PreviewProxyMode::VirtualHost, &headers, 5173)
                .and_then(|value| value.to_str().ok().map(str::to_string)),
            Some("ses-123-p5173.localhost:18080".to_string())
        );
        assert_eq!(
            preview_http_upstream_host(
                &PreviewProxyMode::LegacyOpaque {
                    base: "/legacy/".into()
                },
                &headers,
                5173,
            )
            .and_then(|value| value.to_str().ok().map(str::to_string)),
            Some("localhost:5173".to_string())
        );
        assert!(!request_header_is_forwardable(
            &header::ACCEPT_ENCODING,
            &PreviewProxyMode::VirtualHost
        ));
        assert!(request_header_is_forwardable(
            &header::AUTHORIZATION,
            &PreviewProxyMode::VirtualHost
        ));
        assert!(request_header_is_forwardable(
            &header::HeaderName::from_static("x-api-key"),
            &PreviewProxyMode::VirtualHost
        ));
        let legacy = PreviewProxyMode::LegacyOpaque {
            base: "/legacy/".into(),
        };
        assert!(!request_header_is_forwardable(
            &header::AUTHORIZATION,
            &legacy
        ));
        assert!(!request_header_is_forwardable(
            &header::HeaderName::from_static("x-api-key"),
            &legacy
        ));
        assert!(response_header_is_forwardable(&header::CONTENT_ENCODING));
        assert!(preview_response_needs_html_injection(
            "text/html; charset=utf-8"
        ));
        assert!(!preview_response_needs_html_injection("text/event-stream"));
        assert!(!preview_response_needs_html_injection(
            "application/octet-stream"
        ));
        assert!(!preview_response_allows_body(&Method::HEAD, StatusCode::OK));
        assert!(!preview_response_allows_body(
            &Method::GET,
            StatusCode::NO_CONTENT
        ));
        assert!(!preview_response_allows_body(
            &Method::GET,
            StatusCode::NOT_MODIFIED
        ));
        assert!(preview_response_allows_body(&Method::GET, StatusCode::OK));
        assert!(!preview_html_size_exceeds(MAX_PREVIEW_HTML_BODY - 1, 1));
        assert!(preview_html_size_exceeds(MAX_PREVIEW_HTML_BODY, 1));

        assert!(LEGACY_PREVIEW_SANDBOX_CSP.starts_with("sandbox "));
        assert!(!LEGACY_PREVIEW_SANDBOX_CSP.contains("allow-same-origin"));
        assert!(BROWSER_JS.contains(
            "f.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-forms allow-popups allow-modals')"
        ));
        assert!(BROWSER_JS.contains("-p${port}.localhost"));
        assert!(!BROWSER_JS.contains(".preview.localhost"));
        assert!(BROWSER_JS.contains("data-act=\"open-full\""));
        assert!(BROWSER_JS.contains("target=\"_blank\" rel=\"noopener noreferrer\""));
        assert!(BROWSER_JS.contains("this.#openFull.href = fullPreviewUrl"));
        assert!(!BROWSER_JS.contains("window.open("));
        assert!(DASHBOARD_HTML.contains("!url.hostname.endsWith('.localhost')"));
        assert!(DASHBOARD_HTML.contains("e.origin !== expectedOrigin"));
        assert!(DASHBOARD_HTML.contains("creation?.discovered_ids"));
        assert!(AXO_TAP_JS.contains("e.source !== parent || e.origin !== parentOrigin"));
    }

    #[test]
    fn dashboard_canonicalizes_only_loopback_ip_literals_and_preserves_query() {
        let uri: Uri = "/?session=ses-123&review=preview".parse().unwrap();
        for (host, expected) in [
            (
                "127.0.0.1:18080",
                Some("http://localhost:18080/?session=ses-123&review=preview"),
            ),
            (
                "127.42.3.9:8080",
                Some("http://localhost:8080/?session=ses-123&review=preview"),
            ),
            (
                "[::1]:18080",
                Some("http://localhost:18080/?session=ses-123&review=preview"),
            ),
            ("localhost:18080", None),
            ("operator.example:18080", None),
            ("192.0.2.10:18080", None),
            ("0.0.0.0:18080", None),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert_eq!(
                canonical_workbench_location(&headers, &uri)
                    .and_then(|value| value.to_str().ok().map(str::to_string))
                    .as_deref(),
                expected,
                "unexpected dashboard canonicalization for {host}",
            );
        }
    }

    #[tokio::test]
    async fn preview_upstream_observes_app_credentials_only_on_virtual_hosts() {
        async fn capture(mode: PreviewProxyMode) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind upstream header observer");
            let address = listener.local_addr().expect("upstream observer address");
            let observer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept proxy request");
                let mut received = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = socket.read(&mut chunk).await.expect("read proxy request");
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&chunk[..read]);
                    if received.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("respond to proxy request");
                String::from_utf8(received).expect("HTTP request headers are UTF-8")
            });

            let mut headers = HeaderMap::new();
            headers.insert(
                header::HOST,
                "ses-123-p5173.localhost:18080".parse().unwrap(),
            );
            headers.insert(header::AUTHORIZATION, "Bearer app-token".parse().unwrap());
            headers.insert("x-api-key", "app-key".parse().unwrap());
            headers.insert(
                header::PROXY_AUTHORIZATION,
                "Bearer control-token".parse().unwrap(),
            );
            headers.insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());

            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build test proxy client");
            let response = preview_upstream_request(
                &client,
                reqwest::Method::POST,
                &format!("http://{address}/echo"),
                &headers,
                &mode,
                5173,
            )
            .body("probe")
            .send()
            .await
            .expect("send proxy request to observer");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            observer.await.expect("upstream observer joins")
        }

        let virtual_request = capture(PreviewProxyMode::VirtualHost).await.to_lowercase();
        assert!(virtual_request.contains("host: ses-123-p5173.localhost:18080\r\n"));
        assert!(virtual_request.contains("authorization: bearer app-token\r\n"));
        assert!(virtual_request.contains("x-api-key: app-key\r\n"));
        assert!(virtual_request.contains("accept-encoding: identity\r\n"));
        assert!(!virtual_request.contains("proxy-authorization:"));

        let legacy_request = capture(PreviewProxyMode::LegacyOpaque {
            base: "/legacy/".into(),
        })
        .await
        .to_lowercase();
        assert!(legacy_request.contains("host: localhost:5173\r\n"));
        assert!(!legacy_request.contains("authorization:"));
        assert!(!legacy_request.contains("x-api-key:"));
        assert!(legacy_request.contains("accept-encoding: identity\r\n"));
        assert!(!legacy_request.contains("proxy-authorization:"));
    }

    #[tokio::test]
    async fn preview_http_proxy_transports_real_requests_and_responses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
            let mut received = Vec::new();
            let mut header_end = None;
            let mut content_length = 0;
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.expect("read proxy request");
                assert!(read > 0, "proxy request ended before its body was complete");
                received.extend_from_slice(&chunk[..read]);
                if header_end.is_none() {
                    header_end = received
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4);
                    if let Some(end) = header_end {
                        let headers = String::from_utf8_lossy(&received[..end]);
                        content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':').and_then(|(name, value)| {
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                            })
                            .unwrap_or(0);
                    }
                }
                if header_end.is_some_and(|end| received.len() >= end + content_length) {
                    break;
                }
            }
            received
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind real Preview upstream");
        let host_port = listener
            .local_addr()
            .expect("real Preview upstream address")
            .port();
        let upstream = tokio::spawn(async move {
            let (mut html_socket, _) = listener.accept().await.expect("accept HTML request");
            let html_request = read_request(&mut html_socket).await;
            let html = b"<!doctype html><html><body><main>saved</main></body></html>";
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:9999/done?ok=1#saved\r\nSet-Cookie: app_session=alpha; Path=/; HttpOnly\r\nX-Upstream: real-html\r\nContent-Security-Policy: default-src 'none'\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
                html.len()
            );
            html_socket
                .write_all(response.as_bytes())
                .await
                .expect("write HTML response headers");
            html_socket
                .write_all(html)
                .await
                .expect("write HTML response body");

            let (mut binary_socket, _) = listener.accept().await.expect("accept binary request");
            let binary_request = read_request(&mut binary_socket).await;
            binary_socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Encoding: x-preview-test\r\nX-Upstream: real-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write binary response headers");
            binary_socket
                .write_all(b"3\r\n\x00\xffA\r\n4\r\nB\x01CD\r\n0\r\n\r\n")
                .await
                .expect("write chunked binary response");

            (html_request, binary_request)
        });

        let virtual_host = "ses-transport-p5173.localhost:18080";
        let request = axum::http::Request::builder()
            .method(Method::PATCH)
            .uri("/forms/save?mode=full&encoded=a%2Fb")
            .header(header::HOST, virtual_host)
            .header(header::ORIGIN, format!("http://{virtual_host}"))
            .header(header::AUTHORIZATION, "Bearer app-token")
            .header("x-api-key", "app-key")
            .header("x-client-probe", "transport-test")
            .header(header::PROXY_AUTHORIZATION, "Bearer control-token")
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(axum::body::Body::from(r#"{"move":"north"}"#))
            .expect("build Preview HTML request");
        let target = preview_target_from_headers(request.headers())
            .expect("virtual Preview Host is valid")
            .expect("virtual Preview Host resolves a target");
        let html_response = proxy_preview_http_to_port(
            target.clone(),
            "forms/save".to_string(),
            request,
            PreviewProxyMode::VirtualHost,
            host_port,
        )
        .await;
        assert_eq!(html_response.status(), StatusCode::CREATED);
        assert_eq!(
            html_response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some("/done?ok=1#saved")
        );
        assert_eq!(
            html_response
                .headers()
                .get(header::SET_COOKIE)
                .and_then(|value| value.to_str().ok()),
            Some("app_session=alpha; Path=/; HttpOnly")
        );
        assert_eq!(
            html_response
                .headers()
                .get("x-upstream")
                .and_then(|value| value.to_str().ok()),
            Some("real-html")
        );
        assert_eq!(
            html_response
                .headers()
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
        assert!(!html_response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY));
        assert!(!html_response.headers().contains_key("x-frame-options"));
        let html_body = axum::body::to_bytes(html_response.into_body(), MAX_PREVIEW_HTML_BODY)
            .await
            .expect("collect injected HTML response");
        let html_body = String::from_utf8(html_body.to_vec()).expect("HTML remains UTF-8");
        assert_eq!(
            html_body,
            "<!doctype html><html><body><main>saved</main><script src=\"/.axocoatl/preview-picker.js\"></script></body></html>"
        );

        let binary_request = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/assets/blob.bin?download=1")
            .header(header::HOST, virtual_host)
            .body(axum::body::Body::empty())
            .expect("build Preview binary request");
        let binary_response = proxy_preview_http_to_port(
            target,
            "assets/blob.bin".to_string(),
            binary_request,
            PreviewProxyMode::VirtualHost,
            host_port,
        )
        .await;
        assert_eq!(binary_response.status(), StatusCode::OK);
        assert_eq!(
            binary_response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("x-preview-test")
        );
        assert_eq!(
            binary_response
                .headers()
                .get("x-upstream")
                .and_then(|value| value.to_str().ok()),
            Some("real-stream")
        );
        assert!(!binary_response
            .headers()
            .contains_key(header::TRANSFER_ENCODING));
        let binary_body = axum::body::to_bytes(binary_response.into_body(), 64)
            .await
            .expect("collect streamed binary response");
        assert_eq!(binary_body.as_ref(), b"\x00\xffAB\x01CD");

        let (html_request, binary_request) = upstream.await.expect("real upstream joins");
        let header_end = html_request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTML request has header terminator")
            + 4;
        let html_headers = String::from_utf8_lossy(&html_request[..header_end]).to_lowercase();
        assert!(html_headers.starts_with("patch /forms/save?mode=full&encoded=a%2fb http/1.1\r\n"));
        assert!(html_headers.contains(&format!("host: {virtual_host}\r\n")));
        assert!(html_headers.contains("authorization: bearer app-token\r\n"));
        assert!(html_headers.contains("x-api-key: app-key\r\n"));
        assert!(html_headers.contains("x-client-probe: transport-test\r\n"));
        assert!(html_headers.contains("accept-encoding: identity\r\n"));
        assert!(!html_headers.contains("proxy-authorization:"));
        assert_eq!(&html_request[header_end..], br#"{"move":"north"}"#);

        let binary_headers = String::from_utf8(binary_request)
            .expect("binary request headers are UTF-8")
            .to_lowercase();
        assert!(binary_headers.starts_with("get /assets/blob.bin?download=1 http/1.1\r\n"));
        assert!(binary_headers.contains(&format!("host: {virtual_host}\r\n")));
        assert!(binary_headers.contains("accept-encoding: identity\r\n"));
    }

    #[tokio::test]
    // Tungstenite fixes the handshake callback's error type to a full HTTP
    // response; this test needs that callback to inspect and select protocols.
    #[allow(clippy::result_large_err)]
    async fn preview_websocket_proxy_bridges_real_clients_and_upstream() {
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind real Preview WebSocket upstream");
        let upstream_port = upstream_listener
            .local_addr()
            .expect("real WebSocket upstream address")
            .port();
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (socket, _) = upstream_listener
                .accept()
                .await
                .expect("accept proxied WebSocket");
            let mut observed_tx = Some(observed_tx);
            let mut socket = tokio_tungstenite::accept_hdr_async(
                socket,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    let observed = (
                        request.uri().to_string(),
                        request
                            .headers()
                            .get(header::HOST)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                        request
                            .headers()
                            .get(header::ORIGIN)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                        request
                            .headers()
                            .get(header::SEC_WEBSOCKET_PROTOCOL)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                    );
                    observed_tx
                        .take()
                        .expect("handshake callback runs once")
                        .send(observed)
                        .expect("Preview test still observes the handshake");
                    response.headers_mut().insert(
                        header::SEC_WEBSOCKET_PROTOCOL,
                        HeaderValue::from_static("preview-v1"),
                    );
                    Ok(response)
                },
            )
            .await
            .expect("accept real Preview WebSocket handshake");

            let client_payload = socket
                .next()
                .await
                .expect("client sends payload through bridge")
                .expect("client payload is valid");
            assert_eq!(
                client_payload,
                UpstreamWsMessage::Text("client-to-upstream".into())
            );
            socket
                .send(UpstreamWsMessage::Binary(bytes::Bytes::from_static(
                    b"upstream-to-client",
                )))
                .await
                .expect("send upstream payload through bridge");
            let closing = socket
                .next()
                .await
                .expect("client closes proxied WebSocket")
                .expect("close frame is valid");
            assert!(matches!(closing, UpstreamWsMessage::Close(_)));
        });

        let downstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind downstream Preview WebSocket listener");
        let downstream_address = downstream_listener
            .local_addr()
            .expect("downstream Preview WebSocket address");
        let app = axum::Router::new().fallback(
            move |request: axum::http::Request<axum::body::Body>| async move {
                let target = match preview_target_from_headers(request.headers()) {
                    Ok(Some(target)) => target,
                    _ => return StatusCode::MISDIRECTED_REQUEST.into_response(),
                };
                if !is_websocket_upgrade(request.headers())
                    || preview_request_has_disallowed_origin(
                        request.method(),
                        true,
                        request.headers(),
                    )
                {
                    return StatusCode::FORBIDDEN.into_response();
                }
                preview_websocket_proxy_to_port(target, request, upstream_port).await
            },
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let downstream = tokio::spawn(async move {
            axum::serve(downstream_listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve downstream Preview WebSocket");
        });

        let virtual_host = format!(
            "ses-websocket-p5173.localhost:{}",
            downstream_address.port()
        );
        let virtual_origin = format!("http://{virtual_host}");
        let mut request = format!("ws://{downstream_address}/hmr?token=preview")
            .into_client_request()
            .expect("build downstream WebSocket request");
        request.headers_mut().insert(
            header::HOST,
            HeaderValue::from_str(&virtual_host).expect("virtual Host is valid"),
        );
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&virtual_origin).expect("virtual Origin is valid"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("preview-v1, preview-v2"),
        );
        let (mut client, response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect through real Preview WebSocket proxy");
        assert_eq!(
            response
                .headers()
                .get(header::SEC_WEBSOCKET_PROTOCOL)
                .and_then(|value| value.to_str().ok()),
            Some("preview-v1")
        );
        client
            .send(UpstreamWsMessage::Text("client-to-upstream".into()))
            .await
            .expect("send downstream payload");
        let upstream_payload = client
            .next()
            .await
            .expect("upstream sends payload through bridge")
            .expect("upstream payload is valid");
        assert_eq!(
            upstream_payload,
            UpstreamWsMessage::Binary(bytes::Bytes::from_static(b"upstream-to-client"))
        );
        client.close(None).await.expect("close downstream client");

        let observed = observed_rx.await.expect("observe upstream handshake");
        assert_eq!(observed.0, "/hmr?token=preview");
        assert_eq!(observed.1.as_deref(), Some(virtual_host.as_str()));
        assert_eq!(observed.2.as_deref(), Some(virtual_origin.as_str()));
        assert_eq!(observed.3.as_deref(), Some("preview-v1, preview-v2"));

        upstream.await.expect("real WebSocket upstream joins");
        shutdown_tx
            .send(())
            .expect("downstream Preview server is still running");
        downstream
            .await
            .expect("downstream Preview WebSocket server joins");
    }

    #[test]
    fn preview_origins_cannot_write_across_session_or_port_boundaries() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "ses-b-p5173.localhost:18080".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://ses-a-p5173.localhost:18080".parse().unwrap(),
        );
        assert!(preview_request_has_disallowed_origin(
            &Method::POST,
            false,
            &headers
        ));
        assert!(preview_request_has_disallowed_origin(
            &Method::GET,
            true,
            &headers
        ));

        headers.insert(
            header::ORIGIN,
            "http://ses-b-p5173.localhost:18080".parse().unwrap(),
        );
        assert!(!preview_request_has_disallowed_origin(
            &Method::POST,
            false,
            &headers
        ));
    }

    #[test]
    fn global_file_delete_guard_keeps_inactive_historical_session_blob_pinned() {
        let dir = std::env::temp_dir().join(format!(
            "axocoatl-server-session-attachment-test-{}",
            uuid::Uuid::new_v4()
        ));
        let mut files =
            axocoatl_memory::files::FileStore::new(dir.join("files")).expect("open blob store");
        let entry = files
            .store_with(b"history", "history.txt", "text/plain", |_, _| (None, None))
            .expect("store blob");
        let mut references = axocoatl_session::SessionAttachmentStore::open(dir.join("relations"))
            .expect("open relation store");
        references
            .create(axocoatl_session::CreateSessionAttachmentRef {
                reference_id: Some("ctx-history".to_string()),
                session_id: "ses-a".to_string(),
                blob_id: format!("sha256:{}", entry.id),
                display_name: "history.txt".to_string(),
                declared_mime: Some("text/plain".to_string()),
                size: entry.size,
                scope: axocoatl_session::TurnContextScope::Session,
                extraction: Default::default(),
                metadata: serde_json::Map::new(),
            })
            .expect("create relation");
        references
            .mark_consumed("ses-a", &["ctx-history".to_string()], "turn-a")
            .expect("record historical use");
        references
            .deactivate("ses-a", "ctx-history")
            .expect("deactivate relation");

        assert!(references.list("ses-a").is_empty());
        let historical = references
            .get("ctx-history")
            .expect("historical relation remains addressable");
        assert_eq!(
            read_session_attachment_bytes(&files, &historical).expect("historical content"),
            b"history"
        );
        assert!(session_relations_retain_blob(&references, &entry.id));

        drop(references);
        std::fs::remove_dir_all(&dir).expect("remove owned test directory");
    }

    #[test]
    fn agent_patch_can_explicitly_clear_a_token_budget() {
        let patch: AgentPatch =
            serde_json::from_str(r#"{"clear_token_budget":true,"restart_now":false}"#).unwrap();
        let mut budget = Some(axocoatl_config::TokenBudgetYaml {
            per_call: 2048,
            per_execution: 8192,
            overflow_policy: axocoatl_config::OverflowPolicyYaml::Abort,
        });

        apply_token_budget_patch(&mut budget, &patch).unwrap();

        assert!(budget.is_none());
        assert!(!patch.restart_now.unwrap());
    }

    #[test]
    fn runtime_cleanup_body_supports_high_friction_creation_token_confirmation() {
        let body: ConfirmSessionRuntimeCleanupBody = serde_json::from_str(
            r#"{
                "creation_token":"session:generation:nonce",
                "confirmed_all_matching_sandboxes_deleted":true
            }"#,
        )
        .expect("creation-token cleanup payload should deserialize");
        assert!(body.runtime_id.is_none());
        assert_eq!(
            body.creation_token.as_deref(),
            Some("session:generation:nonce")
        );
        assert!(!body.confirmed);
        assert!(body.confirmed_all_matching_sandboxes_deleted);

        let legacy: ConfirmSessionRuntimeCleanupBody =
            serde_json::from_str(r#"{"runtime_id":"sandbox-exact","confirmed":true}"#)
                .expect("exact-runtime cleanup payload remains compatible");
        assert_eq!(legacy.runtime_id.as_deref(), Some("sandbox-exact"));
        assert!(legacy.creation_token.is_none());
        assert!(legacy.confirmed);
        assert!(!legacy.confirmed_all_matching_sandboxes_deleted);
    }

    #[test]
    fn agent_patch_rejects_ambiguous_budget_clear_and_values() {
        let patch: AgentPatch =
            serde_json::from_str(r#"{"clear_token_budget":true,"per_call_budget":1024}"#).unwrap();
        let mut budget = Some(axocoatl_config::TokenBudgetYaml {
            per_call: 2048,
            per_execution: 8192,
            overflow_policy: axocoatl_config::OverflowPolicyYaml::Abort,
        });

        let error = apply_token_budget_patch(&mut budget, &patch).unwrap_err();

        assert!(error.contains("cannot be combined"));
        let unchanged = budget.expect("a rejected patch must leave the budget intact");
        assert_eq!(unchanged.per_call, 2048);
        assert_eq!(unchanged.per_execution, 8192);
    }

    #[test]
    fn agent_patch_configures_budget_and_validates_policy() {
        let patch: AgentPatch = serde_json::from_str(
            r#"{"per_call_budget":1024,"per_execution_budget":4096,"overflow_policy":"abort"}"#,
        )
        .unwrap();
        let mut budget = None;
        apply_token_budget_patch(&mut budget, &patch).unwrap();
        let configured = budget.as_ref().expect("budget should be configured");
        assert_eq!(configured.per_call, 1024);
        assert_eq!(configured.per_execution, 4096);
        assert!(matches!(
            configured.overflow_policy,
            axocoatl_config::OverflowPolicyYaml::Abort
        ));

        let invalid: AgentPatch = serde_json::from_str(r#"{"overflow_policy":"ignore"}"#).unwrap();
        let error = apply_token_budget_patch(&mut budget, &invalid).unwrap_err();
        assert!(error.contains("Unknown overflow policy"));
    }

    #[test]
    fn variants_body_defaults_task_to_input_and_resolves_lane_count() {
        let body: VariantsBody = serde_json::from_str(r#"{"input":"implement it","n":2}"#)
            .expect("minimal variants body should deserialize");

        let (task, input, lanes) = body.into_attempt_run();

        assert_eq!(task, "implement it");
        assert_eq!(input, "implement it");
        assert_eq!(lanes.len(), 2);

        let body: VariantsBody = serde_json::from_str(
            r#"{"task":"original task","input":"reviewed plan","lanes":[{"agent":"reviewer","model":"qwen3:32b"}]}"#,
        )
        .expect("planned attempt body should deserialize");
        let (task, input, lanes) = body.into_attempt_run();
        assert_eq!(task, "original task");
        assert_eq!(input, "reviewed plan");
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].agent.as_deref(), Some("reviewer"));
        assert_eq!(lanes[0].model.as_deref(), Some("qwen3:32b"));
    }

    #[tokio::test]
    async fn request_owned_mutation_survives_waiter_cancellation() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = run_request_owned("test mutation", async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                let _ = finished_tx.send(());
            })
            .await;
        });

        started_rx.await.expect("owned mutation started");
        waiter.abort();
        assert!(waiter
            .await
            .expect_err("waiter was cancelled")
            .is_cancelled());
        release_tx.send(()).expect("release owned mutation");
        tokio::time::timeout(std::time::Duration::from_secs(1), finished_rx)
            .await
            .expect("owned mutation must finish after its waiter disappears")
            .expect("owned mutation reported completion");
    }

    #[test]
    fn attempt_set_queries_require_identity_and_keep_route_fields() {
        let uri: axum::http::Uri = "/api/sessions/s/variants/status?attempt_set_id=set-1"
            .parse()
            .unwrap();
        let Query(status) = Query::<AttemptSetQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(status.attempt_set_id, "set-1");

        let missing: axum::http::Uri = "/api/sessions/s/variants/status".parse().unwrap();
        assert!(Query::<AttemptSetQuery>::try_from_uri(&missing).is_err());

        let uri: axum::http::Uri =
            "/api/sessions/s/variants/diff?attempt_set_id=set-1&index=2&path=src%2Flib.rs"
                .parse()
                .unwrap();
        let Query(diff) = Query::<VariantDiffQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(diff.attempt_set_id, "set-1");
        assert_eq!(diff.index, 2);
        assert_eq!(diff.path, "src/lib.rs");

        let uri: axum::http::Uri =
            "/api/sessions/s/variants/trajectories?attempt_set_id=set-1&baseline=3"
                .parse()
                .unwrap();
        let Query(trajectories) = Query::<TrajectoryQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(trajectories.attempt_set_id, "set-1");
        assert_eq!(trajectories.baseline, 3);

        let uri: axum::http::Uri = "/api/sessions/s/variants/cost?attempt_set_id=set-1&baseline=gpt-5&baseline_provider=openai"
            .parse()
            .unwrap();
        let Query(cost) = Query::<CostQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(cost.attempt_set_id, "set-1");
        assert_eq!(cost.baseline, "gpt-5");
        assert_eq!(cost.baseline_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn attempt_action_bodies_carry_attempt_set_identity() {
        let verify: VerifyBody =
            serde_json::from_str(r#"{"attempt_set_id":"set-1","check":"cargo test"}"#).unwrap();
        assert_eq!(verify.attempt_set_id, "set-1");
        assert_eq!(verify.check, "cargo test");
        assert!(serde_json::from_str::<VerifyBody>(r#"{"check":"cargo test"}"#).is_err());

        let judge: JudgeBody =
            serde_json::from_str(r#"{"attempt_set_id":"set-1","agent_id":"reviewer"}"#).unwrap();
        assert_eq!(judge.attempt_set_id, "set-1");
        assert_eq!(judge.agent_id, "reviewer");

        let plan: PlanBody =
            serde_json::from_str(r#"{"task":"fix it","agent_id":"planner"}"#).unwrap();
        assert_eq!(plan.task, "fix it");
        assert_eq!(plan.agent_id, "planner");

        let keep: AdoptBody =
            serde_json::from_str(r#"{"attempt_set_id":"set-1","index":2}"#).unwrap();
        assert_eq!(keep.attempt_set_id, "set-1");
        assert_eq!(keep.index, 2);

        let discard: DiscardAttemptBody =
            serde_json::from_str(r#"{"attempt_set_id":"set-1"}"#).unwrap();
        assert_eq!(discard.attempt_set_id, "set-1");
    }

    #[test]
    fn attempt_conflicts_map_to_http_409_and_other_errors_stay_400() {
        let (status, Json(body)) = attempt_err(axocoatl_daemon::DaemonError::AttemptConflict(
            "keep or discard first".to_string(),
        ));
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.error.contains("keep or discard first"));

        let (status, Json(body)) = attempt_err(axocoatl_daemon::DaemonError::SessionConflict(
            "attachment is part of durable history".to_string(),
        ));
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.error.contains("durable history"));

        let (status, Json(body)) = attempt_err(axocoatl_daemon::DaemonError::Session(
            "unknown attempt".to_string(),
        ));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.error.contains("unknown attempt"));
    }

    /// A commit with no `stage_all` must commit the index as the user built it.
    ///
    /// This is the whole point of staging a file or a hunk, and it used to be
    /// the other way round: `git_commit` always ran `add -A` first, so every
    /// staging decision was discarded at the moment it was meant to take
    /// effect. Flipping this default silently restores that, which is why the
    /// default is asserted rather than left to the reader of the struct.
    #[test]
    fn commit_body_defaults_to_committing_only_the_index() {
        let body: GitCommitBody = serde_json::from_str(r#"{"message":"only staged"}"#).unwrap();
        assert!(
            !body.stage_all,
            "an absent stage_all must mean 'commit the index', not 'stage everything'"
        );

        let explicit: GitCommitBody =
            serde_json::from_str(r#"{"message":"everything","stage_all":true}"#).unwrap();
        assert!(explicit.stage_all);

        // A commit with no message at all still defaults the same way.
        let bare: GitCommitBody = serde_json::from_str("{}").unwrap();
        assert!(!bare.stage_all);
        assert!(bare.message.is_none());
    }
}
