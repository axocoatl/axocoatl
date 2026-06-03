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
    #[serde(default)]
    pub daemon: DaemonConfigYaml,
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
}

/// Web-search provider for session agents. When present, the `web_search`
/// tool is offered to a session's agents.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSearchConfigYaml {
    /// Provider name — currently `"tavily"`.
    #[serde(default)]
    pub provider: String,
    /// Provider API key.
    #[serde(default)]
    pub api_key: SecretString,
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
    /// Fire whenever a named lattice event occurs (e.g. `"AgentFailed"`, or a
    /// custom event name emitted by a Skill).
    OnEvent { event: String },
}

/// A scheduled workflow run. `every` accepts simple intervals:
///   "30s", "5m", "2h", "1d", or "cron: 0 9 * * *" (cron daily 09:00).
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

/// A Skill — Axocoatl's lattice-aware unit of capability.
/// Differentiator vs. classic Skills: declares `emits` and `reacts_to` events,
/// composing through the lattice without manual wiring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillConfigYaml {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Lattice events this Skill emits when it completes.
    #[serde(default)]
    pub emits: Vec<String>,
    /// Lattice events this Skill reacts to (auto-activation).
    #[serde(default)]
    pub reacts_to: Vec<String>,
    /// Agents that hold this Skill (any of them can win an auction for it).
    #[serde(default)]
    pub agents: Vec<String>,
    /// Inline prompt template (rendered when the Skill fires).
    #[serde(default)]
    pub prompt: String,
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
    pub token_budget: Option<TokenBudgetYaml>,
    #[serde(default)]
    pub memory: MemoryConfigYaml,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub role: AgentRoleYaml,
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
    #[default]
    Summarize,
    Abort,
    Warn,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfigYaml {
    #[serde(default)]
    pub backend: MemoryBackendYaml,
    #[serde(default = "default_max_session")]
    pub max_session_messages: usize,
    /// Path for LanceDB storage (used when backend is lancedb).
    pub path: Option<String>,
}

fn default_max_session() -> usize {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBackendYaml {
    #[default]
    InMemory,
    Lancedb,
    Qdrant,
}

/// Workflow configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfigYaml {
    pub id: String,
    pub name: String,
    #[serde(default = "default_coordination")]
    pub coordination: String,
    #[serde(default)]
    pub agents: Vec<String>,
    pub entry_point: Option<String>,
    pub htn_methods_file: Option<String>,
}

fn default_coordination() -> String {
    "stigmergic_lattice".to_string()
}

/// MCP server connection config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfigYaml {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredentials {
    pub api_key: SecretString,
    /// Fallback provider/model identifier for the registry's fallback chain —
    /// not a credential.
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
}

impl Default for ServerConfigYaml {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            auth: ServerAuthYaml::default(),
            cors_origins: Vec::new(),
        }
    }
}

/// API authentication for the HTTP/WS server. Tokens support `${ENV_VAR}`
/// interpolation so they need not be committed in plaintext.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerAuthYaml {
    /// Accepted `x-api-key` values.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Accepted `Authorization: Bearer <token>` values.
    #[serde(default)]
    pub bearer_tokens: Vec<String>,
    /// Escape hatch: bind a non-loopback address **without** auth (e.g. when an
    /// upstream proxy enforces it). The operator takes responsibility — the
    /// fail-closed guard is skipped only when this is explicitly `true`.
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
    /// Run a repo's `postCreateCommand` automatically when a session opens.
    /// Off by default — otherwise opening a hostile repo is remote code
    /// execution.
    #[serde(default)]
    pub allow_post_create_command: bool,
    /// Honor a repo/UI-specified base image other than the trusted default.
    #[serde(default)]
    pub allow_untrusted_images: bool,
    /// Container networking: `"bridge"` (default, outbound + published ports)
    /// or `"none"` (no network — blocks exfiltration for untrusted code, but
    /// also package installs and dev servers).
    #[serde(default = "default_sandbox_network")]
    pub network: String,
    /// Refuse to start a session if memory/CPU/pid limits can't be applied,
    /// instead of silently running uncapped. Off by default because some hosts
    /// (rootless podman on WSL2) can't delegate cgroups.
    #[serde(default)]
    pub require_resource_limits: bool,
}

impl Default for SandboxConfigYaml {
    fn default() -> Self {
        Self {
            allow_post_create_command: false,
            allow_untrusted_images: false,
            network: default_sandbox_network(),
            require_resource_limits: false,
        }
    }
}

fn default_sandbox_network() -> String {
    "bridge".to_string()
}

/// Daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfigYaml {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_secs: u64,
}

impl Default for DaemonConfigYaml {
    fn default() -> Self {
        Self {
            enabled: false,
            heartbeat_interval_secs: default_heartbeat(),
        }
    }
}

fn default_heartbeat() -> u64 {
    1800
}

/// Hook configuration in YAML.
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

// (Dead-code duplicate `SkillConfigYaml` removed during the Glyphs→Skills
//  rename. There was a pre-existing unused struct for prompt templates;
//  the real Skill type lives above with id/emits/reacts_to/agents/prompt.)
