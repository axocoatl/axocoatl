use serde::{Deserialize, Serialize};

use crate::secret::SecretString;

/// Root configuration — parses axocoatl.yaml.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AxocoatlConfig {
    #[serde(default)]
    pub agents: Vec<AgentConfigYaml>,
    #[serde(default)]
    pub workflows: Vec<WorkflowConfigYaml>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfigYaml>,
    #[serde(default)]
    pub providers: ProvidersConfigYaml,
    #[serde(default)]
    pub server: ServerConfigYaml,
    #[serde(default)]
    pub sandbox: SandboxConfigYaml,
    /// **Experimental — not yet active.** User-defined hooks are parsed but not
    /// executed at runtime; only the built-in MCP tool-approval hook runs. A
    /// non-empty `hooks:` section logs a warning at daemon startup. See
    /// `HookConfigYaml`.
    #[serde(default)]
    pub hooks: Vec<HookConfigYaml>,
    #[serde(default)]
    pub skills: Vec<SkillConfigYaml>,
    #[serde(default)]
    pub schedules: Vec<ScheduleConfigYaml>,
    #[serde(default)]
    pub proactive: Vec<ProactiveConfigYaml>,
    #[serde(default)]
    pub web_search: Option<WebSearchConfigYaml>,
    /// The `web_fetch` tool. Present enables it for Agents whose tools list it.
    #[serde(default)]
    pub web_fetch: Option<WebFetchConfigYaml>,
    /// The `browser` tool. Present enables it for Agents whose tools list it.
    #[serde(default)]
    pub browser: Option<BrowserConfigYaml>,
    #[serde(default)]
    pub consolidation: ConsolidationConfigYaml,
    #[serde(default)]
    pub webhooks: Vec<WebhookConfigYaml>,
    /// Model prices, in dollars per million tokens, used to report what a
    /// variants run cost against what it would have cost on one expensive model.
    /// Models absent from this map have an unknown price. The daemon separately
    /// recognizes Ollama at a configured loopback endpoint as having a known-zero
    /// model API charge, so an unpriced remote model—including non-loopback
    /// Ollama—is never misreported as costing $0.
    #[serde(default)]
    pub pricing: std::collections::HashMap<String, ModelPriceYaml>,
    /// Named credentials that `sandbox.egress.routes` add to requests. Each
    /// names where the daemon reads the value when a request needs it: an
    /// environment variable of the daemon or an owner-only file. The config
    /// never holds a credential value.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub credentials: std::collections::BTreeMap<String, CredentialSourceYaml>,
}

/// Price of one model, in dollars per million tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct ModelPriceYaml {
    #[serde(default)]
    pub input_per_mtok: f64,
    #[serde(default)]
    pub output_per_mtok: f64,
}

/// Background "sleep-time" memory consolidation: idle agents promote durable
/// facts from semantic memory (Tier 4) into their curated core-memory blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfigYaml {
    #[serde(default = "default_consolidation_enabled")]
    pub enabled: bool,
    /// An agent must have been idle at least this long before a pass runs.
    #[serde(default = "default_idle_threshold")]
    pub idle_threshold_secs: u64,
    /// Minimum time between consolidation passes for a single agent.
    #[serde(default = "default_consolidation_interval")]
    pub interval_secs: u64,
}

impl Default for ConsolidationConfigYaml {
    fn default() -> Self {
        Self {
            enabled: default_consolidation_enabled(),
            idle_threshold_secs: default_idle_threshold(),
            interval_secs: default_consolidation_interval(),
        }
    }
}

fn default_consolidation_enabled() -> bool {
    true
}
fn default_idle_threshold() -> u64 {
    120
}
fn default_consolidation_interval() -> u64 {
    1800
}

/// Web-search provider for session agents. When present, the `web_search`
/// tool is offered to a session's agents.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSearchConfigYaml {
    /// Provider name: `"searxng"`, or the legacy `"tavily"`.
    #[serde(default)]
    pub provider: String,
    /// Provider API key (legacy `tavily` only).
    #[serde(default)]
    pub api_key: SecretString,
    /// SearXNG settings for `provider: searxng`.
    #[serde(default)]
    pub searxng: Option<SearxngConfigYaml>,
}

