//! Step 4: the user configuration onboarding writes.
//!
//! A new configuration holds the provider the writer and reviewer need, the
//! default team for workbench Sessions, `run.models` (the defaults
//! `axocoatl run` gives `writer_model` and `reviewer_model`) and the server
//! port. "Update this setup" keeps an existing file and changes only what is
//! missing or what this run chose: a provider section it needs, `run`, and
//! `server.port`; everything else, comments included, stays as it was. The
//! old file is backed up first and the change is shown as a diff.

use serde::Deserialize;

/// A provider native Sessions and runs accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Ollama,
    OpenRouter,
}

impl Provider {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Provider::Ollama => "ollama",
            Provider::OpenRouter => "openrouter",
        }
    }
}

/// `provider:model` as `axocoatl run --model ROLE=…` and `run.models` write
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelRef {
    pub(crate) provider: Provider,
    pub(crate) model: String,
}

impl ModelRef {
    pub(crate) fn new(provider: Provider, model: &str) -> Self {
        Self {
            provider,
            model: model.to_string(),
        }
    }

    /// Read `ollama:qwen3-coder:30b` or `openrouter:qwen/qwen3-coder`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let (provider, model) = text
            .split_once(':')
            .ok_or_else(|| format!("{text:?}: write PROVIDER:MODEL, such as ollama:gpt-oss:20b"))?;
        let provider = match provider {
            "ollama" => Provider::Ollama,
            "openrouter" => Provider::OpenRouter,
            other => {
                return Err(format!(
                    "{text:?}: the provider {other:?} cannot run native Agents; use ollama:MODEL \
                     or openrouter:VENDOR/MODEL (Claude Code and Codex can write but not review)"
                ))
            }
        };
        if model.trim().is_empty() {
            return Err(format!("{text:?}: the model is missing"));
        }
        Ok(Self::new(provider, model.trim()))
    }

    pub(crate) fn spec(&self) -> String {
        format!("{}:{}", self.provider.id(), self.model)
    }
}

/// Where the OpenRouter key comes from. A value is written to the
/// owner-only config; a variable is written as `${VAR}` and read from the
/// environment of whatever starts Axocoatl.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum KeyRef {
    Value(zeroize::Zeroizing<String>),
    Env(String),
}

impl std::fmt::Debug for KeyRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyRef::Value(_) => formatter.write_str("KeyRef::Value(<hidden>)"),
            KeyRef::Env(name) => write!(formatter, "KeyRef::Env({name})"),
        }
    }
}

impl KeyRef {
    fn yaml(&self) -> String {
        match self {
            KeyRef::Value(key) => yaml_string(key),
            KeyRef::Env(name) => yaml_string(&format!("${{{name}}}")),
        }
    }
}

/// What onboarding chose, to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigPlan {
    pub(crate) ollama_base_url: Option<String>,
    pub(crate) openrouter_key: Option<KeyRef>,
    /// The workbench team: Lead, Scout and the chat Assistant run on `lead`,
    /// Reviewer on `reviewer`.
    pub(crate) lead: ModelRef,
    pub(crate) reviewer: ModelRef,
    /// `run.models.writer`: absent when the writer is Claude Code or Codex
    /// (their loadouts name their own writer model).
    pub(crate) run_writer: Option<ModelRef>,
    pub(crate) run_reviewer: Option<ModelRef>,
    pub(crate) port: u16,
    /// A comment at the top, saying what the setup is for.
    pub(crate) note: String,
}

pub(crate) fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

pub(crate) const LEAD_PROMPT: &str =
    "You own this change. 1. Look first: find the relevant files and \
    tests for code you do not know. If you have helpers, ask scout to find them. 2. Make the \
    change. 3. Run the check command from the project instructions (AXOCOATL.md), or the tests, \
    and fix failures. 4. Check your diff against the task. If you have helpers, ask reviewer to \
    review it. Fix each defect found. Then summarize what you changed.";
pub(crate) const SCOUT_PROMPT: &str =
    "Answer the lead's question with file paths, line numbers and short \
    evidence. Change nothing.";
