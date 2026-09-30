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

const MAX_PATH_IDENTIFIER_LEN: usize = 64;

fn is_filesystem_safe_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    !value.is_empty()
        && value.len() <= MAX_PATH_IDENTIFIER_LEN
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_autonomous_workflow_graph(
    workflow: &WorkflowConfigYaml,
    agents_by_id: &std::collections::HashMap<&str, &AgentConfigYaml>,
    member_ids: &std::collections::HashSet<&str>,
) -> Result<(), ConfigError> {
    let mut indegree = workflow
        .agents
        .iter()
        .map(|agent_id| (agent_id.as_str(), 0_usize))
        .collect::<std::collections::HashMap<_, _>>();
    let mut children = std::collections::HashMap::<&str, Vec<&str>>::new();
    for agent_id in &workflow.agents {
        let agent = agents_by_id[agent_id.as_str()];
        let mut dependencies = std::collections::HashSet::new();
        for dependency in &agent.depends_on {
            if !dependencies.insert(dependency.as_str()) {
                return Err(ConfigError::InvalidField {
                    field: format!("agents[{}].depends_on", agent.id),
                    value: format!("{:?}", agent.depends_on),
                    reason: format!(
                        "Agent '{}' repeats dependency '{}' in workflow '{}'",
                        agent.id, dependency, workflow.id
                    ),
                    suggestion: "List each dependency exactly once".to_string(),
                });
            }
            if dependency == &agent.id {
                return Err(ConfigError::InvalidField {
                    field: format!("agents[{}].depends_on", agent.id),
                    value: format!("{:?}", agent.depends_on),
                    reason: format!("Agent '{}' cannot depend on itself", agent.id),
                    suggestion: "Remove the self dependency".to_string(),
                });
            }
            if !member_ids.contains(dependency.as_str()) {
                return Err(ConfigError::InvalidField {
                    field: format!("agents[{}].depends_on", agent.id),
                    value: format!("{:?}", dependency),
                    reason: format!(
                        "Agent '{}' depends on '{}', which is outside workflow '{}'",
                        agent.id, dependency, workflow.id
                    ),
                    suggestion: format!(
                        "Add '{dependency}' to workflow '{}' or remove the dependency",
                        workflow.id
                    ),
                });
            }
            *indegree
                .get_mut(agent_id.as_str())
                .expect("workflow members initialize indegree") += 1;
            children
                .entry(dependency.as_str())
                .or_default()
                .push(agent_id.as_str());
        }
    }

    let mut ready = workflow
        .agents
        .iter()
        .filter(|agent_id| indegree[agent_id.as_str()] == 0)
        .map(String::as_str)
        .collect::<std::collections::VecDeque<_>>();
    let mut visited = 0_usize;
    while let Some(agent_id) = ready.pop_front() {
        visited += 1;
        if let Some(dependents) = children.get(agent_id) {
            for dependent in dependents {
                let remaining = indegree
                    .get_mut(dependent)
                    .expect("workflow dependent initializes indegree");
                *remaining -= 1;
                if *remaining == 0 {
                    ready.push_back(dependent);
                }
            }
        }
    }
    if visited != workflow.agents.len() {
        return Err(ConfigError::InvalidField {
            field: format!("workflows[{}].agents", workflow.id),
            value: format!("{:?}", workflow.agents),
            reason: format!("Workflow '{}' contains a dependency cycle", workflow.id),
            suggestion: "Remove at least one dependency so the team forms a directed acyclic graph"
                .to_string(),
        });
    }
    Ok(())
}

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

/// Structural checks on an Agent's `writes:` list. The daemon checks the full
/// pattern grammar again when it prepares the Agent for a Session.
fn validate_writes(agent_id: &str, writes: &[String]) -> Result<(), ConfigError> {
    const MAX_PATTERNS: usize = 64;
    let field = format!("agents[{agent_id}].writes");
    if writes.len() > MAX_PATTERNS {
        return Err(ConfigError::InvalidField {
            field,
            value: format!("{} paths", writes.len()),
            reason: format!("An Agent may name at most {MAX_PATTERNS} paths it can change"),
            suggestion: "Name directories (lib/) or patterns (*.md) instead of single files"
                .to_string(),
        });
    }
    for (index, pattern) in writes.iter().enumerate() {
        let trimmed = pattern.strip_suffix('/').unwrap_or(pattern);
        let reason = if trimmed.is_empty() {
            Some("An empty path names nothing")
        } else if pattern.starts_with('/') || pattern.contains('\\') {
            Some("Paths are relative to the repository root and use '/'")
        } else if trimmed
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            Some("Paths cannot contain '.', '..' or empty segments")
        } else if trimmed
            .split('/')
            .any(|segment| segment.eq_ignore_ascii_case(".git"))
        {
            Some("Git's own .git directory is never among the paths an Agent may change")
        } else if writes[..index].contains(pattern) {
            Some("This path is listed twice")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(ConfigError::InvalidField {
                field,
                value: format!("{pattern:?}"),
                reason: reason.to_string(),
                suggestion: "Use repository paths such as lib/, docs/*.md or src/**/*.rs; \
                             use writes: [] for an Agent that changes nothing"
                    .to_string(),
            });
        }
    }
    Ok(())
}