/// A SearXNG instance, either run by Axocoatl (`managed: true`) or an existing
/// one at `url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearxngConfigYaml {
    /// Axocoatl runs and stops the SearXNG container itself.
    #[serde(default = "default_true")]
    pub managed: bool,
    /// Container image for a managed instance. Defaults to the pinned image.
    #[serde(default)]
    pub image: Option<String>,
    /// Base URL of an unmanaged instance. Required exactly when `managed` is false.
    #[serde(default)]
    pub url: Option<String>,
    /// Keep only these engines. Empty keeps SearXNG's defaults.
    #[serde(default)]
    pub engines: Vec<String>,
    #[serde(default = "default_searxng_language")]
    pub language: String,
    /// 0 (off), 1 (moderate) or 2 (strict).
    #[serde(default)]
    pub safesearch: u8,
    /// 1-60 seconds.
    #[serde(default = "default_searxng_timeout")]
    pub timeout_secs: u64,
}

impl Default for SearxngConfigYaml {
    fn default() -> Self {
        Self {
            managed: true,
            image: None,
            url: None,
            engines: Vec::new(),
            language: default_searxng_language(),
            safesearch: 0,
            timeout_secs: default_searxng_timeout(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_searxng_language() -> String {
    "all".to_string()
}
fn default_searxng_timeout() -> u64 {
    15
}

/// The `web_fetch` tool. Its presence enables the tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchConfigYaml {
    /// Largest response body read, 64 KiB to 8 MiB.
    #[serde(default = "default_web_fetch_max_bytes")]
    pub max_bytes: u64,
    /// 1-60 seconds.
    #[serde(default = "default_web_fetch_timeout")]
    pub timeout_secs: u64,
}

impl Default for WebFetchConfigYaml {
    fn default() -> Self {
        Self {
            max_bytes: default_web_fetch_max_bytes(),
            timeout_secs: default_web_fetch_timeout(),
        }
    }
}

fn default_web_fetch_max_bytes() -> u64 {
    4 * 1024 * 1024
}
fn default_web_fetch_timeout() -> u64 {
    20
}

/// The `browser` tool. Its presence enables the tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfigYaml {
    /// Browser image. Defaults to `localhost/axocoatl-browser:pw1.60.0`.
    #[serde(default)]
    pub image: Option<String>,
    /// Hosts the browser may reach beyond the Session's exposed ports.
    #[serde(default)]
    pub allow: Vec<EgressAllowYaml>,
    /// Private ranges `allow` may reach.
    #[serde(default)]
    pub private_destinations: Vec<String>,
    /// 1024-65536 bytes.
    #[serde(default = "default_browser_snapshot_bytes")]
    pub snapshot_max_bytes: u32,
    /// 10-170 seconds.
    #[serde(default = "default_browser_timeout")]
    pub timeout_secs: u64,
    /// 1-4 browser calls at once per Session.
    #[serde(default = "default_browser_parallel")]
    pub max_parallel: u32,
}

impl Default for BrowserConfigYaml {
    fn default() -> Self {
        Self {
            image: None,
            allow: Vec::new(),
            private_destinations: Vec::new(),
            snapshot_max_bytes: default_browser_snapshot_bytes(),
            timeout_secs: default_browser_timeout(),
            max_parallel: default_browser_parallel(),
        }
    }
}

fn default_browser_snapshot_bytes() -> u32 {
    16_384
}
fn default_browser_timeout() -> u64 {
    120
}
fn default_browser_parallel() -> u32 {
    2
}

/// A **proactive agent** — an agent that acts on its own, with no user prompt,
/// when its trigger fires. This is one half of "Always-On": the Always-On
/// *Service* keeps the daemon process alive; *Proactive Agents* make the
/// agents act autonomously while it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProactiveConfigYaml {
    pub id: String,
    pub name: String,
    /// The agent that runs each time the trigger fires.
    pub agent: String,
    /// What causes this proactive agent to act.
    pub trigger: ProactiveTrigger,
    /// The instruction handed to the agent on each fire.
    #[serde(default)]
    pub input: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// What causes a proactive agent to act.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProactiveTrigger {
    /// Fire on a fixed interval — `"30s"`, `"5m"`, `"2h"`, `"1d"`.
    Schedule { every: String },
    /// Fire whenever an event with this name is published on the event feed —
    /// a name from some Skill's `emits` list.
    OnEvent { event: String },
}

/// A scheduled workflow run. `every` accepts fixed intervals only:
///   "30s", "5m", "2h", "1d" (seconds / minutes / hours / days). Cron
///   expressions are not supported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfigYaml {
    pub id: String,
    pub name: String,
    pub workflow: String,
    pub every: String,
    #[serde(default)]
    pub input: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// An outbound webhook — **event-feed egress**. When the event feed publishes an
/// event whose name matches `events` (or `events` is empty, i.e. every event),
/// Axocoatl sends a signed JSON `POST` to `url`. The daemon publishes a Skill's
/// declared events when the Skill fires. This is the outbound counterpart to
/// inbound A2A: signals leave, opt-in, to systems you own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfigYaml {
    pub name: String,
    pub url: String,
    /// Event names to dispatch — names from Skills' `emits` lists, e.g.
    /// `["ReviewRequested"]`. Empty means every event.
    #[serde(default)]
    pub events: Vec<String>,
    /// Optional shared secret. When set, each delivery is HMAC-SHA256 signed over
    /// the request body and the hex digest is sent in `X-Axocoatl-Signature:
    /// sha256=…`, so the receiver can verify authenticity. Redacted in logs.
    #[serde(default)]
    pub secret: Option<SecretString>,
    /// Static headers added to every request (e.g. an `Authorization` bearer for
    /// an internal endpoint). Values are redacted in logs.
    #[serde(default)]
    pub headers: std::collections::HashMap<String, SecretString>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// A Skill: a named set of events. Firing it (from Settings, the HTTP API or
/// an Agent's `skill_<id>` tool) publishes each `emits` name on the event feed,
/// where On-event and On-skill Automations and webhooks react to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillConfigYaml {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Event names published on the event feed when this Skill fires.
    #[serde(default)]
    pub emits: Vec<String>,
    /// Removed in 1.1.0. Still read so the daemon can warn that it is ignored
    /// instead of dropping a 1.0 key silently; nothing ever reacted to it.
    #[serde(default)]
    pub reacts_to: Vec<String>,
    /// Removed in 1.1.0 with `reacts_to`; read only to warn.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Removed in 1.1.0 with `reacts_to`; read only to warn. Firing a Skill
    /// never ran this prompt.
    #[serde(default)]
    pub prompt: String,
}