pub(crate) const REVIEWER_PROMPT: &str =
    "Review the described change. Check it against the task and \
    against every contract and edge case the docs, comments and tests describe, one by one. \
    Report each defect with file:line, or say you found none. Change nothing.";

/// One Agent's provider, already-serialized model and output limit.
pub(crate) struct AgentModel<'a> {
    pub(crate) provider: &'a str,
    pub(crate) model_yaml: &'a str,
    pub(crate) max_tokens: Option<u32>,
}

fn sampling(max_tokens: Option<u32>) -> String {
    max_tokens
        .map(|max_tokens| format!("    sampling:\n      max_tokens: {max_tokens}\n"))
        .unwrap_or_default()
}

/// The default team in every starting configuration: Lead owns the change
/// and is the only Agent that edits files; Scout and Reviewer are Workers with
/// `writes: []`, read-only helpers Lead can delegate to. A new Session on Lead
/// starts with Lead alone; Team & budget offers both helpers as an option,
/// and the person approves every limit. Lead and Scout run on `lead`,
/// Reviewer on `reviewer`.
pub(crate) fn team_agents(lead: &AgentModel, reviewer: &AgentModel) -> String {
    let lead_sampling = sampling(lead.max_tokens);
    let reviewer_sampling = sampling(reviewer.max_tokens);
    format!(
        r#"  # Default team. Lead owns the change; Scout and Reviewer are read-only
  # helpers it can delegate to once you opt in and approve their limits in
  # Team & budget. A new Session starts with Lead alone.
  - id: lead
    name: "Lead"
    provider: {lead_provider}
    model: {lead_model}
    role: autonomous
    tools: [read_file, list_dir, grep, glob, write_file, edit_file, bash]
    system_prompt: "{LEAD_PROMPT}"
{lead_sampling}
  - id: scout
    name: "Scout"
    provider: {lead_provider}
    model: {lead_model}
    role: worker
    tools: [read_file, list_dir, grep, glob, bash]
    writes: []
    system_prompt: "{SCOUT_PROMPT}"
{lead_sampling}
  - id: reviewer
    name: "Reviewer"
    provider: {reviewer_provider}
    model: {reviewer_model}
    role: worker
    tools: [read_file, list_dir, grep, glob, bash]
    writes: []
    system_prompt: "{REVIEWER_PROMPT}"
{reviewer_sampling}"#,
        lead_provider = lead.provider,
        lead_model = lead.model_yaml,
        reviewer_provider = reviewer.provider,
        reviewer_model = reviewer.model_yaml,
    )
}

/// Output limits: Ollama Agents 4096 tokens; on OpenRouter the writer
/// (Lead, Scout) 4096 and the reviewer and Assistant 2048.
fn max_tokens(provider: Provider, writer: bool) -> u32 {
    match (provider, writer) {
        (Provider::Ollama, _) => 4096,
        (Provider::OpenRouter, true) => 4096,
        (Provider::OpenRouter, false) => 2048,
    }
}

/// The `providers:` lines (without the key).
fn providers_block(plan: &ConfigPlan) -> String {
    let mut text = String::from("providers:\n");
    if plan.openrouter_key.is_some() {
        text.push_str("  openrouter_billing: credits\n");
    }
    if let Some(base_url) = &plan.ollama_base_url {
        text.push_str(&format!(
            "  ollama:\n    base_url: {}\n",
            yaml_string(base_url)
        ));
    }
    if let Some(key) = &plan.openrouter_key {
        text.push_str(&format!("  openrouter:\n    api_key: {}\n", key.yaml()));
    }
    text
}

/// The `run:` block: the defaults `axocoatl run` gives the loadout
/// parameters `writer_model` and `reviewer_model`; `--model ROLE=…` on the
/// command line overrides them.
pub(crate) fn run_block(plan: &ConfigPlan) -> String {
    if plan.run_writer.is_none() && plan.run_reviewer.is_none() {
        return String::new();
    }
    let mut text = String::from(
        "# Defaults for `axocoatl run`: writer_model and reviewer_model.\n\
         # `--model writer=PROVIDER:MODEL` or `--model reviewer=…` overrides them.\n\
         run:\n  models:\n",
    );
    if let Some(writer) = &plan.run_writer {
        text.push_str(&format!("    writer: {}\n", yaml_string(&writer.spec())));
    }
    if let Some(reviewer) = &plan.run_reviewer {
        text.push_str(&format!(
            "    reviewer: {}\n",
            yaml_string(&reviewer.spec())
        ));
    }
    text
}

