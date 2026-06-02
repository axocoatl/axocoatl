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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum OverflowPolicy {
    /// Summarize context and continue.
    #[default]
    Summarize,
    /// Abort execution and return error.
    Abort,
    /// Log warning and continue (no enforcement).
    Warn,
}

/// Memory backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub backend: MemoryBackend,
    pub max_session_messages: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: MemoryBackend::default(),
            max_session_messages: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum MemoryBackend {
    #[default]
    InMemory,
    LanceDb {
        path: String,
    },
    Qdrant {
        url: String,
    },
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
                backend: MemoryBackend::LanceDb {
                    path: "./data/memory".to_string(),
                },
                max_session_messages: 50,
            },
            role: AgentRole::default(),
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
    fn overflow_policy_default_is_summarize() {
        let policy = OverflowPolicy::default();
        assert!(matches!(policy, OverflowPolicy::Summarize));
    }

    #[test]
    fn memory_backend_default_is_in_memory() {
        let backend = MemoryBackend::default();
        assert!(matches!(backend, MemoryBackend::InMemory));
    }
}