impl SkillConfigYaml {
    /// The removed 1.0 keys this Skill still sets, for the startup warning.
    pub fn removed_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if !self.reacts_to.is_empty() {
            keys.push("reacts_to");
        }
        if !self.agents.is_empty() {
            keys.push("agents");
        }
        if !self.prompt.is_empty() {
            keys.push("prompt");
        }
        keys
    }
}

/// Role an agent plays in a multi-agent system.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentRoleYaml {
    /// Standard independent agent.
    #[default]
    Autonomous,
    /// Orchestrator that spawns and manages worker agents.
    Coordinator,
    /// Worker agent spawned by a coordinator.
    Worker,
}

/// Per-agent YAML config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigYaml {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub model: String,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Repository paths this Agent may change. Absent leaves every path open;
    /// `[]` makes it a read-only helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<Vec<String>>,
    pub token_budget: Option<TokenBudgetYaml>,
    #[serde(default)]
    pub memory: MemoryConfigYaml,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub role: AgentRoleYaml,
    /// Removed in 1.1.0. Still read so the daemon can warn that it is ignored
    /// instead of dropping a 1.0 key silently; events never activate Agents.
    #[serde(default)]
    pub activation_threshold: Option<f32>,
    /// Removed in 1.1.0 with `activation_threshold`; read only to warn.
    #[serde(default)]
    pub activation_decay: Option<f32>,
    /// Sampling controls threaded into each LLM request this agent makes.
    #[serde(default)]
    pub sampling: SamplingConfigYaml,
    /// The most tool rounds one activation of this Agent may run, 1 to
    /// 1,024. Absent, a native activation may run as many rounds as its
    /// grant has invocations, up to 1,024.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<u32>,
}