fn server_block(port: u16) -> String {
    format!("server:\n  port: {port}\n  host: \"127.0.0.1\"\n")
}

fn agents_block(plan: &ConfigPlan) -> String {
    let lead_yaml = yaml_string(&plan.lead.model);
    let reviewer_yaml = yaml_string(&plan.reviewer.model);
    let team = team_agents(
        &AgentModel {
            provider: plan.lead.provider.id(),
            model_yaml: &lead_yaml,
            max_tokens: Some(max_tokens(plan.lead.provider, true)),
        },
        &AgentModel {
            provider: plan.reviewer.provider.id(),
            model_yaml: &reviewer_yaml,
            max_tokens: Some(max_tokens(plan.reviewer.provider, false)),
        },
    );
    let assistant_sampling = match plan.lead.provider {
        Provider::Ollama => String::new(),
        Provider::OpenRouter => sampling(Some(2048)),
    };
    format!(
        r#"agents:
{team}
  # Plain chat for `axocoatl chat`; not part of the default team.
  - id: assistant
    name: "Assistant"
    provider: {provider}
    model: {lead_yaml}
    system_prompt: "You are a helpful assistant."
    tools: [read_file, list_dir, grep, glob]
{assistant_sampling}    token_budget:
      per_execution: 16000
      per_call: 8192
      overflow_policy: warn
"#,
        provider = plan.lead.provider.id(),
    )
}

/// A whole new configuration.
pub(crate) fn render(plan: &ConfigPlan) -> String {
    let openrouter_note = if plan.openrouter_key.is_some() {
        "# OpenRouter uses your normal API key and OpenRouter credits.\n\
         # BYOK is not supported for native Session execution.\n"
    } else {
        ""
    };
    let run = run_block(plan);
    format!(
        "# Axocoatl — {note}\n{openrouter_note}{agents}\n{providers}\n{run}{run_gap}{server}",
        note = plan.note,
        agents = agents_block(plan),
        providers = providers_block(plan),
        run_gap = if run.is_empty() { "" } else { "\n" },
        server = server_block(plan.port),
    )
}

/// What an existing configuration says, read leniently.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Existing {
    #[serde(default)]
    pub(crate) agents: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) providers: Option<ExistingProviders>,
    #[serde(default)]
    pub(crate) server: Option<ExistingServer>,
    #[serde(default)]
    pub(crate) run: Option<RunSection>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExistingProviders {
    #[serde(default)]
    pub(crate) ollama: Option<ExistingOllama>,
    #[serde(default)]
    pub(crate) openrouter: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExistingOllama {
    #[serde(default)]
    pub(crate) base_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExistingServer {
    #[serde(default)]
    pub(crate) port: Option<u16>,
}

/// `run:` in a configuration: the defaults of `axocoatl run`.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct RunSection {
    #[serde(default)]
    pub(crate) models: RunModels,
}

#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct RunModels {
    #[serde(default)]
    pub(crate) writer: Option<String>,
    #[serde(default)]
    pub(crate) reviewer: Option<String>,
}

pub(crate) fn read_existing(text: &str) -> Result<Existing, String> {
    axocoatl_config::yaml::from_str::<Existing>(text).map_err(|error| error.to_string())
}

/// The top-level block `key:` in `lines`: its first line and the line after
/// its last (comments and blank lines before the next top-level key belong
/// to the next block).
fn top_block(lines: &[String], key: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|rest| rest.trim().is_empty() || rest.trim_start().starts_with('#'))
    })?;
    let mut end = start + 1;
    let mut last_content = start + 1;
    while end < lines.len() {
        let line = &lines[end];
        let top_level = !line.is_empty()
            && !line.starts_with(' ')
            && !line.starts_with('\t')
            && !line.starts_with('#');
        if top_level {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            last_content = end + 1;
        }
        end += 1;
    }
    Some((start, last_content))
}