/// Validate a parsed config, returning actionable errors.
///
/// Runtime configuration editors must call this before replacing the daemon's
/// validated configuration. Parsing is not the only mutation boundary: an
/// in-memory Agent dependency edit can otherwise create a team graph that a
/// fresh config load would reject.
pub fn validate_config(config: &AxocoatlConfig) -> Result<(), ConfigError> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_shared_labels = std::collections::HashMap::<String, String>::new();

    for agent in &config.agents {
        if agent.id.is_empty() {
            return Err(ConfigError::InvalidField {
                field: "agents[].id".to_string(),
                value: "\"\"".to_string(),
                reason: "Agent ID cannot be empty".to_string(),
                suggestion: "Set a unique identifier like: id: my_agent".to_string(),
            });
        }

        if !is_filesystem_safe_identifier(&agent.id) {
            return Err(ConfigError::InvalidField {
                field: "agents[].id".to_string(),
                value: format!("{:?}", agent.id),
                reason: "Agent ID must be a filesystem-safe ASCII identifier".to_string(),
                suggestion: format!(
                    "Use 1-{MAX_PATH_IDENTIFIER_LEN} ASCII letters, numbers, hyphens, or underscores, starting with a letter or number (for example: id: review-agent_2)"
                ),
            });
        }

        for (block_index, block) in agent.memory.core.blocks.iter().enumerate() {
            if block.shared && !is_filesystem_safe_identifier(&block.label) {
                return Err(ConfigError::InvalidField {
                    field: format!(
                        "agents[{}].memory.core.blocks[{block_index}].label",
                        agent.id
                    ),
                    value: format!("{:?}", block.label),
                    reason: "Shared core-memory block label must be a filesystem-safe ASCII identifier"
                        .to_string(),
                    suggestion: format!(
                        "Use 1-{MAX_PATH_IDENTIFIER_LEN} ASCII letters, numbers, hyphens, or underscores, starting with a letter or number (for example: label: team_notes)"
                    ),
                });
            }
            if block.shared {
                let folded = block.label.to_ascii_lowercase();
                if let Some(existing) = seen_shared_labels.get(&folded) {
                    if existing != &block.label {
                        return Err(ConfigError::InvalidField {
                            field: format!(
                                "agents[{}].memory.core.blocks[{block_index}].label",
                                agent.id
                            ),
                            value: format!("{:?}", block.label),
                            reason: format!(
                                "Shared block labels '{existing}' and '{}' collide on case-insensitive filesystems",
                                block.label
                            ),
                            suggestion: "Use one exact spelling for this shared block label"
                                .to_string(),
                        });
                    }
                } else {
                    seen_shared_labels.insert(folded, block.label.clone());
                }
            }
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

        if let Some(writes) = &agent.writes {
            validate_writes(&agent.id, writes)?;
        }

        if !seen_ids.insert(agent.id.to_ascii_lowercase()) {
            return Err(ConfigError::DuplicateId {
                field: "agents[].id".to_string(),
                id: agent.id.clone(),
            });
        }
    }

    // Workflow ids are durable Session references. Resolve the whole roster at
    // load time so a later Session cannot silently choose the first duplicate,
    // ignore an unknown member, or collapse a malformed coordinator team.
    let agents_by_id = config
        .agents
        .iter()
        .map(|agent| (agent.id.as_str(), agent))
        .collect::<std::collections::HashMap<_, _>>();
    let mut seen_workflow_ids = std::collections::HashSet::new();
    let mut worker_owners = std::collections::HashMap::<&str, &str>::new();
    for (index, workflow) in config.workflows.iter().enumerate() {
        if workflow.id.trim().is_empty() {
            return Err(ConfigError::InvalidField {
                field: format!("workflows[{index}].id"),
                value: format!("{:?}", workflow.id),
                reason: "Workflow ID cannot be empty".to_string(),
                suggestion: "Set a stable unique team identifier, for example: id: feature-team"
                    .to_string(),
            });
        }
        if !seen_workflow_ids.insert(workflow.id.as_str()) {
            return Err(ConfigError::DuplicateId {
                field: "workflows[].id".to_string(),
                id: workflow.id.clone(),
            });
        }
        if workflow.agents.is_empty() {
            return Err(ConfigError::InvalidField {
                field: format!("workflows[{}].agents", workflow.id),
                value: "[]".to_string(),
                reason: format!("Workflow '{}' has no Agent roster", workflow.id),
                suggestion: "Add at least one configured Agent to this workflow".to_string(),
            });
        }

        let mut seen_members = std::collections::HashSet::new();
        for member_id in &workflow.agents {
            if !seen_members.insert(member_id.as_str()) {
                return Err(ConfigError::InvalidField {
                    field: format!("workflows[{}].agents", workflow.id),
                    value: format!("{:?}", workflow.agents),
                    reason: format!(
                        "Workflow '{}' repeats Agent '{}' in its roster",
                        workflow.id, member_id
                    ),
                    suggestion: "List every team member exactly once".to_string(),
                });
            }
            if !agents_by_id.contains_key(member_id.as_str()) {
                return Err(ConfigError::InvalidField {
                    field: format!("workflows[{}].agents", workflow.id),
                    value: format!("{:?}", member_id),
                    reason: format!(
                        "Workflow '{}' member '{}' is not a configured Agent",
                        workflow.id, member_id
                    ),
                    suggestion: format!(
                        "Define Agent '{member_id}' or remove it from this workflow"
                    ),
                });
            }
        }

        let entry = workflow
            .entry_point
            .as_deref()
            .map(|entry_id| {
                agents_by_id
                    .get(entry_id)
                    .copied()
                    .ok_or_else(|| ConfigError::InvalidField {
                        field: format!("workflows[{}].entry_point", workflow.id),
                        value: format!("{entry_id:?}"),
                        reason: format!(
                            "Workflow '{}' entry point '{}' is not a configured Agent",
                            workflow.id, entry_id
                        ),
                        suggestion: format!(
                            "Define Agent '{entry_id}' or choose a configured entry point"
                        ),
                    })
            })
            .transpose()?;

        if let Some(entry) = entry {
            if !seen_members.contains(entry.id.as_str()) {
                let reason = if matches!(entry.role, AgentRoleYaml::Coordinator) {
                    format!(
                        "Coordinator entry point '{}' is not included in workflow '{}'",
                        entry.id, workflow.id
                    )
                } else {
                    format!(
                        "Workflow '{}' entry point '{}' is not included in its Agent roster",
                        workflow.id, entry.id
                    )
                };
                return Err(ConfigError::InvalidField {
                    field: format!("workflows[{}].agents", workflow.id),
                    value: format!("{:?}", workflow.agents),
                    reason,
                    suggestion: format!("Add '{}' to this workflow's agents", entry.id),
                });
            }
        }

        if let Some(entry) = entry.filter(|entry| matches!(entry.role, AgentRoleYaml::Coordinator))
        {
            for member_id in &workflow.agents {
                if member_id == &entry.id {
                    continue;
                }
                let member = agents_by_id[member_id.as_str()];
                if !matches!(member.role, AgentRoleYaml::Worker) {
                    return Err(ConfigError::InvalidField {
                        field: format!("workflows[{}].agents", workflow.id),
                        value: format!("{:?}", member_id),
                        reason: format!(
                            "Coordinator-led workflow '{}' may contain only its Coordinator and Worker Agents; '{}' is {:?}",
                            workflow.id, member_id, member.role
                        ),
                        suggestion: format!(
                            "Change '{member_id}' to role: worker or remove it from this coordinator-led workflow"
                        ),
                    });
                }
                if let Some(existing_owner) = worker_owners.insert(member_id, &workflow.id) {
                    return Err(ConfigError::InvalidField {
                        field: format!("workflows[{}].agents", workflow.id),
                        value: format!("{:?}", member_id),
                        reason: format!(
                            "Worker '{}' belongs to both coordinator-led workflows '{}' and '{}'",
                            member_id, existing_owner, workflow.id
                        ),
                        suggestion: "Give each Coordinator an exclusive Worker Agent identity"
                            .to_string(),
                    });
                }
            }
        } else {
            if entry.is_some_and(|entry| matches!(entry.role, AgentRoleYaml::Worker)) {
                return Err(ConfigError::InvalidField {
                    field: format!("workflows[{}].entry_point", workflow.id),
                    value: format!("{:?}", workflow.entry_point),
                    reason: "A Worker cannot be a workflow entry point".to_string(),
                    suggestion: "Use an autonomous Agent or the owning Coordinator as entry_point"
                        .to_string(),
                });
            }
            for member_id in &workflow.agents {
                let member = agents_by_id[member_id.as_str()];
                if !matches!(member.role, AgentRoleYaml::Autonomous) {
                    return Err(ConfigError::InvalidField {
                        field: format!("workflows[{}].agents", workflow.id),
                        value: format!("{:?}", member_id),
                        reason: format!(
                            "Non-coordinator workflow '{}' may contain autonomous Agents only; '{}' is {:?}",
                            workflow.id, member_id, member.role
                        ),
                        suggestion:
                            "Use only autonomous Agents, or make the workflow's Coordinator its entry_point"
                                .to_string(),
                    });
                }
            }
            validate_autonomous_workflow_graph(workflow, &agents_by_id, &seen_members)?;
        }
    }

    // Role invariants: a coordinator only makes sense as a workflow's entry
    // point, so a half-wired multi-agent setup fails loudly at load time
    // instead of at run. A worker never runs on its own: its workflow's
    // coordinator spawns it, or, outside any workflow, it is a helper template
    // a native Session lead may delegate to once Team and budget approves it.
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
                        reason: "A worker is driven by its coordinator, so it must \
                                 not declare depends_on"
                            .to_string(),
                        suggestion: "Remove depends_on from this worker agent.".to_string(),
                    });
                }
            }
            AgentRoleYaml::Coordinator => {
                let workflow_count = config
                    .workflows
                    .iter()
                    .filter(|workflow| workflow.entry_point.as_deref() == Some(agent.id.as_str()))
                    .count();
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
                if workflow_count > 1 {
                    return Err(ConfigError::InvalidField {
                        field: format!("agents[{}].role", agent.id),
                        value: "coordinator".to_string(),
                        reason: format!(
                            "A coordinator may be the entry_point of exactly one workflow; '{}' is the entry_point of {workflow_count}",
                            agent.id
                        ),
                        suggestion: format!(
                            "Keep '{}' as entry_point of one workflow and give every other coordinator-led workflow its own coordinator.",
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

    validate_sandbox_network(&config.sandbox.network)?;

    for webhook in &config.webhooks {
        if webhook.name.trim().is_empty() {
            return Err(ConfigError::InvalidField {
                field: "webhooks[].name".to_string(),
                value: "\"\"".to_string(),
                reason: "Webhook name cannot be empty".to_string(),
                suggestion: "Give each webhook a name, e.g. name: slack-alerts".to_string(),
            });
        }
        if !(webhook.url.starts_with("http://") || webhook.url.starts_with("https://")) {
            return Err(ConfigError::InvalidField {
                field: format!("webhooks[{}].url", webhook.name),
                value: webhook.url.clone(),
                reason: "Webhook URL must be an http(s) URL".to_string(),
                suggestion: "Use a full URL, e.g. url: https://hooks.example.com/axocoatl"
                    .to_string(),
            });
        }
    }

    Ok(())
}

/// The only values `sandbox.network` accepts.
pub const SANDBOX_NETWORK_VALUES: [&str; 2] = ["bridge", "none"];

/// Refuse any `sandbox.network` other than exactly `bridge` or `none`. Any
/// other spelling (`None`, `off`, `disabled`, ...) is an error rather than a
/// silent bridge network.
pub fn validate_sandbox_network(value: &str) -> Result<(), ConfigError> {
    if SANDBOX_NETWORK_VALUES.contains(&value) {
        return Ok(());
    }
    Err(ConfigError::InvalidField {
        field: "sandbox.network".to_string(),
        value: format!("{value:?}"),
        reason: "sandbox.network accepts only \"bridge\" or \"none\", in lowercase".to_string(),
        suggestion: "Set network: none for no container network, or network: bridge to allow \
                     outbound connections"
            .to_string(),
    })
}

/// An Autonomous Agent or Coordinator whose `tools` list is empty. In a native
/// Session the list is exact, so such an Agent gets no repository tools; only
/// a legacy (1.0-format) Session still inherits the baseline for an empty
/// list. A warning, not an error: an Agent that only answers is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoToolsWarning {
    pub agent_id: String,
}

impl NoToolsWarning {
    /// What to do about it, shown after the problem.
    pub const HINT: &'static str = "List the tools it needs, for example [read_file, list_dir, \
                                    grep, glob, write_file, edit_file, bash].";

    /// The problem, without the hint.
    pub fn problem(&self) -> String {
        format!(
            "{} lists no tools: in native Sessions it cannot read or change files.",
            self.agent_id
        )
    }
}

impl std::fmt::Display for NoToolsWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.problem(), Self::HINT)
    }
}

