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

    // Role invariants: coordinators and workers only make sense inside a
    // workflow — a worker is spawned and driven by its workflow's coordinator,
    // never standalone. Reject a role with no workflow to back it so a
    // half-wired multi-agent setup fails loudly at load time instead of at run.
    let workflow_members: std::collections::HashSet<&str> = config
        .workflows
        .iter()
        .flat_map(|w| w.agents.iter().map(String::as_str))
        .collect();
    let workflow_entry_points: std::collections::HashSet<&str> = config
        .workflows
        .iter()
        .filter_map(|w| w.entry_point.as_deref())
        .collect();
    for agent in &config.agents {
        match agent.role {
            AgentRoleYaml::Worker => {
                if !agent.depends_on.is_empty() {
                    return Err(ConfigError::InvalidField {
                        field: format!("agents[{}].depends_on", agent.id),
                        value: format!("{:?}", agent.depends_on),
                        reason: "A worker is driven by its coordinator, not by the \
                                 event lattice, so it must not declare depends_on"
                            .to_string(),
                        suggestion: "Remove depends_on from this worker agent.".to_string(),
                    });
                }
                if !workflow_members.contains(agent.id.as_str()) {
                    return Err(ConfigError::InvalidField {
                        field: format!("agents[{}].role", agent.id),
                        value: "worker".to_string(),
                        reason: "A worker must belong to a workflow (be listed in some \
                                 workflow's agents); its coordinator spawns it on demand"
                            .to_string(),
                        suggestion: format!(
                            "Add '{}' to a workflow's agents list, or change its role.",
                            agent.id
                        ),
                    });
                }
            }
            AgentRoleYaml::Coordinator => {
                if !workflow_entry_points.contains(agent.id.as_str()) {
                    return Err(ConfigError::InvalidField {
                        field: format!("agents[{}].role", agent.id),
                        value: "coordinator".to_string(),
                        reason: "A coordinator must be the entry_point of some workflow \
                                 (the workflow whose workers it manages)"
                            .to_string(),
                        suggestion: format!(
                            "Set a workflow's entry_point to '{}', or change its role.",
                            agent.id
                        ),
                    });
                }
            }
            AgentRoleYaml::Autonomous => {}
        }
    }

    // MCP servers: validate the transport so a malformed or tampered config is
    // rejected up front rather than silently spawning the wrong process or
    // reaching an unexpected endpoint at connect time. The config file is a
    // trust boundary — this is the consistency gate on it.
    for mcp in &config.mcp_servers {
        let field = |suffix: &str| format!("mcp_servers[{}].{suffix}", mcp.name);
        match mcp.transport.as_str() {
            "stdio" => {
                let cmd = mcp.command.as_deref().unwrap_or("");
                if cmd.trim().is_empty() {
                    return Err(ConfigError::InvalidField {
                        field: field("command"),
                        value: "\"\"".to_string(),
                        reason: "stdio MCP servers must specify a non-empty 'command'".to_string(),
                        suggestion: "Set command to the server launcher, e.g. command: npx"
                            .to_string(),
                    });
                }
            }
            "streamable_http" | "http" => {
                let url = mcp.url.as_deref().unwrap_or("");
                if !is_http_url(url) {
                    return Err(ConfigError::InvalidField {
                        field: field("url"),
                        value: format!("{url:?}"),
                        reason: "http MCP servers require a 'url' with an http:// or https:// \
                                 scheme and a host"
                            .to_string(),
                        suggestion: "Set url like: url: https://mcp.example.com/sse".to_string(),
                    });
                }
            }
            other => {
                return Err(ConfigError::InvalidField {
                    field: field("transport"),
                    value: format!("{other:?}"),
                    reason: "unknown MCP transport".to_string(),
                    suggestion: "Use transport: stdio | streamable_http | http".to_string(),
                });
            }
        }
    }

    Ok(())
}

/// Lightweight check that a string is an `http`/`https` URL with a host — used
/// to reject scheme confusion (`file://`, …) and hostless URLs in MCP config
/// without pulling in a full URL parser.
fn is_http_url(u: &str) -> bool {
    match u
        .strip_prefix("http://")
        .or_else(|| u.strip_prefix("https://"))
    {
        Some(rest) => !rest.is_empty() && !rest.starts_with('/'),
        None => false,
    }
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
    fn parse_activation_overrides() {
        let yaml = r#"
agents:
  - id: tuned
    name: "Tuned"
    provider: ollama
    model: llama3
    depends_on: [a, b]
    activation_threshold: 0.75
    activation_decay: 0.05
  - id: defaulted
    name: "Defaulted"
    provider: ollama
    model: llama3
    depends_on: [a]
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        // Explicit per-agent overrides are read through.
        assert_eq!(config.agents[0].activation_threshold, Some(0.75));
        assert_eq!(config.agents[0].activation_decay, Some(0.05));
        // Absent → None, so the automatic 0.5 × N threshold still applies.
        assert_eq!(config.agents[1].activation_threshold, None);
        assert_eq!(config.agents[1].activation_decay, None);
    }

    #[test]
    fn worker_with_depends_on_rejected() {
        let yaml = r#"
agents:
  - id: w
    name: "W"
    provider: ollama
    model: llama3
    role: worker
    depends_on: [x]
workflows:
  - id: wf
    name: "WF"
    agents: [w]
    entry_point: lead
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidField { ref field, .. } if field.contains("depends_on"))
        );
    }

    #[test]
    fn worker_without_workflow_rejected() {
        let yaml = r#"
agents:
  - id: w
    name: "W"
    provider: ollama
    model: llama3
    role: worker
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidField { ref reason, .. } if reason.contains("must belong to a workflow"))
        );
    }

    #[test]
    fn coordinator_without_workflow_rejected() {
        let yaml = r#"
agents:
  - id: c
    name: "C"
    provider: ollama
    model: llama3
    role: coordinator
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidField { ref reason, .. } if reason.contains("entry_point of some workflow"))
        );
    }

    #[test]
    fn valid_coordinator_worker_config_accepted() {
        let yaml = r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
    role: coordinator
  - id: w
    name: "W"
    provider: ollama
    model: llama3
    role: worker
workflows:
  - id: wf
    name: "WF"
    agents: [lead, w]
    entry_point: lead
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(config.agents.len(), 2);
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
    fn is_http_url_accepts_http_and_https_with_host() {
        assert!(is_http_url("http://localhost:6334"));
        assert!(is_http_url("https://mcp.example.com/sse"));
        // Wrong scheme, hostless, or empty must be rejected.
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("ftp://host"));
        assert!(!is_http_url("http://"));
        assert!(!is_http_url("http:///path"));
        assert!(!is_http_url(""));
        assert!(!is_http_url("mcp.example.com"));
    }

    #[test]
    fn validate_mcp_stdio_requires_command() {
        let yaml = r#"
mcp_servers:
  - name: tools
    transport: stdio
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("non-empty 'command'"));
    }

    #[test]
    fn validate_mcp_http_rejects_bad_url() {
        let yaml = r#"
mcp_servers:
  - name: remote
    transport: http
    url: "file:///etc/passwd"
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("http:// or https://"));
    }

    #[test]
    fn validate_mcp_unknown_transport() {
        let yaml = r#"
mcp_servers:
  - name: weird
    transport: carrier-pigeon
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("unknown MCP transport"));
    }

    #[test]
    fn validate_mcp_well_formed_passes() {
        let yaml = r#"
mcp_servers:
  - name: local
    transport: stdio
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem"]
  - name: remote
    transport: streamable_http
    url: "https://mcp.example.com/sse"
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);
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