/// Per-agent sampling controls. All optional; an unset field leaves the
/// provider default in place. `response_format` is `"text"` or `"json"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SamplingConfigYaml {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<usize>,
    pub response_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetYaml {
    pub per_execution: usize,
    #[serde(default = "default_per_call")]
    pub per_call: usize,
    #[serde(default)]
    pub overflow_policy: OverflowPolicyYaml,
}

fn default_per_call() -> usize {
    8192
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicyYaml {
    /// Enforce the budget — abort on overflow. The default.
    #[default]
    Abort,
    /// Continue past the budget, logging a warning.
    Warn,
    /// Deprecated: context compaction is now automatic, so `summarize` is no
    /// longer a distinct spend policy. Accepted for backward compatibility and
    /// treated as `warn`.
    Summarize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfigYaml {
    #[serde(default = "default_max_session")]
    pub max_session_messages: usize,
    /// Recall tuning (passive injection + agent-driven recall tools).
    #[serde(default)]
    pub recall: RecallConfigYaml,
    /// Agent-editable core-memory blocks (Tier 3).
    #[serde(default)]
    pub core: CoreMemoryConfigYaml,
}

fn default_max_session() -> usize {
    100
}

/// Core-memory blocks. An empty `blocks` (or an omitted `core`) yields the
/// default set (persona + human + project); a non-empty list replaces it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoreMemoryConfigYaml {
    #[serde(default)]
    pub blocks: Vec<CoreBlockConfigYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreBlockConfigYaml {
    pub label: String,
    #[serde(default)]
    pub value: String,
    #[serde(default = "default_block_limit")]
    pub limit: usize,
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_block_limit() -> usize {
    2000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallConfigYaml {
    #[serde(default = "default_passive_inject")]
    pub passive_inject: bool,
    #[serde(default = "default_recall_top_k")]
    pub top_k: usize,
    #[serde(default = "default_recall_min_score")]
    pub min_score: f32,
}

impl Default for RecallConfigYaml {
    fn default() -> Self {
        Self {
            passive_inject: default_passive_inject(),
            top_k: default_recall_top_k(),
            min_score: default_recall_min_score(),
        }
    }
}

fn default_passive_inject() -> bool {
    true
}
fn default_recall_top_k() -> usize {
    5
}
fn default_recall_min_score() -> f32 {
    0.15
}

/// Workflow configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowConfigYaml {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub agents: Vec<String>,
    pub entry_point: Option<String>,
    /// Removed in 1.1.0. Still read so the daemon can warn that it is ignored
    /// instead of dropping a 1.0 key silently; a Coordinator decomposes with
    /// its model.
    #[serde(default)]
    pub htn_methods_file: Option<String>,
}

/// MCP server connection config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfigYaml {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for stdio servers — typically the API key/token
    /// the server reads on startup (e.g. `BRAVE_API_KEY`).
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    /// Whether a stdio server inherits the daemon's whole environment. With
    /// `false` it gets only `PATH`, `HOME`, `USER`, `LANG`, `LC_*` and
    /// `TMPDIR`, plus `env`.
    #[serde(default = "default_true")]
    pub inherit_env: bool,
}

/// Provider credentials.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfigYaml {
    pub openai: Option<ProviderCredentials>,
    pub anthropic: Option<ProviderCredentials>,
    pub gemini: Option<ProviderCredentials>,
    pub ollama: Option<OllamaCredentials>,
    pub mistral: Option<ProviderCredentials>,
    /// OpenRouter — uses the OpenAI-compatible API at openrouter.ai/api/v1.
    /// One API key, every model. The daemon wires this through the
    /// OpenAI provider with the right base URL and a "openrouter"
    /// provider id, so agents reference it via `provider: openrouter`.
    pub openrouter: Option<ProviderCredentials>,
    /// Explicit billing configuration for bounded native OpenRouter execution.
    /// Credits must match the account configuration; external BYOK changes are
    /// outside this supported contract. BYOK execution is not implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openrouter_billing: Option<OpenRouterBilling>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenRouterBilling {
    Credits,
    Byok,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredentials {
    pub api_key: SecretString,
    /// Optional base URL override for OpenAI-compatible servers (LM Studio,
    /// MLX/oMLX, vLLM, etc.). When set on the `openai` provider, requests go
    /// here instead of api.openai.com. Must include the API version suffix
    /// the server expects (usually `/v1`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Opt-in rate-limit fallback, written as `"provider:model"` (e.g.
    /// `"anthropic:claude-sonnet-4-6"`). If this provider returns a rate-limit
    /// error, the request is retried once on the named backup provider using
    /// that model. A tool-bearing current turn then remains pinned to that
    /// exact provider and model for native-history replay; plain-text turns
    /// may select independently. A bare `"provider"` uses the backup's own
    /// default model. Not a credential.
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaCredentials {
    pub base_url: String,
    /// Default model for Ollama agents (overridden by per-agent `model` field).
    pub model: Option<String>,
}

/// Server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigYaml {
    #[serde(default = "default_port")]
    pub port: u16,
    /// Bind address. Defaults to loopback (`127.0.0.1`) — the server is
    /// unauthenticated-friendly only for local, single-user use. Exposing it on
    /// a non-loopback address (e.g. `0.0.0.0`) requires `auth` to be configured;
    /// see `serve` for the fail-closed guard.
    #[serde(default = "default_host")]
    pub host: String,
    /// API authentication. Empty by default (fine on loopback). Required before
    /// the server will bind to a non-loopback address.
    #[serde(default)]
    pub auth: ServerAuthYaml,
    /// Cross-origin allow-list for the HTTP API. Empty means **same-origin
    /// only** (the dashboard keeps working; arbitrary web pages cannot call the
    /// API from a user's browser). Add explicit origins to opt in.
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// Per-IP HTTP rate limiting. Disabled by default — intended for a
    /// publicly-reachable deployment; a loopback dashboard needs no limit.
    #[serde(default)]
    pub rate_limit: RateLimitYaml,
}

impl Default for ServerConfigYaml {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            auth: ServerAuthYaml::default(),
            cors_origins: Vec::new(),
            rate_limit: RateLimitYaml::default(),
        }
    }
}