/// The child block `  key:` (two-space indent) inside `range`.
fn child_block(lines: &[String], range: (usize, usize), key: &str) -> Option<(usize, usize)> {
    let prefix = format!("  {key}:");
    let start = (range.0 + 1..range.1).find(|&index| {
        lines[index]
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.trim().is_empty() || rest.trim_start().starts_with('#'))
    })?;
    let mut end = start + 1;
    while end < range.1 {
        let line = &lines[end];
        if line.trim().is_empty() || line.starts_with("    ") || line.trim_start().starts_with('#')
        {
            end += 1;
        } else {
            break;
        }
    }
    Some((start, end))
}

fn to_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

fn from_lines(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// "Update this setup": `old` with only what this run needs changed. Fails
/// when `old` cannot be read, or when the result would not load.
pub(crate) fn update(
    old: &str,
    plan: &ConfigPlan,
    path: &std::path::Path,
) -> Result<String, String> {
    let existing = read_existing(old).map_err(|error| {
        format!("the existing configuration cannot be read ({error}); choose Replace instead")
    })?;
    let mut lines = to_lines(old);

    // Agents: only when there are none (no `agents:`, or an empty one such
    // as `agents: []`, which the team replaces).
    if existing.agents.is_empty() {
        let team = to_lines(agents_block(plan).trim_end());
        match lines.iter().position(|line| line.starts_with("agents:")) {
            Some(start) => {
                let end = top_block(&lines, "agents").map_or(start + 1, |(_, end)| end);
                lines.splice(start..end, team);
            }
            None => {
                lines.push(String::new());
                lines.extend(team);
            }
        }
    }

    // Providers: add the sections this setup needs and does not have.
    let has_ollama = existing
        .providers
        .as_ref()
        .and_then(|providers| providers.ollama.as_ref())
        .is_some();
    let has_openrouter = existing
        .providers
        .as_ref()
        .and_then(|providers| providers.openrouter.as_ref())
        .is_some();
    let mut missing_providers = Vec::new();
    if let Some(base_url) = &plan.ollama_base_url {
        let configured = existing
            .providers
            .as_ref()
            .and_then(|providers| providers.ollama.as_ref())
            .and_then(|ollama| ollama.base_url.clone());
        if !has_ollama {
            missing_providers.push(format!(
                "  ollama:\n    base_url: {}",
                yaml_string(base_url)
            ));
        } else if configured.as_deref() != Some(base_url.as_str()) {
            // This run chose another server: point the section at it.
            if let Some(providers) = top_block(&lines, "providers") {
                if let Some(ollama) = child_block(&lines, providers, "ollama") {
                    if let Some(index) = (ollama.0 + 1..ollama.1)
                        .find(|&index| lines[index].trim_start().starts_with("base_url:"))
                    {
                        lines[index] = format!("    base_url: {}", yaml_string(base_url));
                    }
                }
            }
        }
    }
    if let (Some(key), false) = (&plan.openrouter_key, has_openrouter) {
        missing_providers.push(format!(
            "  openrouter_billing: credits\n  openrouter:\n    api_key: {}",
            key.yaml()
        ));
    }
    if !missing_providers.is_empty() {
        let addition = to_lines(&missing_providers.join("\n"));
        match top_block(&lines, "providers") {
            Some((start, _)) => {
                for (offset, line) in addition.into_iter().enumerate() {
                    lines.insert(start + 1 + offset, line);
                }
            }
            None => {
                lines.push(String::new());
                lines.push("providers:".to_string());
                lines.extend(addition);
            }
        }
    }

    // run: replaced as a whole, where it is, with what this run chose.
    let run = run_block(plan);
    if !run.is_empty() {
        match top_block(&lines, "run") {
            Some((start, end)) => {
                // With the comment lines written just above it.
                let mut first = start;
                while first > 0 && lines[first - 1].starts_with("# ") {
                    first -= 1;
                }
                lines.splice(first..end, to_lines(&run));
            }
            None => {
                while lines.last().is_some_and(|line| line.trim().is_empty()) {
                    lines.pop();
                }
                lines.push(String::new());
                lines.extend(to_lines(&run));
            }
        }
    }

    // server.port
    let configured_port = existing.server.as_ref().and_then(|server| server.port);
    if configured_port != Some(plan.port) {
        match top_block(&lines, "server") {
            Some(server) => {
                match (server.0 + 1..server.1).find(|&index| lines[index].starts_with("  port:")) {
                    Some(index) => lines[index] = format!("  port: {}", plan.port),
                    None => lines.insert(server.0 + 1, format!("  port: {}", plan.port)),
                }
            }
            None if plan.port != 8080 || configured_port.is_some() => {
                lines.push(String::new());
                lines.extend(to_lines(&server_block(plan.port)));
            }
            None => {}
        }
    }

    let text = from_lines(&lines);
    axocoatl_config::parse_config(&text, path)
        .map_err(|error| format!("the updated configuration would not load: {error}"))?;
    Ok(text)
}

/// A line diff of `old` and `new`: `-` removed, `+` added, unchanged lines
/// left out.
pub(crate) fn diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut lines = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            lines.push(format!("- {}", a[i]));
            i += 1;
        } else {
            lines.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    lines.extend(a[i..].iter().map(|line| format!("- {line}")));
    lines.extend(b[j..].iter().map(|line| format!("+ {line}")));
    lines
}

