use serde::{Deserialize, Serialize};

/// Unique identifier for an agent instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The current lifecycle state of an agent actor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Running,
    Waiting { reason: String },
    Failed { error: String, restarts: u32 },
    Terminated,
}

/// Role an agent plays in a multi-agent system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum AgentRole {
    /// Standard independent agent.
    #[default]
    Autonomous,
    /// Orchestrator that spawns and manages worker agents.
    Coordinator,
    /// Worker agent spawned by a coordinator.
    Worker,
}

/// Output format requested from the model. `Json` maps to each provider's
/// native JSON mode where one exists (Ollama `format`, OpenAI/Mistral
/// `response_format`, Gemini `responseMimeType`); for a provider without a
/// native mode it is enforced via a system-prompt instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    Json,
}

/// Per-agent sampling controls, threaded into every LLM request the agent
/// makes. All optional — an unset field leaves the provider's default in place.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SamplingConfig {
    /// Sampling temperature. `0` makes a model effectively deterministic.
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff.
    pub top_p: Option<f32>,
    /// Max completion tokens per call (distinct from the spend `token_budget`).
    pub max_tokens: Option<usize>,
    /// Requested output format (e.g. force JSON).
    pub response_format: Option<ResponseFormat>,
}

/// Configuration for a single agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: AgentId,
    pub name: String,
    /// The LLM provider to use (e.g., "openai", "anthropic", "ollama").
    pub provider: String,
    pub model: String,
    /// System prompt — developer-controlled, no hidden injection.
    pub system_prompt: Option<String>,
    /// Maximum tokens this agent may consume per execution.
    pub token_budget: Option<TokenBudget>,
    /// Tools available to this agent (MCP tool names).
    pub tools: Vec<String>,
    /// Memory configuration.
    pub memory: MemoryConfig,
    /// Role in multi-agent orchestration.
    pub role: AgentRole,
    /// Sampling controls (temperature, top_p, max_tokens, response format)
    /// applied to every LLM call this agent makes.
    #[serde(default)]
    pub sampling: SamplingConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            id: AgentId::new("default"),
            name: "Default Agent".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            system_prompt: None,
            token_budget: None,
            tools: Vec::new(),
            memory: MemoryConfig::default(),
            role: AgentRole::default(),
            sampling: SamplingConfig::default(),
        }
    }
}

/// Hard token budget enforcement per agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudget {
    /// Max tokens per single LLM call.
    pub per_call: usize,
    /// Max tokens per agent execution (across all LLM calls).
    pub per_execution: usize,
    /// Policy when budget is exceeded.
    pub overflow_policy: OverflowPolicy,
}

/// What to do when a token *spend* budget (`per_execution`) would be exceeded.
/// Context compaction toward the model's window is automatic and independent of
/// this policy — this is purely the cost-cap behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum OverflowPolicy {
    /// Enforce the budget: return a budget error. The default — a configured
    /// budget is meant to be enforced.
    #[default]
    Abort,
    /// Advisory: log a warning and continue past the budget.
    Warn,
}

/// Tuning for memory recall — governs both the passive top-k injection and the
/// agent-driven `recall_search` tool, so the two paths agree on the relevance bar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallConfig {
    /// Passively inject the top-k semantic hits into the prompt each turn.
    /// When false, the agent relies solely on the `recall_search` tool.
    pub passive_inject: bool,
    /// Number of semantic hits to retrieve (passive injection + `recall_search` default).
    pub top_k: usize,
    /// Minimum cosine similarity for a hit to count as relevant.
    pub min_score: f32,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            passive_inject: true,
            top_k: 5,
            min_score: 0.15,
        }
    }
}

/// One agent-editable core-memory block (Tier 3). Curated, always-in-context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoreBlockConfig {
    pub label: String,
    /// Seed content (e.g. a persona). The agent edits from here.
    pub value: String,
    /// Character budget; `0` = unlimited.
    pub limit: usize,
    /// When true, the block is shared across agents (opt-in).
    pub shared: bool,
    /// What the block is for — guides the agent and renders when empty.
    pub description: Option<String>,
}

/// Core-memory configuration: the set of named blocks an agent maintains.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoreMemoryConfig {
    pub blocks: Vec<CoreBlockConfig>,
}

impl Default for CoreMemoryConfig {
    fn default() -> Self {
        Self {
            blocks: default_core_blocks(),
        }
    }
}

/// The default per-agent block set: `persona` + `human` + `project`.
pub fn default_core_blocks() -> Vec<CoreBlockConfig> {
    vec![
        CoreBlockConfig {
            label: "persona".to_string(),
            value: String::new(),
            limit: 2000,
            shared: false,
            description: Some("Who you are and how you behave.".to_string()),
        },
        CoreBlockConfig {
            label: "human".to_string(),
            value: String::new(),
            limit: 2000,
            shared: false,
            description: Some("What you know about the user you serve.".to_string()),
        },
        CoreBlockConfig {
            label: "project".to_string(),
            value: String::new(),
            limit: 3000,
            shared: false,
            description: Some("Durable project context, decisions, and conventions.".to_string()),
        },
    ]
}

/// Memory configuration. Semantic (Tier-4) memory is always the on-disk store
/// built by `SemanticMemory::new`; there is no runtime backend selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub max_session_messages: usize,
    /// Recall tuning (passive injection + agent-driven recall tools).
    #[serde(default)]
    pub recall: RecallConfig,
    /// Agent-editable core-memory blocks (Tier 3).
    #[serde(default)]
    pub core: CoreMemoryConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_session_messages: 100,
            recall: RecallConfig::default(),
            core: CoreMemoryConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_new() {
        let id = AgentId::new("test-agent");
        assert_eq!(id.0, "test-agent");
        assert_eq!(id.to_string(), "test-agent");
    }

    #[test]
    fn agent_id_random_is_unique() {
        let a = AgentId::random();
        let b = AgentId::random();
        assert_ne!(a, b);
    }

    #[test]
    fn agent_id_serde_roundtrip() {
        let id = AgentId::new("my-agent");
        let json = serde_json::to_string(&id).unwrap();
        let back: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o");
        assert!(config.token_budget.is_none());
        assert!(config.tools.is_empty());
    }

    #[test]
    fn agent_config_serde_roundtrip() {
        let config = AgentConfig {
            id: AgentId::new("researcher"),
            name: "Research Agent".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            system_prompt: Some("You are a researcher.".to_string()),
            token_budget: Some(TokenBudget {
                per_call: 8192,
                per_execution: 20000,
                overflow_policy: OverflowPolicy::Abort,
            }),
            tools: vec!["web_search".to_string(), "read_file".to_string()],
            memory: MemoryConfig {
                max_session_messages: 50,
                recall: RecallConfig::default(),
                core: CoreMemoryConfig::default(),
            },
            role: AgentRole::default(),
            sampling: SamplingConfig::default(),
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, config.id);
        assert_eq!(back.provider, "anthropic");
        assert_eq!(back.tools.len(), 2);
    }

    #[test]
    fn agent_status_serde_roundtrip() {
        let statuses = vec![
            AgentStatus::Idle,
            AgentStatus::Running,
            AgentStatus::Waiting {
                reason: "waiting for tool".to_string(),
            },
            AgentStatus::Failed {
                error: "timeout".to_string(),
                restarts: 2,
            },
            AgentStatus::Terminated,
        ];

        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let back: AgentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn overflow_policy_default_is_abort() {
        let policy = OverflowPolicy::default();
        assert!(matches!(policy, OverflowPolicy::Abort));
    }
}