/// Per-IP HTTP rate-limit configuration. Off by default; when `enabled`, a
/// client exceeding `max_requests` within `window_secs` gets `429`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitYaml {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rate_max")]
    pub max_requests: u32,
    #[serde(default = "default_rate_window")]
    pub window_secs: u64,
}

impl Default for RateLimitYaml {
    fn default() -> Self {
        Self {
            enabled: false,
            max_requests: default_rate_max(),
            window_secs: default_rate_window(),
        }
    }
}

fn default_rate_max() -> u32 {
    100
}

fn default_rate_window() -> u64 {
    60
}

/// API authentication for the HTTP/WS server. Tokens support `${ENV_VAR}`
/// interpolation so they need not be committed in plaintext. With none
/// configured, a loopback server requires its per-daemon local API token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerAuthYaml {
    /// Accepted `x-api-key` values. Held as `SecretString` so a stray `{:?}`
    /// can never leak a credential into logs; `${ENV}` interpolation still
    /// applies (it runs on the raw YAML before parsing).
    #[serde(default)]
    pub api_keys: Vec<SecretString>,
    /// Accepted `Authorization: Bearer <token>` values. Redacted like `api_keys`.
    #[serde(default)]
    pub bearer_tokens: Vec<SecretString>,
    /// Escape hatch: serve **without** auth (e.g. when an upstream proxy
    /// enforces it). On a non-loopback address this skips the fail-closed
    /// bind guard; on loopback it turns off the per-daemon local API token.
    /// The operator takes responsibility — only an explicit `true` does this.
    #[serde(default)]
    pub allow_unauthenticated: bool,
}

impl ServerAuthYaml {
    /// Auth is enforced when at least one credential is configured.
    pub fn is_enabled(&self) -> bool {
        !self.api_keys.is_empty() || !self.bearer_tokens.is_empty()
    }
}

fn default_port() -> u16 {
    8080
}
fn default_host() -> String {
    // Loopback by default. Binding to all interfaces is opt-in and, without
    // auth, refused at startup. See axocoatl-server::serve.
    "127.0.0.1".to_string()
}

