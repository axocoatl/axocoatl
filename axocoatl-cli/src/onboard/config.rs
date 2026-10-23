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
/// environment of whatever starts Axocoatl; a secret (the browser
/// sign-in's) is named in `api_key_secret` and read from Axocoatl's secret
/// store, so the key is never in the file.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum KeyRef {
    Value(zeroize::Zeroizing<String>),
    Env(String),
    Secret(String),
}

impl std::fmt::Debug for KeyRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyRef::Value(_) => formatter.write_str("KeyRef::Value(<hidden>)"),
            KeyRef::Env(name) => write!(formatter, "KeyRef::Env({name})"),
            KeyRef::Secret(name) => write!(formatter, "KeyRef::Secret({name})"),
        }
    }
}

impl KeyRef {
    /// What `api_key` holds: the key, or `${NAME}`; `None` for a secret,
    /// which `api_key_secret` names instead.
    fn raw(&self) -> Option<zeroize::Zeroizing<String>> {
        match self {
            KeyRef::Value(key) => Some(key.clone()),
            KeyRef::Env(name) => Some(zeroize::Zeroizing::new(format!("${{{name}}}"))),
            KeyRef::Secret(_) => None,
        }
    }

    /// The entry under `providers.openrouter`: `api_key: …` or
    /// `api_key_secret: NAME`.
    fn entry(&self) -> String {
        match self {
            KeyRef::Secret(name) => format!("api_key_secret: {name}"),
            KeyRef::Value(_) | KeyRef::Env(_) => {
                format!("api_key: {}", yaml_string(&self.raw().unwrap_or_default()))
            }
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
    /// Who writes: `claude-code`, `codex`, `ollama` or `openrouter`,
    /// recorded as a comment above `run:` ([`writer_marker`]) so `doctor`
    /// and a later `onboard --update` know the path chosen, including for
    /// Claude Code and Codex, which have no `run.models.writer`.
    pub(crate) writer: &'static str,
    /// The credential a Codex run uses (`codex-chatgpt` or `codex-openai`).
    pub(crate) codex_credential: Option<&'static str>,
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

/// The `providers:` lines: the key, or the secret that holds it (the lines
/// `axocoatl connect openrouter` writes).
fn providers_block(plan: &ConfigPlan) -> String {
    let mut text = String::from("providers:\n");
    let secret = match &plan.openrouter_key {
        Some(KeyRef::Secret(name)) => Some(name),
        _ => None,
    };
    if plan.openrouter_key.is_some() && secret.is_none() {
        text.push_str("  openrouter_billing: credits\n");
    }
    if let Some(base_url) = &plan.ollama_base_url {
        text.push_str(&format!(
            "  ollama:\n    base_url: {}\n",
            yaml_string(base_url)
        ));
    }
    match (&plan.openrouter_key, secret) {
        (_, Some(name)) => text.push_str(&crate::connect_cmd::openrouter::provider_yaml(name)),
        (Some(key), None) => text.push_str(&format!("  openrouter:\n    {}\n", key.entry())),
        (None, None) => {}
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
    let loadout = match plan.writer {
        "claude-code" => "fix-claude-code",
        "codex" => "fix-codex",
        _ => "fix",
    };
    let credential = plan
        .codex_credential
        .map(|credential| format!(" --param credential={credential}"))
        .unwrap_or_default();
    let mut text = format!(
        "# Defaults for `axocoatl run`: writer_model and reviewer_model.\n\
         # `--model writer=PROVIDER:MODEL` or `--model reviewer=…` overrides them.\n\
         {WRITER_MARKER}{} (axocoatl run {loadout}{credential})\n\
         run:\n  models:\n",
        plan.writer
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

/// The comment line that records who writes ([`ConfigPlan::writer`]).
pub(crate) const WRITER_MARKER: &str = "# Writer: ";

/// Who writes, as onboarding recorded it: the writer path and, for Codex,
/// the credential its runs use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriterMarker {
    pub(crate) writer: String,
    pub(crate) credential: Option<String>,
}

/// The `# Writer: …` comment of a configuration onboarding wrote.
pub(crate) fn writer_marker(text: &str) -> Option<WriterMarker> {
    let rest = text
        .lines()
        .find_map(|line| line.strip_prefix(WRITER_MARKER))?;
    let writer = rest.split_whitespace().next()?.to_string();
    if !matches!(
        writer.as_str(),
        "claude-code" | "codex" | "ollama" | "openrouter"
    ) {
        return None;
    }
    let credential = rest.split_once("credential=").and_then(|(_, value)| {
        let value: String = value
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        (!value.is_empty()).then_some(value)
    });
    Some(WriterMarker { writer, credential })
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
    let openrouter_note = match &plan.openrouter_key {
        Some(KeyRef::Secret(name)) => format!(
            "# OpenRouter's key is in Axocoatl's secret store (secret {name}), never in this \
             file.\n# OpenRouter credits pay; BYOK is not supported for native Session \
             execution.\n"
        ),
        Some(_) => "# OpenRouter uses your normal API key and OpenRouter credits.\n\
                    # BYOK is not supported for native Session execution.\n"
            .to_string(),
        None => String::new(),
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
    /// The model an Agent without one runs.
    #[serde(default)]
    pub(crate) model: Option<String>,
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

/// The ids of the Agents onboarding writes: the default team and the chat
/// Assistant.
const TEAM_IDS: [&str; 4] = ["lead", "scout", "reviewer", "assistant"];

/// The prompts onboarding wrote in earlier releases (1.4.0 and the 1.5.0
/// candidates), which a configuration written then still holds: such an
/// Agent is onboarding's, not one edited by hand.
const EARLIER_PROMPTS: [(&str, &str); 3] = [
    (
        "lead",
        "You own this change. 1. Look first: find the relevant files and tests for code you do \
         not know. If you have helpers, ask scout to find them. 2. Make the change. 3. Run the \
         check command from the project instructions (AXOCOATL.md), or the tests, and fix \
         failures. 4. Check your diff against the task. If you have helpers, ask reviewer to \
         review it. Fix each defect found. Then summarize what you changed.",
    ),
    (
        "scout",
        "Answer the lead's question with file paths, line numbers and short evidence. Change \
         nothing.",
    ),
    (
        "reviewer",
        "Review the described change. Check it against the task and against every contract and \
         edge case the docs, comments and tests describe, one by one. Report each defect with \
         file:line, or say you found none. Change nothing.",
    ),
];

/// What an Agent's provider, model and output limit are: onboarding sets
/// them from the models chosen, and moves them with the models.
const MODEL_FIELDS: [&str; 3] = ["provider", "model", "sampling"];

/// The Agent `id` as onboarding writes it now, as read.
fn template_agent(id: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let model = ModelRef::new(Provider::Ollama, "model");
    let plan = ConfigPlan {
        ollama_base_url: None,
        openrouter_key: None,
        lead: model.clone(),
        reviewer: model,
        run_writer: None,
        run_reviewer: None,
        port: 8080,
        note: String::new(),
        writer: "ollama",
        codex_credential: None,
    };
    read_existing(&agents_block(&plan))
        .ok()?
        .agents
        .into_iter()
        .find(|agent| agent.get("id").and_then(serde_json::Value::as_str) == Some(id))?
        .as_object()
        .cloned()
}

/// An Agent's `provider` and `model`.
fn agent_model(agent: &serde_json::Value) -> Option<(String, String)> {
    let field = |name: &str| agent.get(name).and_then(serde_json::Value::as_str);
    Some((field("provider")?.to_string(), field("model")?.to_string()))
}

/// The provider and model onboarding writes for the Agent `id` of a
/// configuration whose run defaults are `run` and whose writer is `writer`
/// (its `# Writer:` comment): the reviewer's for Reviewer; for Lead, Scout
/// and the Assistant the writer's, or the reviewer's when Claude Code or
/// Codex writes. `None` when the configuration does not say.
fn expected_model(id: &str, run: &RunModels, writer: Option<&str>) -> Option<(String, String)> {
    let spec = match id {
        "reviewer" => run.reviewer.as_deref(),
        _ if matches!(writer, Some("claude-code" | "codex")) => run.reviewer.as_deref(),
        _ => run.writer.as_deref(),
    }?;
    let model = ModelRef::parse(spec).ok()?;
    Some((model.provider.id().to_string(), model.model))
}

/// What onboarding wrote in an existing configuration: its run defaults,
/// its writer, and its Lead when that is onboarding's.
struct Written<'a> {
    run: RunModels,
    writer: Option<String>,
    agents: &'a [serde_json::Value],
}

impl<'a> Written<'a> {
    fn of(existing: &'a Existing, text: &str) -> Self {
        Self {
            run: existing
                .run
                .as_ref()
                .map(|run| run.models.clone())
                .unwrap_or_default(),
            writer: writer_marker(text).map(|marker| marker.writer),
            agents: &existing.agents,
        }
    }

    /// Whether `agent` is one onboarding wrote and nobody edited by hand:
    /// one of its ids, with only the fields onboarding writes, each as it
    /// writes it (its prompt as this release or an earlier one wrote it),
    /// on an `ollama` or `openrouter` model with at most
    /// `sampling.max_tokens`, and on the model onboarding writes for it
    /// ([`expected_model`]; for a configuration without run defaults, Scout
    /// and the Assistant on the Lead's). Its provider, model and output limit
    /// are what moves with the models chosen.
    fn by_onboarding(&self, agent: &serde_json::Value) -> bool {
        let Some(id) = agent.get("id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        if !as_written(agent) {
            return false;
        }
        let model = agent_model(agent);
        match expected_model(id, &self.run, self.writer.as_deref()) {
            Some(expected) => model.as_ref() == Some(&expected),
            None if matches!(id, "scout" | "assistant") => self
                .agents
                .iter()
                .find(|lead| lead.get("id").and_then(serde_json::Value::as_str) == Some("lead"))
                .filter(|lead| as_written(lead))
                .is_none_or(|lead| agent_model(lead) == model),
            None => true,
        }
    }

    /// The Agents "Update this setup" keeps as they are: the person's own,
    /// and onboarding's that were edited by hand.
    fn kept(&self) -> Vec<&'a serde_json::Value> {
        self.agents
            .iter()
            .filter(|agent| !self.by_onboarding(agent))
            .collect()
    }
}

/// Whether `agent` has the shape onboarding writes for its id: the fields
/// it writes, each as it writes it, but the provider, the model and
/// `sampling` (with `max_tokens` alone), which follow the models chosen.
fn as_written(agent: &serde_json::Value) -> bool {
    let Some(object) = agent.as_object() else {
        return false;
    };
    let Some(id) = object.get("id").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if !TEAM_IDS.contains(&id) {
        return false;
    }
    let Some(template) = template_agent(id) else {
        return false;
    };
    let fields = |map: &serde_json::Map<String, serde_json::Value>| {
        map.keys()
            .filter(|key| !MODEL_FIELDS.contains(&key.as_str()))
            .cloned()
            .collect::<std::collections::BTreeSet<String>>()
    };
    if fields(object) != fields(&template) {
        return false;
    }
    let same = template.iter().all(|(key, value)| {
        MODEL_FIELDS.contains(&key.as_str())
            || object.get(key) == Some(value)
            || (key == "system_prompt"
                && EARLIER_PROMPTS.iter().any(|(earlier, prompt)| {
                    *earlier == id
                        && object.get(key).and_then(serde_json::Value::as_str) == Some(prompt)
                }))
    });
    let sampling = match object.get("sampling") {
        None => true,
        Some(sampling) => sampling
            .as_object()
            .is_some_and(|sampling| sampling.keys().all(|key| key == "max_tokens")),
    };
    let provider = matches!(
        object.get("provider").and_then(serde_json::Value::as_str),
        Some("ollama" | "openrouter")
    );
    same && sampling && provider && object.get("model").is_some()
}

/// The Ollama models run by the Agents "Update this setup" keeps as they
/// are, the person's own and onboarding's edited by hand, as doctor reads
/// them (an Agent without a model runs `providers.ollama.model`, else
/// llama3.2); `None` without any Agents (onboarding writes its team).
pub(crate) fn kept_agent_models(existing: &Existing, text: &str) -> Option<Vec<String>> {
    if existing.agents.is_empty() {
        return None;
    }
    let default_model = existing
        .providers
        .as_ref()
        .and_then(|providers| providers.ollama.as_ref())
        .and_then(|ollama| ollama.model.clone())
        .unwrap_or_else(|| "llama3.2".to_string());
    Some(
        Written::of(existing, text)
            .kept()
            .into_iter()
            .filter(|agent| {
                agent.get("provider").and_then(serde_json::Value::as_str) == Some("ollama")
            })
            .map(|agent| {
                agent
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| default_model.clone())
            })
            .collect(),
    )
}

/// The lines of the Agent `id` in the `agents:` block `range` of `lines`:
/// from its `  - id: ID` line to the line before the next item, comment or
/// top-level key, without the blank lines at its end.
fn agent_lines(lines: &[String], range: (usize, usize), id: &str) -> Option<(usize, usize)> {
    let start = (range.0 + 1..range.1).find(|&index| {
        lines[index]
            .strip_prefix("  - id:")
            .is_some_and(|rest| rest.trim().trim_matches('"').trim_matches('\'') == id)
    })?;
    let mut end = start + 1;
    while end < range.1
        && !lines[end].starts_with("  - ")
        && !lines[end].starts_with("  #")
        && (lines[end].is_empty() || lines[end].starts_with(' '))
    {
        end += 1;
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some((start, end))
}

/// "Update this setup": the new text, and the Agents of onboarding's team it
/// kept as they are because they were edited by hand, though the models
/// chosen now differ from theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Updated {
    pub(crate) text: String,
    /// `(id, provider:model)` of each such Agent.
    pub(crate) kept: Vec<(String, String)>,
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
) -> Result<Updated, String> {
    let existing = read_existing(old).map_err(|error| {
        format!("the existing configuration cannot be read ({error}); choose Replace instead")
    })?;
    let mut lines = to_lines(old);
    let mut kept = Vec::new();

    // Agents: the team when there are none (no `agents:`, or an empty one
    // such as `agents: []`, which the team replaces); else the Agents
    // onboarding wrote move to the models chosen now, each where it is,
    // and every other Agent stays as it is.
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
    } else {
        let team = to_lines(&agents_block(plan));
        let written = Written::of(&existing, old);
        for agent in &existing.agents {
            let Some(id) = agent
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| TEAM_IDS.contains(id))
            else {
                continue;
            };
            let chosen = if id == "reviewer" {
                &plan.reviewer
            } else {
                &plan.lead
            };
            let moves = written.by_onboarding(agent);
            let block = top_block(&lines, "agents")
                .and_then(|range| agent_lines(&lines, range, id))
                .filter(|_| moves);
            let new = agent_lines(&team, (0, team.len()), id);
            match (block, new) {
                (Some((start, end)), Some((from, to))) => {
                    lines.splice(start..end, team[from..to].iter().cloned());
                }
                _ => {
                    let current = agent_model(agent)
                        .map(|(provider, model)| format!("{provider}:{model}"))
                        .unwrap_or_default();
                    if current != chosen.spec() {
                        kept.push((id.to_string(), current));
                    }
                }
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
    match (&plan.openrouter_key, has_openrouter) {
        // The browser sign-in's secret: named as `axocoatl connect
        // openrouter` names it, once the rest is updated (below).
        (Some(KeyRef::Secret(_)), _) => {}
        (Some(key), false) => missing_providers.push(format!(
            "  openrouter_billing: credits\n  openrouter:\n    {}",
            key.entry()
        )),
        (Some(key), true) => {
            // This run chose a key (another variable, or a value typed at
            // the hidden prompt): it replaces the configured one, or the
            // secret named in its place, where it is.
            let openrouter = existing
                .providers
                .as_ref()
                .and_then(|providers| providers.openrouter.as_ref());
            let configured = openrouter
                .and_then(|openrouter| openrouter.get("api_key"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let names_secret =
                openrouter.is_some_and(|openrouter| openrouter.get("api_key_secret").is_some());
            let raw = key.raw().unwrap_or_default();
            if configured.as_deref() != Some(raw.as_str()) || names_secret {
                let line = format!("    {}", key.entry());
                if let Some(providers) = top_block(&lines, "providers") {
                    if let Some(openrouter) = child_block(&lines, providers, "openrouter") {
                        // A configuration names a key or a secret, never
                        // both.
                        let entries: Vec<usize> = (openrouter.0 + 1..openrouter.1)
                            .filter(|&index| {
                                let entry = lines[index].trim_start();
                                entry.starts_with("api_key:")
                                    || entry.starts_with("api_key_secret:")
                            })
                            .collect();
                        match entries.split_first() {
                            Some((&first, rest)) => {
                                lines[first] = line;
                                for &index in rest.iter().rev() {
                                    lines.remove(index);
                                }
                            }
                            None => lines.insert(openrouter.0 + 1, line),
                        }
                    }
                }
            }
        }
        (None, _) => {}
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

    let mut text = from_lines(&lines);
    // The browser sign-in's secret, named as `axocoatl connect openrouter`
    // names it: `openrouter_billing: credits` and `api_key_secret`, any key
    // written in the file removed, every other line kept.
    if let Some(KeyRef::Secret(name)) = &plan.openrouter_key {
        text = crate::connect_cmd::openrouter::plan_connect(&text, name)
            .map_err(|reason| {
                format!("providers.openrouter cannot be pointed at the secret {name}: {reason}")
            })?
            .text;
    }
    axocoatl_config::parse_config(&text, path)
        .map_err(|error| format!("the updated configuration would not load: {error}"))?;
    Ok(Updated { text, kept })
}

/// A line diff of `old` and `new`: `-` removed, `+` added, unchanged lines
/// left out.
pub(crate) fn diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<String> = old.lines().map(str::to_string).collect();
    let b: Vec<String> = new.lines().map(str::to_string).collect();
    diff_shown(old, new, &a, &b)
}

/// [`diff`] as it is shown and logged: every secret value the configuration
/// can hold is hidden ([`redacted_lines`]), on both sides.
pub(crate) fn diff_redacted(old: &str, new: &str) -> Vec<String> {
    diff_shown(old, new, &redacted_lines(old), &redacted_lines(new))
}

/// The keys whose values are secrets (`SecretString` fields of the
/// configuration, and the usual names for one).
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "secret",
    "git_token",
    "token",
    "password",
    "client_secret",
    "authorization",
];
/// The keys whose every entry is a secret: server auth's `api_keys` and
/// `bearer_tokens`, and a webhook's `headers`.
const SECRET_BLOCKS: &[&str] = &["api_keys", "bearer_tokens", "headers"];

/// `text`'s lines with every secret value replaced by `(hidden)`; a
/// `${VARIABLE}` reference is not a secret and stays.
pub(crate) fn redacted_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    // The indent of a secret block being read.
    let mut secret_block: Option<usize> = None;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let content = line.trim_start();
        if let Some(block) = secret_block {
            if content.is_empty() || content.starts_with('#') || indent > block {
                lines.push(hide_entry(line));
                continue;
            }
            secret_block = None;
        }
        let item = content.strip_prefix("- ").unwrap_or(content);
        let Some((key, value)) = item.split_once(':') else {
            lines.push(line.to_string());
            continue;
        };
        let key_name = key
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_ascii_lowercase();
        let value = value.trim();
        let hidden = || {
            let prefix = &line[..line.len() - content.len()];
            let dash = if content.starts_with("- ") { "- " } else { "" };
            format!("{prefix}{dash}{}: (hidden)", key.trim())
        };
        if SECRET_BLOCKS.contains(&key_name.as_str()) {
            if value.is_empty() || value.starts_with('#') {
                secret_block = Some(indent);
                lines.push(line.to_string());
            } else {
                lines.push(hidden());
            }
        } else if is_secret_value(&key_name, value) {
            lines.push(hidden());
        } else {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Whether the value of `key_name` (lower case) is hidden: a secret key's,
/// unless it is a `${VARIABLE}` reference, and an `api_key_secret` that
/// is not a secret's name (a key written where the name belongs; a name is
/// shown).
fn is_secret_value(key_name: &str, value: &str) -> bool {
    fn unquoted(text: &str) -> &str {
        text.trim().trim_matches('"').trim_matches('\'')
    }
    if SECRET_KEYS.contains(&key_name) {
        return !value.is_empty() && !unquoted(value).starts_with("${");
    }
    if key_name == "api_key_secret" {
        let name = unquoted(value.split(" #").next().unwrap_or_default());
        return !name.is_empty() && axocoatl_config::shown_secret_name(name) != name;
    }
    false
}

/// One entry of a secret block, hidden: `- (hidden)` or `Name: (hidden)`.
fn hide_entry(line: &str) -> String {
    let content = line.trim_start();
    if content.is_empty() || content.starts_with('#') {
        return line.to_string();
    }
    let prefix = &line[..line.len() - content.len()];
    if content.starts_with('-') {
        return format!("{prefix}- (hidden)");
    }
    match content.split_once(':') {
        Some((key, _)) => format!("{prefix}{key}: (hidden)"),
        None => format!("{prefix}(hidden)"),
    }
}

/// The diff of `old` and `new`, each changed line shown as `shown_*` has it
/// at the same index.
fn diff_shown(old: &str, new: &str, shown_a: &[String], shown_b: &[String]) -> Vec<String> {
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
            lines.push(format!("- {}", shown_a[i]));
            i += 1;
        } else {
            lines.push(format!("+ {}", shown_b[j]));
            j += 1;
        }
    }
    lines.extend(shown_a[i..].iter().map(|line| format!("- {line}")));
    lines.extend(shown_b[j..].iter().map(|line| format!("+ {line}")));
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
            writer: "ollama",
            codex_credential: None,
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
            writer: "openrouter",
            codex_credential: None,
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
            writer: "claude-code",
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
        assert_eq!(
            writer_marker(&text),
            Some(WriterMarker {
                writer: "claude-code".into(),
                credential: None
            })
        );
        assert!(text.contains("# Writer: claude-code (axocoatl run fix-claude-code)\nrun:\n"));
        // Codex records its credential, which its runs name.
        let codex = render(&ConfigPlan {
            writer: "codex",
            codex_credential: Some("codex-chatgpt"),
            ..plan
        });
        assert_eq!(
            writer_marker(&codex),
            Some(WriterMarker {
                writer: "codex".into(),
                credential: Some("codex-chatgpt".into())
            })
        );
        assert_eq!(writer_marker("agents: []\n"), None);
    }

    /// Update with a key this run chose replaces the configured one in
    /// place: a `${VARIABLE}` by a value typed at the hidden prompt, and
    /// back; the same key changes nothing.
    #[test]
    fn update_replaces_the_configured_openrouter_key_with_the_one_chosen() {
        let plan = openrouter_plan();
        let written = render(&plan);
        assert!(written.contains("api_key: \"${OPENROUTER_API_KEY}\""));
        assert_eq!(
            update(&written, &plan, Path::new("c.yaml")).unwrap().text,
            written
        );
        let typed = ConfigPlan {
            openrouter_key: Some(KeyRef::Value(zeroize::Zeroizing::new(
                "sk-or-v1-typed".to_string(),
            ))),
            ..plan.clone()
        };
        let updated = update(&written, &typed, Path::new("c.yaml")).unwrap().text;
        let config = axocoatl_config::parse_config(&updated, Path::new("c.yaml")).unwrap();
        assert_eq!(
            config.providers.openrouter.unwrap().api_key.expose_secret(),
            "sk-or-v1-typed"
        );
        assert_eq!(updated.matches("api_key:").count(), 1, "{updated}");
        let shown = diff_redacted(&written, &updated);
        assert_eq!(
            shown,
            [
                "-     api_key: \"${OPENROUTER_API_KEY}\"",
                "+     api_key: (hidden)"
            ]
        );
        assert!(!shown.join("\n").contains("sk-or-v1-typed"));
        let back = update(&updated, &plan, Path::new("c.yaml")).unwrap().text;
        assert_eq!(back, written);
    }

    /// A key from the browser sign-in is a secret the configuration names
    /// (`api_key_secret`), as `axocoatl connect openrouter` writes it: a
    /// new file has `connect`'s lines, an update replaces a key written in
    /// the file (or a variable) by the name and back, and a name is shown
    /// while a key written where the name belongs is not.
    #[test]
    fn a_signed_in_key_is_a_secret_the_configuration_names() {
        let secret = ConfigPlan {
            openrouter_key: Some(KeyRef::Secret("openrouter".into())),
            ..openrouter_plan()
        };
        assert_eq!(
            format!("{:?}", secret.openrouter_key),
            "Some(KeyRef::Secret(openrouter))"
        );
        let written = render(&secret);
        assert!(
            written.contains(&format!(
                "providers:\n{}",
                crate::connect_cmd::openrouter::provider_yaml("openrouter")
            )),
            "{written}"
        );
        assert!(written.starts_with(
            "# Axocoatl — OpenRouter setup\n# OpenRouter's key is in Axocoatl's secret store \
             (secret openrouter), never in this file.\n"
        ));
        assert!(!written.contains("api_key:"), "{written}");
        let config = axocoatl_config::parse_config(&written, Path::new("c.yaml")).unwrap();
        let openrouter = config.providers.openrouter.unwrap();
        assert_eq!(openrouter.api_key_secret.as_deref(), Some("openrouter"));
        assert_eq!(
            config.providers.openrouter_billing,
            Some(axocoatl_config::OpenRouterBilling::Credits)
        );
        assert_eq!(
            update(&written, &secret, Path::new("c.yaml")).unwrap().text,
            written
        );
        // A variable, then a key typed at the hidden prompt, give way to the
        // secret, and the secret to them.
        let typed = ConfigPlan {
            openrouter_key: Some(KeyRef::Value(zeroize::Zeroizing::new(
                "sk-or-v1-typed".to_string(),
            ))),
            ..openrouter_plan()
        };
        for old in [render(&openrouter_plan()), render(&typed)] {
            let updated = update(&old, &secret, Path::new("c.yaml")).unwrap().text;
            assert!(!updated.contains("api_key:"), "{updated}");
            assert!(!updated.contains("sk-or-v1-typed"), "{updated}");
            assert_eq!(updated.matches("api_key_secret: openrouter").count(), 1);
            let config = axocoatl_config::parse_config(&updated, Path::new("c.yaml")).unwrap();
            assert_eq!(
                config
                    .providers
                    .openrouter
                    .unwrap()
                    .api_key_secret
                    .as_deref(),
                Some("openrouter")
            );
            let shown = diff_redacted(&old, &updated).join("\n");
            assert!(
                shown.contains("+     api_key_secret: openrouter"),
                "{shown}"
            );
            assert!(!shown.contains("sk-or-v1-typed"), "{shown}");
        }
        let back = update(&written, &typed, Path::new("c.yaml")).unwrap().text;
        assert!(!back.contains("api_key_secret"), "{back}");
        let config = axocoatl_config::parse_config(&back, Path::new("c.yaml")).unwrap();
        assert_eq!(
            config.providers.openrouter.unwrap().api_key.expose_secret(),
            "sk-or-v1-typed"
        );
        // A key written where the name belongs is hidden; a name is not.
        let shown = redacted_lines(
            "providers:\n  openrouter:\n    api_key_secret: sk-or-v1-0123456789abcdef0123\n  \
             openai:\n    api_key_secret: \"openai-key\"\n",
        )
        .join("\n");
        assert!(!shown.contains("sk-or-v1-0123456789abcdef0123"), "{shown}");
        assert!(shown.contains("    api_key_secret: (hidden)"), "{shown}");
        assert!(shown.contains("api_key_secret: \"openai-key\""), "{shown}");
    }

    /// Every secret the configuration can hold is hidden in what onboarding
    /// shows and logs: API keys, webhook secrets and headers, server auth
    /// keys and bearer tokens, the E2B git token.
    #[test]
    fn shown_configurations_hide_every_secret() {
        let text = "providers:\n  openrouter:\n    api_key: \"sk-or-v1-abc\"\n  anthropic:\n    api_key: \"${ANTHROPIC_API_KEY}\"\nwebhooks:\n  - name: ci\n    url: \"https://example.test/hook\"\n    secret: \"whsec-1\"\n    headers:\n      Authorization: \"Bearer tok-1\"\n      X-Other: \"v-1\"\nserver:\n  auth:\n    api_keys:\n      - \"key-1\"\n      - key-2\n    bearer_tokens: [\"tok-2\"]\n  port: 8080\nsandbox:\n  e2b:\n    git_token: \"ghp-1\"\n";
        let shown = redacted_lines(text).join("\n");
        for secret in [
            "sk-or-v1-abc",
            "whsec-1",
            "tok-1",
            "v-1",
            "key-1",
            "key-2",
            "tok-2",
            "ghp-1",
        ] {
            assert!(!shown.contains(secret), "{secret}:\n{shown}");
        }
        for kept in [
            "api_key: \"${ANTHROPIC_API_KEY}\"",
            "url: \"https://example.test/hook\"",
            "  port: 8080",
            "    secret: (hidden)",
            "      Authorization: (hidden)",
            "      - (hidden)",
            "    bearer_tokens: (hidden)",
        ] {
            assert!(shown.contains(kept), "{kept}:\n{shown}");
        }
        assert_eq!(redacted_lines(text).len(), text.lines().count());
        let removed = diff_redacted(text, "agents: []\n").join("\n");
        assert!(
            !removed.contains("whsec-1") && !removed.contains("key-1"),
            "{removed}"
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
        let new = update(old, &plan, Path::new("config.yaml")).unwrap().text;
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
        let again = update(&new, &plan, Path::new("config.yaml")).unwrap().text;
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
            update(&written, &plan, Path::new("c.yaml")).unwrap().text,
            written
        );
        let mut other = plan.clone();
        other.run_reviewer = Some(ModelRef::new(Provider::Ollama, "gpt-oss:120b"));
        let updated = update(&written, &other, Path::new("c.yaml")).unwrap().text;
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
        let new = update(old, &plan, Path::new("c.yaml")).unwrap().text;
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

    /// "Update this setup" moves the Agents onboarding wrote to the models
    /// chosen now, each where it is, and keeps any Agent edited by hand, or
    /// the person's own, as it is, saying which of onboarding's it kept.
    #[test]
    fn update_moves_the_team_onboarding_wrote_and_keeps_edited_agents() {
        let old = render(&local_plan());
        let mut new_plan = local_plan();
        new_plan.lead = ModelRef::new(Provider::Ollama, "devstral:24b");
        new_plan.run_writer = Some(new_plan.lead.clone());
        new_plan.reviewer = ModelRef::new(Provider::Ollama, "qwen3:32b");
        new_plan.run_reviewer = Some(new_plan.reviewer.clone());
        let updated = update(&old, &new_plan, Path::new("c.yaml")).unwrap();
        assert!(updated.kept.is_empty(), "{:?}", updated.kept);
        // As --replace writes it, every comment and line where it was.
        assert_eq!(updated.text, render(&new_plan));
        let changes = diff(&old, &updated.text);
        assert!(
            changes.contains(&"-     model: \"qwen3-coder:30b\"".to_string())
                && changes.contains(&"+     model: \"devstral:24b\"".to_string())
                && changes.contains(&"+     model: \"qwen3:32b\"".to_string()),
            "{changes:?}"
        );

        // Scout's prompt edited by hand, and an Agent of the person's own:
        // both stay as they are; Scout is said, the person's own is not.
        let edited = old
            .replace(
                "Answer the lead's question with file paths",
                "Answer with file paths",
            )
            .replace(
                "agents:\n",
                "agents:\n  - id: mine\n    name: \"Mine\"\n    provider: ollama\n    model: \
                 \"qwen3-coder:30b\"\n",
            );
        let updated = update(&edited, &new_plan, Path::new("c.yaml")).unwrap();
        assert_eq!(
            updated.kept,
            [("scout".to_string(), "ollama:qwen3-coder:30b".to_string())]
        );
        let config = axocoatl_config::parse_config(&updated.text, Path::new("c.yaml")).unwrap();
        assert_eq!(agent(&config, "lead").model, "devstral:24b");
        assert_eq!(agent(&config, "assistant").model, "devstral:24b");
        assert_eq!(agent(&config, "reviewer").model, "qwen3:32b");
        assert_eq!(agent(&config, "scout").model, "qwen3-coder:30b");
        assert!(agent(&config, "scout")
            .system_prompt
            .as_deref()
            .unwrap()
            .starts_with("Answer with file paths"));
        assert_eq!(agent(&config, "mine").model, "qwen3-coder:30b");

        // A Reviewer whose model was changed by hand (it no longer runs
        // run.models.reviewer) is the person's choice too.
        let moved = old.replacen("model: \"gpt-oss:20b\"", "model: \"qwen3:8b\"", 1);
        let updated = update(&moved, &new_plan, Path::new("c.yaml")).unwrap();
        assert_eq!(
            updated.kept,
            [("reviewer".to_string(), "ollama:qwen3:8b".to_string())]
        );

        // Moving to OpenRouter gives each Agent OpenRouter's output limit.
        let hosted = update(&old, &openrouter_plan(), Path::new("c.yaml")).unwrap();
        let config = axocoatl_config::parse_config(&hosted.text, Path::new("c.yaml")).unwrap();
        assert_eq!(agent(&config, "lead").provider, "openrouter");
        assert_eq!(agent(&config, "reviewer").sampling.max_tokens, Some(2048));
        assert_eq!(agent(&config, "assistant").sampling.max_tokens, Some(2048));
        assert_eq!(agent(&config, "scout").sampling.max_tokens, Some(4096));
    }

    /// A team written by 1.4.0 (the same Agents, with no `# Writer:` line
    /// or run defaults) is onboarding's, and moves; an Agent with a field
    /// onboarding never writes is the person's.
    #[test]
    fn a_team_written_by_1_4_moves_and_an_extra_field_is_an_edit() {
        let old = render(&local_plan());
        let start = old.find("# Defaults for `axocoatl run`").unwrap();
        let end = old.find("server:").unwrap();
        let earlier = format!("{}{}", &old[..start], &old[end..]);
        assert_eq!(writer_marker(&earlier), None);
        let mut plan = local_plan();
        plan.lead = ModelRef::new(Provider::Ollama, "devstral:24b");
        let updated = update(&earlier, &plan, Path::new("c.yaml")).unwrap();
        assert!(updated.kept.is_empty(), "{:?}", updated.kept);
        let config = axocoatl_config::parse_config(&updated.text, Path::new("c.yaml")).unwrap();
        assert_eq!(agent(&config, "lead").model, "devstral:24b");
        assert_eq!(agent(&config, "scout").model, "devstral:24b");
        let extra = earlier.replace(
            "    role: autonomous\n",
            "    role: autonomous\n    description: \"mine\"\n",
        );
        let updated = update(&extra, &plan, Path::new("c.yaml")).unwrap();
        assert_eq!(
            updated.kept,
            [("lead".to_string(), "ollama:qwen3-coder:30b".to_string())]
        );
    }

    /// An empty `agents: []` gets the default team in its place.
    #[test]
    fn update_fills_an_empty_agents_list() {
        let new = update("agents: []\n", &local_plan(), Path::new("c.yaml"))
            .unwrap()
            .text;
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