/// The models `run:` in a configuration file names, read without the
/// rest of the configuration.
pub(crate) fn run_models(text: &str) -> RunModels {
    read_existing(text)
        .ok()
        .and_then(|existing| existing.run)
        .map(|run| run.models)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn local_plan() -> ConfigPlan {
        ConfigPlan {
            ollama_base_url: Some("http://localhost:11434".into()),
            openrouter_key: None,
            lead: ModelRef::new(Provider::Ollama, "qwen3-coder:30b"),
            reviewer: ModelRef::new(Provider::Ollama, "gpt-oss:20b"),
            run_writer: Some(ModelRef::new(Provider::Ollama, "qwen3-coder:30b")),
            run_reviewer: Some(ModelRef::new(Provider::Ollama, "gpt-oss:20b")),
            port: 8080,
            note: "local Ollama setup".into(),
        }
    }

    fn openrouter_plan() -> ConfigPlan {
        ConfigPlan {
            ollama_base_url: None,
            openrouter_key: Some(KeyRef::Env("OPENROUTER_API_KEY".into())),
            lead: ModelRef::new(Provider::OpenRouter, "qwen/qwen3-coder"),
            reviewer: ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b"),
            run_writer: Some(ModelRef::new(Provider::OpenRouter, "qwen/qwen3-coder")),
            run_reviewer: Some(ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b")),
            port: 8081,
            note: "OpenRouter setup".into(),
        }
    }

    fn agent<'a>(
        config: &'a axocoatl_config::AxocoatlConfig,
        id: &str,
    ) -> &'a axocoatl_config::AgentConfigYaml {
        config.agents.iter().find(|agent| agent.id == id).unwrap()
    }

    #[test]
    fn model_refs_name_a_native_provider() {
        assert_eq!(
            ModelRef::parse("ollama:gpt-oss:20b").unwrap(),
            ModelRef::new(Provider::Ollama, "gpt-oss:20b")
        );
        assert_eq!(
            ModelRef::parse("openrouter:openai/gpt-oss-120b")
                .unwrap()
                .spec(),
            "openrouter:openai/gpt-oss-120b"
        );
        let refused = ModelRef::parse("anthropic:opus").unwrap_err();
        assert!(refused.contains("cannot run native Agents"), "{refused}");
        assert!(ModelRef::parse("gpt-oss").is_err());
        assert!(ModelRef::parse("ollama:").is_err());
    }

    #[test]
    fn a_local_setup_writes_the_team_the_server_and_the_run_defaults() {
        let text = render(&local_plan());
        let config = axocoatl_config::parse_config(&text, Path::new("config.yaml")).unwrap();
        assert_eq!(agent(&config, "lead").model, "qwen3-coder:30b");
        assert_eq!(agent(&config, "scout").model, "qwen3-coder:30b");
        assert_eq!(agent(&config, "reviewer").model, "gpt-oss:20b");
        assert_eq!(agent(&config, "assistant").model, "qwen3-coder:30b");
        assert!(config
            .agents
            .iter()
            .filter(|agent| agent.id != "assistant")
            .all(|agent| agent.sampling.max_tokens == Some(4096)));
        assert_eq!(
            config.providers.ollama.unwrap().base_url,
            "http://localhost:11434"
        );
        assert!(config.providers.openrouter.is_none());
        assert_eq!(config.server.port, 8080);
        assert_eq!(
            run_models(&text),
            RunModels {
                writer: Some("ollama:qwen3-coder:30b".into()),
                reviewer: Some("ollama:gpt-oss:20b".into()),
            }
        );
        assert!(text.contains("run:\n  models:\n    writer: \"ollama:qwen3-coder:30b\"\n"));
    }

    /// OpenRouter: a different reviewer model, the writer's output limit
    /// 4096, the key by reference, credits billing.
    #[test]
    fn an_openrouter_setup_has_a_separate_reviewer_and_a_larger_writer_limit() {
        let text = render(&openrouter_plan());
        let config = axocoatl_config::parse_config(&text, Path::new("config.yaml")).unwrap();
        assert_eq!(agent(&config, "lead").model, "qwen/qwen3-coder");
        assert_eq!(agent(&config, "reviewer").model, "openai/gpt-oss-120b");
        assert_ne!(
            agent(&config, "lead").model,
            agent(&config, "reviewer").model
        );
        assert_eq!(agent(&config, "lead").sampling.max_tokens, Some(4096));
        assert_eq!(agent(&config, "scout").sampling.max_tokens, Some(4096));
        assert_eq!(agent(&config, "reviewer").sampling.max_tokens, Some(2048));
        assert_eq!(agent(&config, "assistant").sampling.max_tokens, Some(2048));
        assert_eq!(
            config.providers.openrouter_billing,
            Some(axocoatl_config::OpenRouterBilling::Credits)
        );
        assert!(text.contains("api_key: \"${OPENROUTER_API_KEY}\""));
        assert_eq!(config.server.port, 8081);
    }

    /// Claude Code with a local reviewer: Ollama for the reviewer, no
    /// writer default (fix-claude-code names its own), the workbench team
    /// on the reviewer's model.
    #[test]
    fn an_external_writer_setup_writes_only_the_reviewer_default() {
        let plan = ConfigPlan {
            run_writer: None,
            lead: ModelRef::new(Provider::Ollama, "gpt-oss:20b"),
            note: "Claude Code writes in `axocoatl run fix-claude-code`".into(),
            ..local_plan()
        };
        let text = render(&plan);
        axocoatl_config::parse_config(&text, Path::new("config.yaml")).unwrap();
        assert_eq!(
            run_models(&text),
            RunModels {
                writer: None,
                reviewer: Some("ollama:gpt-oss:20b".into()),
            }
        );
    }

    /// A local writer with an OpenRouter reviewer: both providers, each
    /// Agent on its own.
    #[test]
    fn mixed_providers_put_each_agent_on_its_own() {
        let plan = ConfigPlan {
            openrouter_key: Some(KeyRef::Value(zeroize::Zeroizing::new(
                "sk-or-test: \"quoted\"".to_string(),
            ))),
            reviewer: ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b"),
            run_reviewer: Some(ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b")),
            ..local_plan()
        };
        let text = render(&plan);
        let config = axocoatl_config::parse_config(&text, Path::new("config.yaml")).unwrap();
        assert_eq!(agent(&config, "lead").provider, "ollama");
        assert_eq!(agent(&config, "reviewer").provider, "openrouter");
        assert_eq!(
            config.providers.openrouter.unwrap().api_key.expose_secret(),
            "sk-or-test: \"quoted\""
        );
        assert!(config.providers.ollama.is_some());
        assert!(!format!("{:?}", plan.openrouter_key).contains("sk-or"));
    }

    /// Update keeps everything the person wrote, comments included, and
    /// adds only what this run chose.
    #[test]
    fn update_keeps_the_file_and_changes_only_run_the_port_and_missing_providers() {
        let old = "# my notes\nagents:\n  - id: mine\n    name: \"Mine\"\n    provider: ollama\n    model: \"qwen3:8b\"\n\nproviders:\n  ollama:\n    base_url: \"http://localhost:11434\"\n\nserver:\n  port: 8080\n  host: \"127.0.0.1\"\n";
        let mut plan = openrouter_plan();
        plan.ollama_base_url = Some("http://localhost:11434".into());
        let new = update(old, &plan, Path::new("config.yaml")).unwrap();
        assert!(
            new.starts_with("# my notes\nagents:\n  - id: mine\n"),
            "{new}"
        );
        let config = axocoatl_config::parse_config(&new, Path::new("config.yaml")).unwrap();
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.server.port, 8081);
        assert!(config.providers.openrouter.is_some());
        assert_eq!(
            config.providers.ollama.unwrap().base_url,
            "http://localhost:11434"
        );
        assert_eq!(
            run_models(&new).reviewer.as_deref(),
            Some("openrouter:openai/gpt-oss-120b")
        );
        let changes = diff(old, &new);
        assert!(
            changes.contains(&"-   port: 8080".to_string()),
            "{changes:?}"
        );
        assert!(
            changes.contains(&"+   port: 8081".to_string()),
            "{changes:?}"
        );
        assert!(changes.iter().all(|line| !line.contains("id: mine")));

        // Updating again replaces run instead of adding a second one.
        let again = update(&new, &plan, Path::new("config.yaml")).unwrap();
        assert_eq!(again.matches("\nrun:\n").count(), 1, "{again}");
        assert_eq!(again, new);
    }

    /// A configuration onboarding wrote is unchanged by the same setup, and
    /// a changed reviewer replaces `run` where it is.
    #[test]
    fn updating_a_written_configuration_changes_nothing_or_run_in_place() {
        let plan = local_plan();
        let written = render(&plan);
        assert_eq!(
            update(&written, &plan, Path::new("c.yaml")).unwrap(),
            written
        );
        let mut other = plan.clone();
        other.run_reviewer = Some(ModelRef::new(Provider::Ollama, "gpt-oss:120b"));
        let updated = update(&written, &other, Path::new("c.yaml")).unwrap();
        assert_eq!(
            diff(&written, &updated),
            [
                "-     reviewer: \"ollama:gpt-oss:20b\"",
                "+     reviewer: \"ollama:gpt-oss:120b\""
            ]
        );
    }

    #[test]
    fn update_points_a_changed_ollama_server_and_fills_an_empty_file() {
        let old = "providers:\n  ollama:\n    base_url: \"http://localhost:11434\"\n";
        let mut plan = local_plan();
        plan.ollama_base_url = Some("http://127.0.0.1:11440".into());
        let new = update(old, &plan, Path::new("c.yaml")).unwrap();
        let config = axocoatl_config::parse_config(&new, Path::new("c.yaml")).unwrap();
        assert_eq!(
            config.providers.ollama.as_ref().unwrap().base_url,
            "http://127.0.0.1:11440"
        );
        assert_eq!(agent(&config, "reviewer").model, "gpt-oss:20b");
        assert!(update("agents: [", &plan, Path::new("c.yaml"))
            .unwrap_err()
            .contains("choose Replace"));
    }

    /// An empty `agents: []` gets the default team in its place.
    #[test]
    fn update_fills_an_empty_agents_list() {
        let new = update("agents: []\n", &local_plan(), Path::new("c.yaml")).unwrap();
        let config = axocoatl_config::parse_config(&new, Path::new("c.yaml")).unwrap();
        assert_eq!(config.agents.len(), 4);
        assert_eq!(new.matches("agents:").count(), 1, "{new}");
    }

    #[test]
    fn the_diff_lists_only_changed_lines() {
        assert_eq!(diff("a\nb\nc\n", "a\nB\nc\nd\n"), ["- b", "+ B", "+ d"]);
        assert!(diff("same\n", "same\n").is_empty());
    }
}