/// Session sandbox (podman container) trust + isolation policy. Defaults are
/// secure: a freshly-opened repository cannot run its own setup scripts or pull
/// an attacker-chosen image. Loosen these only for repositories you trust.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfigYaml {
    /// Default an exact devcontainer `postCreateCommand` to approved for an
    /// unreviewed Session. The browser exposes that default and an explicit
    /// per-Session decision overrides it. It never covers an independently
    /// detected setup command such as `npm ci`.
    #[serde(default)]
    pub allow_post_create_command: bool,
    /// Honor a repo/UI-specified base image other than the trusted default.
    #[serde(default)]
    pub allow_untrusted_images: bool,
    /// Container networking: `"bridge"` (default, outbound + published ports),
    /// `"none"` (no network — blocks exfiltration for untrusted code, but
    /// also package installs and dev servers) or `"egress"` (only the hosts
    /// under `egress.allow`, through Axocoatl's proxy). Any other value fails
    /// validation instead of falling back to bridge.
    #[serde(default = "default_sandbox_network")]
    pub network: String,
    /// Refuse to start a session if memory/CPU/pid limits can't be applied,
    /// instead of silently running uncapped. Off by default because some hosts
    /// (rootless podman on WSL2) can't delegate cgroups.
    #[serde(default)]
    pub require_resource_limits: bool,
    /// Isolation backend: `"podman"` (default — a local rootless container) or
    /// `"e2b"` (a remote microVM backend validated with E2B Cloud). This is one
    /// daemon-global choice, not a per-Workspace or per-Session picker. `"e2b"`
    /// requires the `e2b:` block below.
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,
    /// Settings for the `e2b` backend. Ignored unless `backend: e2b`.
    #[serde(default)]
    pub e2b: Option<E2bBackendYaml>,
    /// Egress allowlist for `network: egress`. Under other network modes
    /// `allow`, `private_destinations` and `routes` are ignored with a
    /// warning; the browser's own proxy still reads `sidecar_network` and
    /// `max_connections`, and `record_max_events` caps the Session network
    /// record in every mode.
    #[serde(default)]
    pub egress: Option<EgressConfigYaml>,
    /// Which users run Agents' commands and read-only helpers' processes in
    /// a local Podman Session container. Omitted means `mode: auto`.
    #[serde(default)]
    pub workload: Option<WorkloadConfigYaml>,
}

/// `sandbox.workload`. `auto` (the default) is `hardened` under
/// `network: egress` and `image` under `bridge` and `none`. `hardened` runs
/// Agents' commands, setup commands and terminals as `writer_user` and
/// read-only helpers as `helper_user`, both without Linux capabilities,
/// while the container's first process stays root; it needs rootless Podman.
/// `image` runs every command as the image's own user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadConfigYaml {
    #[serde(default = "default_workload_mode")]
    pub mode: String,
    /// Numeric `uid:gid`.
    #[serde(default = "default_workload_writer_user")]
    pub writer_user: String,
    /// Numeric `uid:gid` sharing no id with `writer_user`.
    #[serde(default = "default_workload_helper_user")]
    pub helper_user: String,
}

impl Default for WorkloadConfigYaml {
    fn default() -> Self {
        Self {
            mode: default_workload_mode(),
            writer_user: default_workload_writer_user(),
            helper_user: default_workload_helper_user(),
        }
    }
}

fn default_workload_mode() -> String {
    "auto".to_string()
}
fn default_workload_writer_user() -> String {
    "1000:1000".to_string()
}
fn default_workload_helper_user() -> String {
    "1001:1001".to_string()
}

