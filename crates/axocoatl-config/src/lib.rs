pub mod automation;
mod convert;
pub mod error;
pub mod secret;
pub mod types;

pub use automation::*;
pub use error::*;
pub use secret::SecretString;
pub use types::*;

use std::path::Path;

/// Load and validate config from a YAML file.
pub async fn load_config(path: &Path) -> Result<AxocoatlConfig, ConfigError> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(ConfigError::Io)?;
    parse_config(&raw, path)
}

/// Parse and validate config from a YAML string.
pub fn parse_config(yaml: &str, source_path: &Path) -> Result<AxocoatlConfig, ConfigError> {
    let interpolated = interpolate_env_vars(yaml);

    let config: AxocoatlConfig =
        serde_yaml::from_str(&interpolated).map_err(|e| ConfigError::ParseError {
            path: source_path.to_path_buf(),
            reason: e.to_string(),
            suggestion: generate_parse_suggestion(&e.to_string()),
        })?;

    validate_config(&config)?;
    Ok(config)
}

/// Interpolate `${VAR_NAME}` patterns with environment variable values.
pub fn interpolate_env_vars(input: &str) -> String {
    let re = regex::Regex::new(r"\$\{([^}]+)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        std::env::var(var_name).unwrap_or_else(|_| {
            tracing::warn!(var = var_name, "Environment variable not set");
            String::new()
        })
    })
    .to_string()
}

/// Validate a parsed config, returning actionable errors.
fn validate_config(config: &AxocoatlConfig) -> Result<(), ConfigError> {
    let mut seen_ids = std::collections::HashSet::new();

    for agent in &config.agents {
        if agent.id.is_empty() {
            return Err(ConfigError::InvalidField {
                field: "agents[].id".to_string(),
                value: "\"\"".to_string(),
                reason: "Agent ID cannot be empty".to_string(),
                suggestion: "Set a unique identifier like: id: my_agent".to_string(),
            });
        }

        if agent.provider.is_empty() {
            return Err(ConfigError::InvalidField {
                field: format!("agents[{}].provider", agent.id),
                value: "\"\"".to_string(),
                reason: "Provider must be specified".to_string(),
                suggestion: "Set provider to one of: openai, anthropic, gemini, ollama, mistral"
                    .to_string(),
            });
        }

        if let Some(budget) = &agent.token_budget {
            if budget.per_call > budget.per_execution {
                return Err(ConfigError::InvalidField {
                    field: format!("agents[{}].token_budget", agent.id),
                    value: format!(
                        "per_call: {} > per_execution: {}",
                        budget.per_call, budget.per_execution
                    ),
                    reason: "per_call cannot exceed per_execution".to_string(),
                    suggestion: format!(
                        "Set per_execution to at least {} (current per_call value)",
                        budget.per_call
                    ),
                });
            }
        }

        if !seen_ids.insert(&agent.id) {
            return Err(ConfigError::DuplicateId {
                field: "agents[].id".to_string(),
                id: agent.id.clone(),
            });
        }
    }

    Ok(())
}

fn generate_parse_suggestion(error_msg: &str) -> String {
    if error_msg.contains("expected") && error_msg.contains("found") {
        "Check the YAML indentation and value types. YAML is indentation-sensitive.".to_string()
    } else if error_msg.contains("missing field") {
        format!("A required field is missing. {}", error_msg)
    } else {
        "Check YAML syntax: proper indentation, colons after keys, quotes around special values."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const VALID_YAML: &str = r#"
agents:
  - id: researcher
    name: "Research Agent"
    provider: openai
    model: gpt-4o
    system_prompt: "You are a researcher."
    tools:
      - web_search
    token_budget:
      per_execution: 20000
      per_call: 8192
      overflow_policy: summarize
    memory:
      backend: in_memory
      max_session_messages: 100

  - id: summarizer
    name: "Summary Agent"
    provider: anthropic
    model: claude-haiku-4-5-20251001
    system_prompt: "Summarize the research."

providers:
  openai:
    api_key: "sk-test-key"
  anthropic:
    api_key: "sk-ant-test"

server:
  port: 8080
  host: "0.0.0.0"
"#;

    #[test]
    fn parse_valid_config() {
        let config = parse_config(VALID_YAML, &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[0].id, "researcher");
        assert_eq!(config.agents[0].provider, "openai");
        assert_eq!(config.agents[1].id, "summarizer");
        assert!(config.agents[0].token_budget.is_some());
        assert!(config.agents[1].token_budget.is_none());
    }

    #[test]
    fn parse_minimal_config() {
        let yaml = r#"
agents:
  - id: basic
    name: "Basic"
    provider: ollama
    model: llama3
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].model, "llama3");
    }

    #[test]
    fn parse_empty_config() {
        let yaml = "";
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        assert!(config.agents.is_empty());
    }

    #[test]
    fn validate_empty_agent_id() {
        let yaml = r#"
agents:
  - id: ""
    name: "Bad"
    provider: openai
    model: gpt-4o
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("Agent ID cannot be empty"));
    }

    #[test]
    fn validate_empty_provider() {
        let yaml = r#"
agents:
  - id: test
    name: "Bad"
    provider: ""
    model: gpt-4o
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("Provider must be specified"));
    }

    #[test]
    fn validate_duplicate_ids() {
        let yaml = r#"
agents:
  - id: same
    name: "First"
    provider: openai
    model: gpt-4o
  - id: same
    name: "Second"
    provider: openai
    model: gpt-4o
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("Duplicate ID"));
    }

    #[test]
    fn validate_per_call_exceeds_per_execution() {
        let yaml = r#"
agents:
  - id: bad_budget
    name: "Bad"
    provider: openai
    model: gpt-4o
    token_budget:
      per_call: 10000
      per_execution: 5000
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("per_call cannot exceed"));
    }

    #[test]
    fn env_var_interpolation() {
        std::env::set_var("AXOCOATL_TEST_KEY", "secret123");
        let input = "api_key: ${AXOCOATL_TEST_KEY}";
        let result = interpolate_env_vars(input);
        assert_eq!(result, "api_key: secret123");
        std::env::remove_var("AXOCOATL_TEST_KEY");
    }

    #[test]
    fn env_var_missing_becomes_empty() {
        let input = "api_key: ${DEFINITELY_NOT_SET_12345}";
        let result = interpolate_env_vars(input);
        assert_eq!(result, "api_key: ");
    }

    #[test]
    fn invalid_yaml_returns_parse_error() {
        let yaml = "agents: [[[invalid yaml";
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        match err {
            ConfigError::ParseError { suggestion, .. } => {
                assert!(!suggestion.is_empty());
            }
            _ => panic!("Expected ParseError"),
        }
    }

    #[test]
    fn config_with_providers_section() {
        let config = parse_config(VALID_YAML, &PathBuf::from("test.yaml")).unwrap();
        assert!(config.providers.openai.is_some());
        assert!(config.providers.anthropic.is_some());
    }

    #[test]
    fn config_server_section() {
        let config = parse_config(VALID_YAML, &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(config.server.port, 8080);
    }
}
