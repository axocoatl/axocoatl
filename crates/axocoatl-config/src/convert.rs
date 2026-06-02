//! Conversion from YAML config types to axocoatl-core types.

use axocoatl_core::{
    AgentConfig, AgentId, AgentRole, MemoryBackend, MemoryConfig, OverflowPolicy, TokenBudget,
};

use crate::types::*;

impl AgentConfigYaml {
    /// Convert to the axocoatl-core AgentConfig type.
    pub fn to_core(&self) -> AgentConfig {
        AgentConfig {
            id: AgentId::new(&self.id),
            name: self.name.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            token_budget: self.token_budget.as_ref().map(|b| b.to_core()),
            tools: self.tools.clone(),
            memory: self.memory.to_core(),
            role: self.role.to_core(),
        }
    }
}

impl TokenBudgetYaml {
    pub fn to_core(&self) -> TokenBudget {
        TokenBudget {
            per_call: self.per_call,
            per_execution: self.per_execution,
            overflow_policy: self.overflow_policy.to_core(),
        }
    }
}

impl OverflowPolicyYaml {
    pub fn to_core(&self) -> OverflowPolicy {
        match self {
            OverflowPolicyYaml::Summarize => OverflowPolicy::Summarize,
            OverflowPolicyYaml::Abort => OverflowPolicy::Abort,
            OverflowPolicyYaml::Warn => OverflowPolicy::Warn,
        }
    }
}

impl AgentRoleYaml {
    pub fn to_core(&self) -> AgentRole {
        match self {
            AgentRoleYaml::Autonomous => AgentRole::Autonomous,
            AgentRoleYaml::Coordinator => AgentRole::Coordinator,
            AgentRoleYaml::Worker => AgentRole::Worker,
        }
    }
}

impl MemoryConfigYaml {
    pub fn to_core(&self) -> MemoryConfig {
        MemoryConfig {
            backend: match &self.backend {
                MemoryBackendYaml::InMemory => MemoryBackend::InMemory,
                MemoryBackendYaml::Lancedb => MemoryBackend::LanceDb {
                    path: self
                        .path
                        .clone()
                        .unwrap_or_else(|| "./data/memory".to_string()),
                },
                MemoryBackendYaml::Qdrant => MemoryBackend::Qdrant {
                    url: self
                        .path
                        .clone()
                        .unwrap_or_else(|| "http://localhost:6334".to_string()),
                },
            },
            max_session_messages: self.max_session_messages,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_yaml_to_core() {
        let yaml = AgentConfigYaml {
            id: "test".to_string(),
            name: "Test Agent".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            system_prompt: Some("You are helpful.".to_string()),
            tools: vec!["web_search".to_string()],
            token_budget: Some(TokenBudgetYaml {
                per_execution: 20000,
                per_call: 8192,
                overflow_policy: OverflowPolicyYaml::Abort,
            }),
            memory: MemoryConfigYaml::default(),
            depends_on: vec![],
            role: AgentRoleYaml::default(),
        };

        let core = yaml.to_core();
        assert_eq!(core.id, AgentId::new("test"));
        assert_eq!(core.provider, "openai");
        assert!(core.token_budget.is_some());
        let budget = core.token_budget.unwrap();
        assert_eq!(budget.per_execution, 20000);
        assert!(matches!(budget.overflow_policy, OverflowPolicy::Abort));
    }

    #[test]
    fn memory_config_lancedb() {
        let yaml = MemoryConfigYaml {
            backend: MemoryBackendYaml::Lancedb,
            max_session_messages: 50,
            path: Some("./custom/path".to_string()),
        };
        let core = yaml.to_core();
        assert!(matches!(core.backend, MemoryBackend::LanceDb { path } if path == "./custom/path"));
    }
}