/// What a Session container may reach under `network: egress`. Everything
/// not listed is refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressConfigYaml {
    #[serde(default)]
    pub allow: Vec<EgressAllowYaml>,
    /// Private ranges (CIDR) that allowed hosts may resolve to.
    #[serde(default)]
    pub private_destinations: Vec<String>,
    /// Podman network for the egress sidecar, and for the browser's own
    /// proxy under `bridge` and `none`. Defaults to Podman's default.
    #[serde(default)]
    pub sidecar_network: Option<String>,
    /// 8-256 connections open at once through the proxy (the browser's own
    /// proxy too, under `bridge` and `none`).
    #[serde(default = "default_egress_max_connections")]
    pub max_connections: u32,
    /// 1,000-1,000,000 events in one Session's network record.
    #[serde(default = "default_egress_record_max_events")]
    pub record_max_events: u32,
    /// Hosts whose HTTPS traffic Axocoatl ends on this computer, checks
    /// request by request and, with a credential, signs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<EgressRouteYaml>,
}

impl Default for EgressConfigYaml {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            private_destinations: Vec::new(),
            sidecar_network: None,
            max_connections: default_egress_max_connections(),
            record_max_events: default_egress_record_max_events(),
            routes: Vec::new(),
        }
    }
}

fn default_egress_max_connections() -> u32 {
    128
}
fn default_egress_record_max_events() -> u32 {
    50_000
}

/// One entry of `sandbox.egress.routes`: a host whose HTTPS connections
/// Axocoatl ends with a certificate from the Session's own authority, so it
/// can check each request against `rules` (or `access`) and add
/// `credential`. Validation is in [`crate::egress_routes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressRouteYaml {
    /// Exact host name; no wildcard and no IP address.
    pub host: String,
    /// Defaults to `[443]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<u16>>,
    /// Name of a `credentials` entry added to every allowed request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// How the credential is added. Required with `credential`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<RouteInjectYaml>,
    /// Which processes the route serves. Defaults to `[agent]`.
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<RouteForYaml>>,
    /// PEM file with the certificate authority of a private upstream, trusted
    /// for this route in addition to this computer's own trust settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_ca: Option<String>,
    /// A preset instead of `rules`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<RouteAccessYaml>,
    /// Requests the route allows; everything else is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RouteRuleYaml>,
    /// Environment variables set to a placeholder in Agents' environments, for
    /// tools that refuse to run without a token of their own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_placeholders: Vec<String>,
    /// Pass compressed responses on a credentialed route, which the
    /// credential-reflection check cannot read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_encoded_responses: bool,
    /// Pass `Set-Cookie` and `Set-Cookie2` on a credentialed route. They are
    /// removed by default: a session the host starts for the credential would
    /// otherwise reach the container and work without the route.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_set_cookie: bool,
    /// Largest request body, in bytes. Defaults to 1 GiB.
    #[serde(default = "default_route_max_request_bytes")]
    pub max_request_bytes: u64,
}

fn default_route_max_request_bytes() -> u64 {
    1 << 30
}

/// How a route adds its credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteInjectYaml {
    /// `Authorization: Basic base64(username:credential)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic: Option<RouteBasicYaml>,
    /// A header whose value is `format` with `{}` replaced by the credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Defaults to `"{}"`, the credential alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteBasicYaml {
    pub username: String,
}

/// A process kind a route serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteForYaml {
    /// Agents' tool calls.
    Agent,
    /// Terminals you open.
    Terminal,
    /// Setup commands.
    Setup,
}

/// Request presets for a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteAccessYaml {
    /// `GET`, `HEAD` and `OPTIONS` on every path.
    ReadOnly,
    /// Every method on every path.
    Full,
}

/// One allowed request shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRuleYaml {
    /// Uppercase method names, such as `[GET, HEAD]`.
    pub methods: Vec<String>,
    /// A path glob: `*` matches one segment, `**` any number of segments.
    pub path: String,
    /// Query parameters the request must carry, each exactly once, with this
    /// value (`*` for any value). Other parameters are allowed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub query: std::collections::BTreeMap<String, String>,
}

/// Where the daemon reads one credential: exactly one of `env` and `file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialSourceYaml {
    /// An environment variable of the daemon, such as `GITHUB_TOKEN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// An owner-only file outside every Workspace; absolute or `~/...`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Anything but a mapping is refused without repeating it, since a value