/// One warning for each Agent that is not a Worker and lists no tools, in
/// configuration order. `validate`, `doctor` and daemon startup report them.
pub fn no_tools_warnings(config: &AxocoatlConfig) -> Vec<NoToolsWarning> {
    config
        .agents
        .iter()
        .filter(|agent| !matches!(agent.role, AgentRoleYaml::Worker) && agent.tools.is_empty())
        .map(|agent| NoToolsWarning {
            agent_id: agent.id.clone(),
        })
        .collect()
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
        format!("A required field is missing. {error_msg}")
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
    fn parse_writes_scope() {
        let yaml = r#"
agents:
  - id: open
    name: "Open"
    provider: ollama
    model: llama3
  - id: scoped
    name: "Scoped"
    provider: ollama
    model: llama3
    writes: [lib/, "docs/*.md"]
  - id: helper
    name: "Helper"
    provider: ollama
    model: llama3
    writes: []
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(config.agents[0].writes, None);
        assert_eq!(
            config.agents[1].writes,
            Some(vec!["lib/".to_string(), "docs/*.md".to_string()])
        );
        assert_eq!(config.agents[2].writes, Some(vec![]));
        assert_eq!(config.agents[1].to_core().writes, config.agents[1].writes);
        assert_eq!(config.agents[2].to_core().writes, Some(vec![]));
        // An Agent without the key writes back without it.
        let written = serde_yaml::to_string(&config.agents[0]).unwrap();
        assert!(!written.contains("writes"), "{written}");

        for bad in [
            "[../outside]",
            "[/etc]",
            "[lib/, lib/]",
            "[\"\"]",
            "[.git/]",
            "[.GIT/hooks/pre-commit]",
            "[vendor/.git/]",
        ] {
            let yaml = format!(
                "agents:\n  - id: bad\n    name: Bad\n    provider: ollama\n    model: llama3\n    writes: {bad}\n"
            );
            let err = parse_config(&yaml, &PathBuf::from("test.yaml")).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidField { ref field, .. } if field == "agents[bad].writes"),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn removed_activation_keys_still_parse_so_they_can_be_reported() {
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
        // A 1.0 config that still sets them keeps loading; the daemon warns.
        assert_eq!(config.agents[0].activation_threshold, Some(0.75));
        assert_eq!(config.agents[0].activation_decay, Some(0.05));
        assert_eq!(config.agents[1].activation_threshold, None);
        assert_eq!(config.agents[1].activation_decay, None);
    }

    #[test]
    fn agents_that_are_not_workers_and_list_no_tools_are_reported() {
        let yaml = r#"
agents:
  - id: chat
    name: "Chat"
    provider: ollama
    model: llama3
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
    role: coordinator
    tools: []
  - id: coder
    name: "Coder"
    provider: ollama
    model: llama3
    tools: [read_file, bash]
  - id: helper
    name: "Helper"
    provider: ollama
    model: llama3
    role: worker
workflows:
  - id: wf
    name: "WF"
    agents: [lead, helper]
    entry_point: lead
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        let warnings = no_tools_warnings(&config);
        let ids: Vec<&str> = warnings.iter().map(|w| w.agent_id.as_str()).collect();
        // A Worker with no tools and an Agent that lists tools are not reported.
        assert_eq!(ids, vec!["chat", "lead"]);
        assert_eq!(
            warnings[0].to_string(),
            "chat lists no tools: in native Sessions it cannot read or change files. List the \
             tools it needs, for example [read_file, list_dir, grep, glob, write_file, \
             edit_file, bash]."
        );
    }

    #[test]
    fn removed_skill_keys_still_parse_so_they_can_be_reported() {
        let yaml = r#"
agents:
  - id: coder
    name: "Coder"
    provider: ollama
    model: llama3
skills:
  - id: legacy
    name: "Legacy"
    description: "A 1.0 Skill"
    emits: [CodeReady]
    reacts_to: [ReviewRequested]
    agents: [coder]
    prompt: "Review the change."
  - id: current
    name: "Current"
    description: "A 1.1 Skill"
    emits: [ReviewRequested]
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        // A 1.0 config that still sets them keeps loading; the daemon warns.
        assert_eq!(
            config.skills[0].removed_keys(),
            vec!["reacts_to", "agents", "prompt"]
        );
        assert_eq!(config.skills[0].emits, vec!["CodeReady".to_string()]);
        assert!(config.skills[1].removed_keys().is_empty());
    }

    #[test]
    fn removed_htn_methods_file_still_parses_so_it_can_be_reported() {
        let yaml = r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
    role: coordinator
workflows:
  - id: wf
    name: "WF"
    agents: [lead]
    entry_point: lead
    htn_methods_file: methods.yaml
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        // A 1.0 config that still sets it keeps loading; the daemon warns.
        assert_eq!(
            config.workflows[0].htn_methods_file.as_deref(),
            Some("methods.yaml")
        );
    }

    #[test]
    fn worker_with_depends_on_rejected() {
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
    depends_on: [x]
workflows:
  - id: wf
    name: "WF"
    agents: [lead, w]
    entry_point: lead
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidField { ref field, .. } if field.contains("depends_on"))
        );
    }

    /// A Worker outside any workflow is a helper template: a native Session
    /// lead can delegate to it once Team and budget approves it.
    #[test]
    fn worker_outside_any_workflow_is_a_helper_template() {
        let yaml = r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
  - id: w
    name: "W"
    provider: ollama
    model: llama3
    role: worker
    writes: []
"#;
        let config = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap();
        assert!(matches!(config.agents[1].role, AgentRoleYaml::Worker));
        assert_eq!(config.agents[1].writes, Some(vec![]));
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
    fn coordinator_cannot_own_multiple_workflows() {
        let yaml = r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
    role: coordinator
workflows:
  - id: first
    name: "First"
    agents: [lead]
    entry_point: lead
  - id: second
    name: "Second"
    agents: [lead]
    entry_point: lead
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidField { ref reason, .. }
                if reason.contains("entry_point of exactly one workflow")
        ));
    }

    #[test]
    fn workflow_ids_must_be_nonempty_and_unique() {
        let empty = parse_config(
            r#"
workflows:
  - id: "  "
    name: "Unnamed"
    agents: []
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(empty.contains("Workflow ID cannot be empty"), "{empty}");

        let duplicate = parse_config(
            r#"
agents:
  - id: known
    name: "Known"
    provider: ollama
    model: llama3
workflows:
  - id: repeated
    name: "First"
    agents: [known]
    entry_point: known
  - id: repeated
    name: "Second"
    agents: [known]
    entry_point: known
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err();
        assert!(matches!(
            duplicate,
            ConfigError::DuplicateId { ref field, ref id }
                if field == "workflows[].id" && id == "repeated"
        ));
    }

    #[test]
    fn workflow_ids_may_use_human_or_punctuated_config_keys() {
        let config = parse_config(
            r#"
agents:
  - id: known
    name: "Known"
    provider: ollama
    model: llama3
workflows:
  - id: review.v1
    name: "Review"
    agents: [known]
    entry_point: known
  - id: Release Team
    name: "Release"
    agents: [known]
    entry_point: known
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap();
        assert_eq!(config.workflows[0].id, "review.v1");
        assert_eq!(config.workflows[1].id, "Release Team");
    }

    #[test]
    fn workflow_roster_cannot_be_empty() {
        let error = parse_config(
            r#"
workflows:
  - id: empty-team
    name: "Empty"
    agents: []
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("has no Agent roster"), "{error}");
    }

    #[test]
    fn workflow_members_and_entry_points_must_resolve_exactly() {
        let unknown_member = parse_config(
            r#"
agents:
  - id: known
    name: "Known"
    provider: ollama
    model: llama3
workflows:
  - id: team
    name: "Team"
    agents: [known, missing]
    entry_point: known
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(unknown_member.contains("member 'missing' is not a configured Agent"));

        let unknown_entry = parse_config(
            r#"
agents:
  - id: known
    name: "Known"
    provider: ollama
    model: llama3
workflows:
  - id: team
    name: "Team"
    agents: [known]
    entry_point: missing
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(unknown_entry.contains("entry point 'missing' is not a configured Agent"));

        let repeated_member = parse_config(
            r#"
agents:
  - id: known
    name: "Known"
    provider: ollama
    model: llama3
workflows:
  - id: team
    name: "Team"
    agents: [known, known]
    entry_point: known
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(repeated_member.contains("repeats Agent 'known'"));

        let out_of_roster_entry = parse_config(
            r#"
agents:
  - id: member
    name: "Member"
    provider: ollama
    model: llama3
  - id: entry
    name: "Entry"
    provider: ollama
    model: llama3
workflows:
  - id: team
    name: "Team"
    agents: [member]
    entry_point: entry
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(out_of_roster_entry.contains("entry point 'entry' is not included"));
    }

    #[test]
    fn autonomous_workflow_dependency_graph_must_be_closed_and_acyclic() {
        let outside = parse_config(
            r#"
agents:
  - { id: root, name: "Root", provider: ollama, model: llama3 }
  - { id: omitted, name: "Omitted", provider: ollama, model: llama3 }
  - id: child
    name: "Child"
    provider: ollama
    model: llama3
    depends_on: [omitted]
workflows:
  - id: team
    name: "Team"
    agents: [root, child]
    entry_point: root
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(outside.contains("depends on 'omitted', which is outside workflow 'team'"));

        let duplicate = parse_config(
            r#"
agents:
  - { id: root, name: "Root", provider: ollama, model: llama3 }
  - id: child
    name: "Child"
    provider: ollama
    model: llama3
    depends_on: [root, root]
workflows:
  - id: team
    name: "Team"
    agents: [root, child]
    entry_point: root
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(duplicate.contains("repeats dependency 'root'"));

        let self_dependency = parse_config(
            r#"
agents:
  - id: self
    name: "Self"
    provider: ollama
    model: llama3
    depends_on: [self]
workflows:
  - id: team
    name: "Team"
    agents: [self]
    entry_point: self
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(self_dependency.contains("cannot depend on itself"));

        let cycle = parse_config(
            r#"
agents:
  - id: one
    name: "One"
    provider: ollama
    model: llama3
    depends_on: [two]
  - id: two
    name: "Two"
    provider: ollama
    model: llama3
    depends_on: [one]
workflows:
  - id: team
    name: "Team"
    agents: [one, two]
    entry_point: one
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(cycle.contains("contains a dependency cycle"));

        let diamond = parse_config(
            r#"
agents:
  - { id: root, name: "Root", provider: ollama, model: llama3 }
  - id: left
    name: "Left"
    provider: ollama
    model: llama3
    depends_on: [root]
  - id: right
    name: "Right"
    provider: ollama
    model: llama3
    depends_on: [root]
  - id: sink
    name: "Sink"
    provider: ollama
    model: llama3
    depends_on: [left, right]
workflows:
  - id: diamond
    name: "Diamond"
    agents: [root, left, right, sink]
    entry_point: root
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap();
        assert_eq!(diamond.workflows[0].agents.len(), 4);
    }

    #[test]
    fn coordinator_workflow_roster_is_exact_and_worker_ownership_is_exclusive() {
        let missing_coordinator = parse_config(
            r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
    role: coordinator
  - id: worker
    name: "Worker"
    provider: ollama
    model: llama3
    role: worker
workflows:
  - id: team
    name: "Team"
    agents: [worker]
    entry_point: lead
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing_coordinator.contains("Coordinator entry point 'lead' is not included"),
            "{missing_coordinator}"
        );

        for (other_role, expected_agent) in [("", "autonomous"), ("coordinator", "other-lead")] {
            let yaml = format!(
                r#"
agents:
  - id: lead
    name: "Lead"
    provider: ollama
    model: llama3
    role: coordinator
  - id: {expected_agent}
    name: "Other"
    provider: ollama
    model: llama3
    {role_line}
workflows:
  - id: team
    name: "Team"
    agents: [lead, {expected_agent}]
    entry_point: lead
"#,
                role_line = if other_role.is_empty() {
                    String::new()
                } else {
                    format!("role: {other_role}")
                }
            );
            let error = parse_config(&yaml, &PathBuf::from("test.yaml"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("may contain only its Coordinator and Worker Agents"));
            assert!(error.contains(expected_agent));
        }

        let shared_worker = parse_config(
            r#"
agents:
  - id: lead-one
    name: "Lead one"
    provider: ollama
    model: llama3
    role: coordinator
  - id: lead-two
    name: "Lead two"
    provider: ollama
    model: llama3
    role: coordinator
  - id: worker
    name: "Worker"
    provider: ollama
    model: llama3
    role: worker
workflows:
  - id: team-one
    name: "Team one"
    agents: [lead-one, worker]
    entry_point: lead-one
  - id: team-two
    name: "Team two"
    agents: [lead-two, worker]
    entry_point: lead-two
"#,
            &PathBuf::from("test.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(shared_worker.contains("belongs to both coordinator-led workflows"));
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
    fn validate_agent_id_storage_boundary() {
        fn config_with_agent_id(id: &str) -> String {
            format!(
                "agents:\n  - id: {}\n    name: Test\n    provider: ollama\n    model: llama3\n",
                serde_json::to_string(id).unwrap()
            )
        }

        let sixty_four_chars = format!("a{}", "1".repeat(63));
        for id in ["a", "Coder1", "review-agent_2", sixty_four_chars.as_str()] {
            parse_config(&config_with_agent_id(id), &PathBuf::from("test.yaml"))
                .unwrap_or_else(|error| panic!("valid agent id {id:?} was rejected: {error}"));
        }

        let sixty_five_chars = format!("a{}", "1".repeat(64));
        for id in [
            "../escape",
            "agent/name",
            r"agent\name",
            ".",
            "..",
            "/tmp/escape",
            "-leading-hyphen",
            "_leading-underscore",
            "trailing-space ",
            "embedded space",
            "agent.name",
            "agent:name",
            "agent#name",
            "café",
            "control\u{001f}",
            sixty_five_chars.as_str(),
        ] {
            let error =
                parse_config(&config_with_agent_id(id), &PathBuf::from("test.yaml")).unwrap_err();
            assert!(
                matches!(error, ConfigError::InvalidField { ref field, .. } if field == "agents[].id"),
                "unsafe agent id {id:?} failed for the wrong reason: {error}"
            );
        }
    }

    #[test]
    fn validate_shared_core_block_label_storage_boundary() {
        fn config_with_shared_label(label: &str) -> String {
            format!(
                "agents:\n  - id: coder\n    name: Test\n    provider: ollama\n    model: llama3\n    memory:\n      core:\n        blocks:\n          - label: {}\n            shared: true\n",
                serde_json::to_string(label).unwrap()
            )
        }

        let sixty_four_chars = format!("t{}", "1".repeat(63));
        for label in [
            "team",
            "Project1",
            "team-notes_2",
            sixty_four_chars.as_str(),
        ] {
            parse_config(
                &config_with_shared_label(label),
                &PathBuf::from("test.yaml"),
            )
            .unwrap_or_else(|error| {
                panic!("valid shared block label {label:?} was rejected: {error}")
            });
        }

        let sixty_five_chars = format!("t{}", "1".repeat(64));
        for label in [
            "",
            "../outside",
            "team/notes",
            r"team\notes",
            ".",
            "..",
            "/tmp/outside",
            "-team",
            "team notes",
            "team.notes",
            "team:notes",
            "team#notes",
            "téam",
            "control\u{001f}",
            sixty_five_chars.as_str(),
        ] {
            let error = parse_config(
                &config_with_shared_label(label),
                &PathBuf::from("test.yaml"),
            )
            .unwrap_err();
            assert!(
                matches!(error, ConfigError::InvalidField { ref field, .. } if field.contains("memory.core.blocks") && field.ends_with(".label")),
                "unsafe shared block label {label:?} failed for the wrong reason: {error}"
            );
        }
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
    fn validate_rejects_casefolded_agent_id_collision() {
        let yaml = r#"
agents:
  - id: Coder
    name: "First"
    provider: openai
    model: gpt-4o
  - id: coder
    name: "Second"
    provider: openai
    model: gpt-4o
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("Duplicate ID"));
    }

    #[test]
    fn validate_rejects_casefolded_shared_label_collision() {
        let yaml = r#"
agents:
  - id: first
    name: "First"
    provider: openai
    model: gpt-4o
    memory:
      core:
        blocks:
          - label: Team
            shared: true
  - id: second
    name: "Second"
    provider: openai
    model: gpt-4o
    memory:
      core:
        blocks:
          - label: team
            shared: true
"#;
        let err = parse_config(yaml, &PathBuf::from("test.yaml")).unwrap_err();
        assert!(err
            .to_string()
            .contains("collide on case-insensitive filesystems"));
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
    fn sandbox_network_accepts_only_bridge_or_none() {
        for accepted in ["bridge", "none"] {
            let yaml = format!("sandbox:\n  network: {accepted}\n");
            let config = parse_config(&yaml, &PathBuf::from("test.yaml")).unwrap();
            assert_eq!(config.sandbox.network, accepted);
        }
        let default = parse_config("agents: []\n", &PathBuf::from("test.yaml")).unwrap();
        assert_eq!(default.sandbox.network, "bridge");

        for refused in [
            "None",
            "NONE",
            "off",
            "disabled",
            "host",
            "\"\"",
            "\" none\"",
        ] {
            let yaml = format!("sandbox:\n  network: {refused}\n");
            let err = parse_config(&yaml, &PathBuf::from("test.yaml")).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("sandbox.network") && message.contains("\"bridge\" or \"none\""),
                "{refused}: {message}"
            );
        }

        let mut config = parse_config("agents: []\n", &PathBuf::from("test.yaml")).unwrap();
        config.sandbox.network = "off".to_string();
        assert!(validate_config(&config).is_err());
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