/// written there is most likely the credential itself.
impl<'de> Deserialize<'de> for CredentialSourceYaml {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(default)]
            env: Option<String>,
            #[serde(default)]
            file: Option<String>,
        }

        struct SourceVisitor;

        const EXPECTED: &str = "{env: VARIABLE} or {file: /path/to/file}; Axocoatl does not take \
                                credential values in its config";

        fn refuse<E: serde::de::Error>() -> E {
            E::custom(format!(
                "a credential is {EXPECTED} (the value written here is not shown)"
            ))
        }

        impl<'de> serde::de::Visitor<'de> for SourceVisitor {
            type Value = CredentialSourceYaml;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str(EXPECTED)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let fields =
                    Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(CredentialSourceYaml {
                    env: fields.env,
                    file: fields.file,
                })
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Err(refuse())
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                _: A,
            ) -> Result<Self::Value, A::Error> {
                Err(refuse())
            }
        }

        deserializer.deserialize_any(SourceVisitor)
    }
}

/// One egress allowlist entry: a preset name, a host name, or an address range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    untagged,
    expecting = "a preset name such as npm, {host: example.com, ports: [443]} or {cidr: 10.0.0.0/8, ports: [443]}"
)]
pub enum EgressAllowYaml {
    Preset(String),
    Host(EgressHostYaml),
    Cidr(EgressCidrYaml),
}

/// An allowed host name, or `*.example.com` for its subdomains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressHostYaml {
    pub host: String,
    /// Defaults to `[443]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<u16>>,
}

/// An allowed address range for IP-literal destinations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressCidrYaml {
    pub cidr: String,
    /// Defaults to `[443]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<u16>>,
}

impl Default for SandboxConfigYaml {
    fn default() -> Self {
        Self {
            allow_post_create_command: false,
            allow_untrusted_images: false,
            network: default_sandbox_network(),
            require_resource_limits: false,
            backend: default_sandbox_backend(),
            e2b: None,
            egress: None,
            workload: None,
        }
    }
}

fn default_sandbox_network() -> String {
    "bridge".to_string()
}

fn default_sandbox_backend() -> String {
    "podman".to_string()
}

/// Connection settings for the remote E2B backend. Axocoatl 1.0 validates these
/// semantics with E2B Cloud; third-party E2B API implementations are outside the
/// 1.0 support claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2bBackendYaml {
    /// Control-plane API URL. The validated E2B Cloud value is
    /// `https://api.e2b.dev`.
    #[serde(default = "default_e2b_api_url")]
    pub api_url: String,
    /// Control-plane API key. Write `${E2B_API_KEY}` to source it from the
    /// environment rather than committing it to the config file.
    #[serde(default)]
    pub api_key: SecretString,
    /// E2B sandbox template / image id, such as `"base"`.
    #[serde(default = "default_e2b_template")]
    pub template: String,
    /// Data-plane host domain — the `envd` control host is
    /// `https://49983-{sandbox_id}.{domain}`. E2B cloud = `e2b.app`.
    #[serde(default = "default_e2b_domain")]
    pub domain: String,
    /// Token for cloning/pushing the session's git repo **inside** the remote
    /// VM (git-native remote sessions). Write `${GITHUB_TOKEN}` to source it from
    /// the environment rather than committing it. Only needed for private repos;
    /// public repos clone without it. Injected as a sandbox secret and read by an
    /// in-VM credential helper — never written into the repo's git config.
    #[serde(default)]
    pub git_token: SecretString,
}

fn default_e2b_api_url() -> String {
    "https://api.e2b.dev".to_string()
}

fn default_e2b_template() -> String {
    "base".to_string()
}

fn default_e2b_domain() -> String {
    "e2b.app".to_string()
}

/// Hook configuration in YAML.
///
/// **Experimental — not yet active.** These entries are parsed and validated but
/// are not invoked by the runtime; the only hook that executes is the built-in
/// MCP tool-approval gate. Configuring `hooks:` is currently a no-op (the daemon
/// emits a startup warning to make this explicit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfigYaml {
    pub name: String,
    #[serde(rename = "type")]
    pub hook_type: String,
    #[serde(default)]
    pub phase: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    /// For HTTP hooks: the webhook URL.
    pub url: Option<String>,
    /// For agent hooks: the agent ID to invoke.
    pub agent_id: Option<String>,
}

fn default_hook_timeout() -> u64 {
    30
}
