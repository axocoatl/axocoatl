//! Loadouts: a versioned YAML file that declares a whole run — the Agents
//! (roles and models), the required checks (each with its own timeout), the
//! required reviewer, the egress allowlist and routes, the budgets and the
//! prompt. See `docs/design/1.3-loadouts.md`.
//!
//! This module parses and validates a loadout and resolves its parameters.
//! It never runs anything: the daemon turns a resolved loadout into a
//! Session, a Team and budget edit and a turn (`axocoatl_daemon::loadout`).
//! The built-in loadouts (`fix`, `qa`, `audit`) are compiled in from
//! `crates/axocoatl-config/loadouts/`; user loadouts are `*.yaml` files in
//! `<config dir>/loadouts/`.
//!
//! Owner: workstream `core` (parser, validation, resolution). The built-in
//! YAML files are owned by `review-qa` (`fix.yaml`, `qa.yaml`) and `audit`
//! (`audit.yaml`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{EgressAllowYaml, EgressRouteYaml};

/// The `schema` value every loadout file carries.
pub const LOADOUT_SCHEMA: &str = "axocoatl.loadout/1";
/// Largest loadout file accepted, in bytes.
pub const MAX_LOADOUT_BYTES: usize = 64 * 1024;
/// Most user loadouts read from the config directory.
pub const MAX_USER_LOADOUTS: usize = 128;
/// Most Agents a loadout declares (audit area workers are expanded at run
/// time and are not counted here).
pub const MAX_LOADOUT_AGENTS: usize = 8;
/// Most required checks a loadout declares.
pub const MAX_LOADOUT_CHECKS: usize = 16;
/// Default timeout of one required check: three minutes.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 180;
/// Longest timeout one required check may have: thirty minutes. Mirrors
/// `axocoatl_session::check_options::MAX_CHECK_TIMEOUT_MS`.
pub const MAX_CHECK_TIMEOUT_SECS: u64 = 30 * 60;
/// Review rounds a loadout may ask for (`turn_review::MAX_REVIEW_ROUNDS`).
pub const MAX_LOADOUT_REVIEW_ROUNDS: u32 = 3;
/// Bounds on the areas an audit plan may split its scope into.
pub const MIN_AUDIT_AREAS: u32 = 2;
pub const MAX_AUDIT_AREAS: u32 = 8;
/// Subdirectory of the config directory that holds user loadouts.
pub const USER_LOADOUT_DIR: &str = "loadouts";
/// Placeholder every prompt must contain.
pub const TASK_PLACEHOLDER: &str = "{task}";
/// Placeholders a prompt may use.
pub const PROMPT_PLACEHOLDERS: [&str; 4] = ["{task}", "{repo}", "{target_url}", "{reference_url}"];
/// The output bound per request of a loadout Agent or reviewer that names
/// none: a native Agent always runs with an explicit sampling maximum.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8192;
/// The smallest context a native model call runs with (the audited native
/// Ollama profile starts at 2048 tokens). A model's real context is known
/// only at run admission; a loadout whose tokens cannot hold even this
/// context plus the output bound can never make a call.
pub const MIN_CALL_CONTEXT_TOKENS: u64 = 2048;

const FIX_YAML: &str = include_str!("../loadouts/fix.yaml");
const QA_YAML: &str = include_str!("../loadouts/qa.yaml");
const AUDIT_YAML: &str = include_str!("../loadouts/audit.yaml");

/// What a loadout is for. The daemon picks the run driver from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadoutKind {
    /// One writer, required checks and a required review by another model.
    Fix,
    /// One browser explorer whose findings carry executable reproductions.
    Qa,
    /// Plan, parallel read-only area workers, integrate.
    Audit,
    /// Any other team shape the validator accepts.
    Custom,
}

impl fmt::Display for LoadoutKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LoadoutKind::Fix => "fix",
            LoadoutKind::Qa => "qa",
            LoadoutKind::Audit => "audit",
            LoadoutKind::Custom => "custom",
        })
    }
}

/// What an Agent does in its loadout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadoutRole {
    /// Owns the change; may write within `writes`.
    Writer,
    /// Explores the app in a browser and writes reproductions only.
    Explorer,
    /// Splits an audit's scope into areas (structured JSON). Read-only.
    Planner,
    /// The template of one audit area worker. Read-only.
    Worker,
    /// Merges the area workers' findings. Read-only.
    Integrator,
}

/// Where an Agent runs: Axocoatl's own tool loop, or an external coding-agent
/// program inside the Session container (workstream `agents`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRuntime {
    #[default]
    Native,
    /// Claude Code CLI, run headless inside the Session.
    ClaudeCode,
    /// Codex CLI, run headless inside the Session.
    Codex,
}

impl AgentRuntime {
    /// The runtime's id: `native`, `claude-code` or `codex`.
    pub fn id(self) -> &'static str {
        match self {
            AgentRuntime::Native => "native",
            AgentRuntime::ClaudeCode => "claude-code",
            AgentRuntime::Codex => "codex",
        }
    }

    /// The provider whose model API an external program calls, through the
    /// one route Axocoatl adds for it: `anthropic` for Claude Code
    /// (`api.anthropic.com`), `openai` for Codex (`api.openai.com`). An
    /// external writer's model names this provider and the program's own
    /// model name (`anthropic:claude-haiku-4-5`, `openai:gpt-5.5`), and the
    /// run records that identity. `None` for a native Agent, whose provider
    /// is its own.
    pub fn model_provider(self) -> Option<&'static str> {
        match self {
            AgentRuntime::Native => None,
            AgentRuntime::ClaudeCode => Some("anthropic"),
            AgentRuntime::Codex => Some("openai"),
        }
    }

    /// Why `model` cannot be this external program's model: it names
    /// another provider than the one whose API the program calls.
    pub fn model_refusal(self, model: &ModelSpec) -> Option<String> {
        let provider = self.model_provider()?;
        (model.provider != provider).then(|| {
            format!(
                "a {} writer calls {provider}'s model API, so its model is {provider}:<the \
                 program's model name>; {model} names the provider {}",
                self.id(),
                model.provider
            )
        })
    }
}

/// An exact provider and model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub provider: String,
    pub model: String,
}

impl ModelSpec {
    /// Parse `provider:model`, splitting at the first `:` (so
    /// `ollama:qwen3:32b` and `openrouter:anthropic/claude-sonnet-5.5` both
    /// work).
    pub fn parse(text: &str) -> Option<Self> {
        let (provider, model) = text.trim().split_once(':')?;
        let (provider, model) = (provider.trim(), model.trim());
        if provider.is_empty() || model.is_empty() || provider.contains('/') {
            return None;
        }
        Some(Self {
            provider: provider.to_string(),
            model: model.to_string(),
        })
    }

    /// Whether `self` and `other` are the same model, through a vendor's own
    /// API or OpenRouter ([`axocoatl_core::same_model`]).
    pub fn same_model(&self, other: &ModelSpec) -> bool {
        axocoatl_core::same_model(&self.provider, &self.model, &other.provider, &other.model)
    }
}

impl fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.provider, self.model)
    }
}

/// A literal value, or a named parameter supplied when the loadout runs
/// (`axocoatl run --param name=value`, `--model role=provider:model`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamOr<T> {
    Param { param: String },
    Value(T),
}

/// The kind of value a parameter takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// `provider:model`.
    Model,
    /// Free text, such as a shell command.
    Text,
    /// An `http://` or `https://` URL.
    Url,
}

/// One declared parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutParam {
    pub kind: ParamKind,
    #[serde(default)]
    pub description: String,
    /// A run without a value for a required parameter is a usage error.
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Grant limits in loadout units. `cost_usd` becomes micro-units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutLimits {
    pub activations: u32,
    pub invocations: u32,
    pub tokens: u64,
    pub cost_usd: f64,
}

impl LoadoutLimits {
    /// The cost limit in micro-units (millionths of a dollar), rounded down.
    pub fn cost_microunits(&self) -> u64 {
        if !self.cost_usd.is_finite() || self.cost_usd <= 0.0 {
            return 0;
        }
        (self.cost_usd * 1_000_000.0).floor() as u64
    }
}

/// `budgets:`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutBudgets {
    /// Limits of each Agent that does not set its own `budget`.
    pub agent: LoadoutLimits,
    /// Limits of the required reviewer over all rounds of one turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<LoadoutLimits>,
    /// Longest the whole run may take, such as `60m`. The run is stopped and
    /// reported as needing attention (budget) when it passes.
    pub wall_clock: String,
}

/// One Agent of a loadout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutAgent {
    /// Unique within the loadout: `[a-z][a-z0-9-]{0,31}`.
    pub id: String,
    pub role: LoadoutRole,
    pub model: ParamOr<ModelSpec>,
    #[serde(default)]
    pub runtime: AgentRuntime,
    /// Tool names (native runtime). Required for native Agents.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Paths the Agent may change; `[]` is read-only. Absent: every path
    /// (only a `writer` may leave it absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<axocoatl_core::ReasoningEffort>,
    /// Overrides `budgets.agent` for this Agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<LoadoutLimits>,
    /// Other Agents of the loadout whose accepted answer this Agent needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// How a required check runs. Exactly one field is set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    /// An exact argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// A shell command, run as `sh -c <command>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ParamOr<String>>,
    /// The Session's detected check command (`Session::check_command`), or
    /// `--check` when given. A run with neither is a usage error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detected: bool,
    /// The tester-army/e2e check (workstream `e2e`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2e: Option<E2eCheck>,
}

/// `checks[].e2e`: run `e2e@0.18.0` inside the Session as a required check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct E2eCheck {
    /// Arguments after `e2e`, such as `["run"]`.
    #[serde(default)]
    pub args: Vec<String>,
    /// The route (host under `routes`) that carries e2e's model calls.
    pub model_route: String,
    /// Model e2e's agent uses. Must support tool calls and vision.
    pub model: ParamOr<String>,
}

/// One required check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutCheck {
    /// Unique within the loadout; the JUnit test case name.
    pub name: String,
    pub run: CheckRun,
    /// Such as `90s`, `3m`; default three minutes, at most thirty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// `review:` — the required reviewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutReview {
    pub model: ParamOr<ModelSpec>,
    /// 1 to 3 rounds.
    pub rounds: u32,
    /// Read-only tools only.
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    /// The writer answers every finding with accept or reject and a reason.
    #[serde(default = "default_true")]
    pub adjudicate: bool,
}

fn default_true() -> bool {
    true
}

/// `egress:` — what the Session container may reach, in addition to nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutEgress {
    #[serde(default)]
    pub allow: Vec<EgressAllowYaml>,
    #[serde(default)]
    pub private_destinations: Vec<String>,
}

/// `sandbox:` — only stricter-or-equal to the loadout default
/// (`network: egress`, `workload: hardened`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutSandbox {
    /// `egress` (default) or `none`. `bridge` is refused.
    #[serde(default = "default_loadout_network")]
    pub network: String,
    /// `hardened` only: Agents run as the non-root writer user and helpers
    /// as the non-root helper user, commands under `--harden`.
    #[serde(default = "default_loadout_workload")]
    pub workload: String,
}

impl Default for LoadoutSandbox {
    fn default() -> Self {
        Self {
            network: default_loadout_network(),
            workload: default_loadout_workload(),
        }
    }
}

fn default_loadout_network() -> String {
    "egress".to_string()
}
fn default_loadout_workload() -> String {
    "hardened".to_string()
}

/// `environment:` — the Session image and setup command a run approves.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutEnvironment {
    /// An image reference (curated or trusted), exclusive with `recipes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Image recipes built by `axocoatl recipe build` (workstream `agents`),
    /// such as `[e2e]` or `[claude-code]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipes: Vec<String>,
    /// The exact setup command a run of this loadout approves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<ParamOr<String>>,
}

/// `qa:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QaSettings {
    /// The build under test, as the browser container reaches it.
    pub target_url: ParamOr<String>,
    /// A clean reference build. Absent: findings are `reproduced`, never
    /// `confirmed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_url: Option<ParamOr<String>>,
    /// Repository directory the explorer writes reproductions to; it is the
    /// explorer's whole write scope.
    pub repro_dir: String,
    /// Whether a confirmed (or, without a reference, reproduced) finding
    /// makes the run need attention.
    #[serde(default = "default_true")]
    pub fail_on_findings: bool,
}

/// `audit:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSettings {
    pub min_areas: u32,
    pub max_areas: u32,
    /// Whether any finding makes the run need attention.
    #[serde(default)]
    pub fail_on_findings: bool,
}

/// A loadout file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadoutFile {
    pub schema: String,
    /// `[a-z][a-z0-9-]{0,63}`; a user loadout may not reuse a built-in id.
    pub id: String,
    /// The loadout's own version, from 1.
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub kind: LoadoutKind,
    /// Listed but run only when named explicitly (the audit loadout).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opt_in: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, LoadoutParam>,
    pub agents: Vec<LoadoutAgent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<LoadoutCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<LoadoutReview>,
    #[serde(default)]
    pub egress: LoadoutEgress,
    /// Same shape as `sandbox.egress.routes`. A `credential` names a
    /// `credentials` entry or a secret stored with `axocoatl secret set`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<EgressRouteYaml>,
    pub budgets: LoadoutBudgets,
    /// The turn's request. Must contain `{task}`.
    pub prompt: String,
    #[serde(default)]
    pub sandbox: LoadoutSandbox,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<LoadoutEnvironment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditSettings>,
}

/// Where a loadout came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadoutSource {
    Builtin,
    User { path: PathBuf },
}

/// A parsed and validated loadout with its digest.
#[derive(Debug, Clone, PartialEq)]
pub struct Loadout {
    pub file: LoadoutFile,
    pub source: LoadoutSource,
    /// SHA-256 of the exact file bytes, lowercase hex.
    pub digest: String,
    /// The exact file text, retained in the run record.
    pub text: String,
    pub warnings: Vec<LoadoutWarning>,
}

/// A problem worth showing that does not stop a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutWarning {
    /// Stable code, such as `same_model_reviewer`.
    pub code: String,
    pub field: String,
    pub message: String,
}

/// Why a loadout cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadoutError {
    #[error("loadout {source_name}: {reason}")]
    Parse { source_name: String, reason: String },
    #[error("loadout field {field}: {reason}")]
    Invalid { field: String, reason: String },
    #[error("loadout parameter {param}: {reason}")]
    Param { param: String, reason: String },
    #[error("no loadout named {0:?}")]
    NotFound(String),
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> LoadoutError {
    LoadoutError::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

/// Parse and validate one loadout file.
pub fn parse_loadout(text: &str, source: LoadoutSource) -> Result<Loadout, LoadoutError> {
    let source_name = match &source {
        LoadoutSource::Builtin => "built-in".to_string(),
        LoadoutSource::User { path } => path.display().to_string(),
    };
    if text.len() > MAX_LOADOUT_BYTES {
        return Err(LoadoutError::Parse {
            source_name,
            reason: format!("the file is larger than {MAX_LOADOUT_BYTES} bytes"),
        });
    }
    let file: LoadoutFile = serde_yaml::from_str(text).map_err(|error| LoadoutError::Parse {
        source_name: source_name.clone(),
        reason: error.to_string(),
    })?;
    let warnings = validate_loadout(&file)?;
    Ok(Loadout {
        digest: format!("{:x}", Sha256::digest(text.as_bytes())),
        text: text.to_string(),
        file,
        source,
        warnings,
    })
}

fn valid_id(id: &str, max: usize) -> bool {
    let mut bytes = id.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && id.len() <= max
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// Parse a duration such as `45s`, `3m` or `2h` into seconds.
pub fn parse_duration_secs(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    if number.is_empty() || number.len() > 6 {
        return None;
    }
    let number: u64 = number.parse().ok()?;
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    let secs = number.checked_mul(factor)?;
    (secs > 0).then_some(secs)
}

/// The timeout of `check` in seconds, defaults applied.
pub fn check_timeout_secs(check: &LoadoutCheck) -> Result<u64, LoadoutError> {
    let Some(text) = &check.timeout else {
        return Ok(DEFAULT_CHECK_TIMEOUT_SECS);
    };
    let field = format!("checks.{}.timeout", check.name);
    let secs = parse_duration_secs(text)
        .ok_or_else(|| invalid(&field, "write a duration such as 90s, 3m or 1h"))?;
    if secs > MAX_CHECK_TIMEOUT_SECS {
        return Err(invalid(
            field,
            format!(
                "a check may run at most {} minutes",
                MAX_CHECK_TIMEOUT_SECS / 60
            ),
        ));
    }
    Ok(secs)
}

const READ_ONLY_TOOLS: [&str; 5] = [
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "workspace_knowledge",
];
const CHANGING_TOOLS: [&str; 3] = ["write_file", "edit_file", "bash"];

fn check_param_ref<T>(
    file: &LoadoutFile,
    value: &ParamOr<T>,
    field: &str,
    kind: ParamKind,
) -> Result<(), LoadoutError> {
    if let ParamOr::Param { param } = value {
        match file.params.get(param) {
            None => {
                return Err(invalid(
                    field,
                    format!("names parameter {param:?}, which params does not declare"),
                ))
            }
            Some(declared) if declared.kind != kind => {
                return Err(invalid(
                    field,
                    format!("parameter {param:?} is not a {kind:?} parameter"),
                ))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn check_limits(field: &str, limits: &LoadoutLimits) -> Result<(), LoadoutError> {
    if limits.activations == 0 || limits.invocations == 0 || limits.tokens == 0 {
        return Err(invalid(
            field,
            "activations, invocations and tokens must each be at least 1",
        ));
    }
    if !limits.cost_usd.is_finite() || limits.cost_usd < 0.0 || limits.cost_usd > 10_000.0 {
        return Err(invalid(field, "cost_usd must be from 0 to 10000"));
    }
    Ok(())
}

/// One model caller of a loadout (an Agent or the reviewer), its tokens
/// budget and its output bound per call ([`call_budgets`],
/// [`external_call_budgets`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallBudget {
    /// The Agent's id; `None` for the reviewer.
    pub agent: Option<String>,
    /// `Agent writer` or `the reviewer`.
    pub who: String,
    /// The loadout field that holds its tokens, such as
    /// `budgets.agent.tokens`.
    pub tokens_field: String,
    pub tokens: u64,
    /// The field that sets its output bound, such as
    /// `agents.writer.max_output_tokens`. An external program's output per
    /// call is its model's own, which this field does not set.
    pub output_field: String,
    pub max_output_tokens: u64,
    pub model: ParamOr<ModelSpec>,
    /// Where the caller runs: Axocoatl's own tool loop, or an external
    /// program.
    pub runtime: AgentRuntime,
    /// The reasoning effort the caller sends (native OpenRouter), if set.
    pub reasoning_effort: Option<axocoatl_core::ReasoningEffort>,
}

/// What one model call of a caller needs at least, as admission learned it
/// from the model's provider: the Ollama model's loaded context, the native
/// OpenRouter call's smallest reservation from OpenRouter's catalog, or the
/// most one call of an external program's pinned model can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFloor {
    /// The fewest tokens one call needs.
    pub tokens: u64,
    /// Those tokens in words, such as `the model's 32768-token context plus
    /// 8192 output tokens (agents.writer.max_output_tokens)`.
    pub words: String,
    /// The field that lowers the floor besides raising the budget, if any.
    pub lower: Option<String>,
}

impl CallBudget {
    /// The floor of a native call whose model's `context` is known (the
    /// model's own when known, else [`MIN_CALL_CONTEXT_TOKENS`]): that
    /// context plus the output bound.
    pub fn context_floor(&self, context: Option<u64>) -> CallFloor {
        let words = match context {
            Some(context) => format!("the model's {context}-token context"),
            None => format!("a context of at least {MIN_CALL_CONTEXT_TOKENS} tokens"),
        };
        CallFloor {
            tokens: context
                .unwrap_or(MIN_CALL_CONTEXT_TOKENS)
                .saturating_add(self.max_output_tokens),
            words: format!(
                "{words} plus {} output tokens ({})",
                self.max_output_tokens, self.output_field
            ),
            lower: Some(self.output_field.clone()),
        }
    }

    /// The fewest tokens one call needs: the model's `context` (the
    /// model's own when known, else [`MIN_CALL_CONTEXT_TOKENS`]) plus the
    /// output bound.
    pub fn minimum(&self, context: Option<u64>) -> u64 {
        self.context_floor(context).tokens
    }

    /// Why one call cannot fit, as the error of [`Self::tokens_field`]:
    /// the budget and the minimum in words; `None` when it fits. `model`
    /// names the resolved model when known.
    pub fn refusal(&self, context: Option<u64>, model: Option<&str>) -> Option<LoadoutError> {
        self.refusal_with(&self.context_floor(context), model)
    }

    /// [`Self::refusal`] against a `floor` the model's provider gave.
    pub fn refusal_with(&self, floor: &CallFloor, model: Option<&str>) -> Option<LoadoutError> {
        let minimum = floor.tokens;
        if self.tokens >= minimum {
            return None;
        }
        let model = model.map(|model| format!(" ({model})")).unwrap_or_default();
        Some(invalid(
            self.tokens_field.clone(),
            format!(
                "{} tokens is less than one model call of {}{model} needs: {}, at least \
                 {minimum} tokens; raise it to at least {minimum}{}",
                self.tokens,
                self.who,
                floor.words,
                floor
                    .lower
                    .as_ref()
                    .map(|field| format!(" or lower {field}"))
                    .unwrap_or_default()
            ),
        ))
    }
}

fn agent_budget(file: &LoadoutFile, agent: &LoadoutAgent) -> CallBudget {
    let (tokens_field, limits) = match &agent.budget {
        Some(limits) => (format!("agents.{}.budget.tokens", agent.id), limits),
        None => ("budgets.agent.tokens".to_string(), &file.budgets.agent),
    };
    CallBudget {
        agent: Some(agent.id.clone()),
        who: format!("Agent {}", agent.id),
        tokens_field,
        tokens: limits.tokens,
        output_field: format!("agents.{}.max_output_tokens", agent.id),
        max_output_tokens: agent.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS) as u64,
        model: agent.model.clone(),
        runtime: agent.runtime,
        reasoning_effort: agent.reasoning_effort,
    }
}

/// Every native model caller of `file`: each Agent that runs in Axocoatl's
/// own tool loop, with its own `budget` or `budgets.agent`, and the
/// reviewer with `budgets.reviewer`. External programs (`claude-code`,
/// `codex`) are [`external_call_budgets`]: their spend is reserved up
/// front, not per call, and their model's context is known only once the
/// model is resolved.
pub fn call_budgets(file: &LoadoutFile) -> Vec<CallBudget> {
    let mut budgets: Vec<CallBudget> = file
        .agents
        .iter()
        .filter(|agent| agent.runtime == AgentRuntime::Native)
        .map(|agent| agent_budget(file, agent))
        .collect();
    if let (Some(review), Some(limits)) = (&file.review, &file.budgets.reviewer) {
        budgets.push(CallBudget {
            agent: None,
            who: "the reviewer".into(),
            tokens_field: "budgets.reviewer.tokens".into(),
            tokens: limits.tokens,
            output_field: "review.max_output_tokens".into(),
            max_output_tokens: review
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS) as u64,
            model: review.model.clone(),
            runtime: AgentRuntime::Native,
            reasoning_effort: None,
        });
    }
    budgets
}

/// Every external program of `file` (a `claude-code` or `codex` writer)
/// with its tokens budget. Admission checks it against the most one call
/// of the program's pinned model can use.
pub fn external_call_budgets(file: &LoadoutFile) -> Vec<CallBudget> {
    file.agents
        .iter()
        .filter(|agent| agent.runtime != AgentRuntime::Native)
        .map(|agent| agent_budget(file, agent))
        .collect()
}

/// Validate a parsed loadout. Returns warnings; errors name the field.
pub fn validate_loadout(file: &LoadoutFile) -> Result<Vec<LoadoutWarning>, LoadoutError> {
    let mut warnings = Vec::new();
    if file.schema != LOADOUT_SCHEMA {
        return Err(invalid(
            "schema",
            format!("must be {LOADOUT_SCHEMA:?}; this Axocoatl reads no other loadout schema"),
        ));
    }
    if !valid_id(&file.id, 64) {
        return Err(invalid(
            "id",
            "1-64 lowercase letters, digits or '-', starting with a letter",
        ));
    }
    if file.version == 0 {
        return Err(invalid("version", "starts at 1"));
    }
    if file.name.trim().is_empty() {
        return Err(invalid("name", "must not be empty"));
    }
    for (name, param) in &file.params {
        if !valid_id(&name.replace('_', "-"), 32) {
            return Err(invalid(
                format!("params.{name}"),
                "1-32 lowercase letters, digits, '-' or '_'",
            ));
        }
        if param.required && param.default.is_some() {
            return Err(invalid(
                format!("params.{name}"),
                "a required parameter has no default",
            ));
        }
        if let (ParamKind::Model, Some(default)) = (param.kind, &param.default) {
            if ModelSpec::parse(default).is_none() {
                return Err(invalid(
                    format!("params.{name}.default"),
                    "write provider:model",
                ));
            }
        }
    }
    if file.agents.is_empty() || file.agents.len() > MAX_LOADOUT_AGENTS {
        return Err(invalid(
            "agents",
            format!("declare 1 to {MAX_LOADOUT_AGENTS} Agents"),
        ));
    }
    let mut ids = BTreeSet::new();
    for agent in &file.agents {
        let field = format!("agents.{}", agent.id);
        if !valid_id(&agent.id, 32) {
            return Err(invalid(
                "agents[].id",
                "1-32 lowercase letters, digits or '-', starting with a letter",
            ));
        }
        if !ids.insert(agent.id.as_str()) {
            return Err(invalid(field, "two Agents share this id"));
        }
        check_param_ref(
            file,
            &agent.model,
            &format!("{field}.model"),
            ParamKind::Model,
        )?;
        if agent.runtime != AgentRuntime::Native && agent.role != LoadoutRole::Writer {
            return Err(invalid(
                format!("{field}.runtime"),
                "an external program (claude-code, codex) runs only as the writer",
            ));
        }
        if let ParamOr::Value(model) = &agent.model {
            if let Some(reason) = agent.runtime.model_refusal(model) {
                return Err(invalid(format!("{field}.model"), reason));
            }
        }
        if agent.runtime == AgentRuntime::Native && agent.tools.is_empty() {
            return Err(invalid(
                format!("{field}.tools"),
                "a native Agent lists its tools",
            ));
        }
        if agent.role != LoadoutRole::Writer && agent.writes.is_none() {
            return Err(invalid(
                format!("{field}.writes"),
                "only a writer may change every path; list the paths, or [] for read-only",
            ));
        }
        if matches!(
            agent.role,
            LoadoutRole::Planner | LoadoutRole::Worker | LoadoutRole::Integrator
        ) && agent.writes.as_ref().is_some_and(|paths| !paths.is_empty())
        {
            return Err(invalid(
                format!("{field}.writes"),
                "planners, workers and integrators are read-only: writes: []",
            ));
        }
        if let Some(limits) = &agent.budget {
            check_limits(&format!("{field}.budget"), limits)?;
        }
    }
    for agent in &file.agents {
        for parent in &agent.depends_on {
            if !ids.contains(parent.as_str()) || parent == &agent.id {
                return Err(invalid(
                    format!("agents.{}.depends_on", agent.id),
                    format!("{parent:?} is not another Agent of this loadout"),
                ));
            }
        }
    }
    if file.checks.len() > MAX_LOADOUT_CHECKS {
        return Err(invalid(
            "checks",
            format!("at most {MAX_LOADOUT_CHECKS} checks"),
        ));
    }
    let mut check_names = BTreeSet::new();
    for check in &file.checks {
        let field = format!("checks.{}", check.name);
        if !valid_id(&check.name, 32) {
            return Err(invalid(
                "checks[].name",
                "1-32 lowercase letters, digits or '-', starting with a letter",
            ));
        }
        if !check_names.insert(check.name.as_str()) {
            return Err(invalid(field, "two checks share this name"));
        }
        let set = [
            check.run.argv.is_some(),
            check.run.shell.is_some(),
            check.run.detected,
            check.run.e2e.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        if set != 1 {
            return Err(invalid(
                format!("{field}.run"),
                "set exactly one of argv, shell, detected or e2e",
            ));
        }
        if let Some(argv) = &check.run.argv {
            if argv.is_empty() || argv.iter().any(|arg| arg.contains('\0')) {
                return Err(invalid(
                    format!("{field}.run.argv"),
                    "a non-empty argv without NUL",
                ));
            }
        }
        if let Some(shell) = &check.run.shell {
            check_param_ref(file, shell, &format!("{field}.run.shell"), ParamKind::Text)?;
        }
        if let Some(e2e) = &check.run.e2e {
            check_param_ref(
                file,
                &e2e.model,
                &format!("{field}.run.e2e.model"),
                ParamKind::Text,
            )?;
            if !file
                .routes
                .iter()
                .any(|route| route.host == e2e.model_route)
            {
                return Err(invalid(
                    format!("{field}.run.e2e.model_route"),
                    "names no host under routes",
                ));
            }
        }
        check_timeout_secs(check)?;
    }
    if let Some(review) = &file.review {
        check_param_ref(file, &review.model, "review.model", ParamKind::Model)?;
        if !(1..=MAX_LOADOUT_REVIEW_ROUNDS).contains(&review.rounds) {
            return Err(invalid(
                "review.rounds",
                format!("1 to {MAX_LOADOUT_REVIEW_ROUNDS}"),
            ));
        }
        if let Some(tool) = review
            .tools
            .iter()
            .find(|tool| !READ_ONLY_TOOLS.contains(&tool.as_str()))
        {
            return Err(invalid(
                "review.tools",
                format!("the reviewer is read-only; {tool} is not a read-only tool"),
            ));
        }
        let writer_models: Vec<&ParamOr<ModelSpec>> = file
            .agents
            .iter()
            .filter(|agent| agent.role == LoadoutRole::Writer)
            .map(|agent| &agent.model)
            .collect();
        // The same parameter, or literal models that are the same model
        // through any provider (an external writer's `anthropic:` or
        // `openai:` model on OpenRouter, say).
        let same = |writer: &ParamOr<ModelSpec>| match (writer, &review.model) {
            (ParamOr::Value(writer), ParamOr::Value(reviewer)) => writer.same_model(reviewer),
            (writer, reviewer) => writer == reviewer,
        };
        if writer_models.iter().any(|model| same(model)) {
            warnings.push(same_model_warning("review.model"));
        }
    }
    check_limits("budgets.agent", &file.budgets.agent)?;
    if let Some(limits) = &file.budgets.reviewer {
        check_limits("budgets.reviewer", limits)?;
    }
    if file.review.is_some() && file.budgets.reviewer.is_none() {
        return Err(invalid(
            "budgets.reviewer",
            "a loadout with a review sets the reviewer's limits",
        ));
    }
    // A model's real context is known only at admission, which checks it;
    // a budget below the smallest context plus the output bound can never
    // make a call.
    for budget in call_budgets(file) {
        if let Some(error) = budget.refusal(None, None) {
            return Err(error);
        }
    }
    if parse_duration_secs(&file.budgets.wall_clock).is_none() {
        return Err(invalid(
            "budgets.wall_clock",
            "write a duration such as 30m or 2h",
        ));
    }
    if !file.prompt.contains(TASK_PLACEHOLDER) {
        return Err(invalid("prompt", "must contain {task}"));
    }
    let mut rest = file.prompt.as_str();
    while let Some(start) = rest.find('{') {
        let after = &rest[start..];
        let Some(end) = after.find('}') else { break };
        let placeholder = &after[..=end];
        if !PROMPT_PLACEHOLDERS.contains(&placeholder) && !placeholder.contains(char::is_whitespace)
        {
            return Err(invalid(
                "prompt",
                format!(
                    "{placeholder} is not one of {}",
                    PROMPT_PLACEHOLDERS.join(", ")
                ),
            ));
        }
        rest = &after[end + 1..];
    }
    match file.sandbox.network.as_str() {
        "egress" | "none" => {}
        other => {
            return Err(invalid(
                "sandbox.network",
                format!("{other:?}: a loadout runs under egress (the default) or none"),
            ))
        }
    }
    if file.sandbox.workload != "hardened" {
        return Err(invalid(
            "sandbox.workload",
            "a loadout runs its commands as the hardened non-root workload users",
        ));
    }
    if file.sandbox.network == "none" && (!file.routes.is_empty() || !file.egress.allow.is_empty())
    {
        return Err(invalid(
            "sandbox.network",
            "network: none reaches no host; remove egress.allow and routes",
        ));
    }
    if file.sandbox.network == "none" {
        for agent in &file.agents {
            if let Some(tool) = agent.tools.iter().find(|tool| {
                matches!(
                    tool.as_str(),
                    "web_search" | "web_fetch" | "browser" | "browser_check"
                )
            }) {
                return Err(invalid(
                    format!("agents.{}.tools", agent.id),
                    format!("{tool} reaches hosts, and network: none reaches no host"),
                ));
            }
        }
    }
    if let Some(environment) = &file.environment {
        if environment.image.is_some() && !environment.recipes.is_empty() {
            return Err(invalid("environment", "set image or recipes, not both"));
        }
        if let Some(setup) = &environment.setup {
            check_param_ref(file, setup, "environment.setup", ParamKind::Text)?;
        }
    }
    validate_kind(file)?;
    Ok(warnings)
}

fn same_model_warning(field: &str) -> LoadoutWarning {
    LoadoutWarning {
        code: "same_model_reviewer".into(),
        field: field.into(),
        message: "the reviewer runs the same model as the writer; a same-model second look \
                  measured no gain"
            .into(),
    }
}

fn agents_with(file: &LoadoutFile, role: LoadoutRole) -> Vec<&LoadoutAgent> {
    file.agents
        .iter()
        .filter(|agent| agent.role == role)
        .collect()
}

fn validate_kind(file: &LoadoutFile) -> Result<(), LoadoutError> {
    let writers = agents_with(file, LoadoutRole::Writer).len();
    match file.kind {
        LoadoutKind::Fix => {
            if file.agents.len() != 1 || writers != 1 {
                return Err(invalid("agents", "a fix loadout has exactly one writer"));
            }
            if file.review.is_none() {
                return Err(invalid("review", "a fix loadout has a required review"));
            }
            if file.checks.is_empty() {
                return Err(invalid("checks", "a fix loadout has a required check"));
            }
            if file.qa.is_some() || file.audit.is_some() {
                return Err(invalid("kind", "a fix loadout has no qa or audit section"));
            }
        }
        LoadoutKind::Qa => {
            let explorers = agents_with(file, LoadoutRole::Explorer);
            if file.agents.len() != 1 || explorers.len() != 1 {
                return Err(invalid(
                    "agents",
                    "a qa loadout has exactly one explorer (no scouts, no merge, no verifier)",
                ));
            }
            let explorer = explorers[0];
            for tool in ["browser", "browser_check"] {
                if !explorer.tools.iter().any(|listed| listed == tool) {
                    return Err(invalid(
                        format!("agents.{}.tools", explorer.id),
                        format!("the explorer lists {tool}"),
                    ));
                }
            }
            let Some(qa) = &file.qa else {
                return Err(invalid("qa", "a qa loadout has a qa section"));
            };
            let repro_dir = qa.repro_dir.trim_end_matches('/');
            if repro_dir.is_empty() || repro_dir.starts_with('/') || repro_dir.contains("..") {
                return Err(invalid("qa.repro_dir", "a relative repository directory"));
            }
            let expected = format!("{repro_dir}/**");
            if explorer.writes.as_deref() != Some(std::slice::from_ref(&expected)) {
                return Err(invalid(
                    format!("agents.{}.writes", explorer.id),
                    format!("the explorer writes only its reproductions: [{expected:?}]"),
                ));
            }
            check_param_ref(file, &qa.target_url, "qa.target_url", ParamKind::Url)?;
            if let Some(reference) = &qa.reference_url {
                check_param_ref(file, reference, "qa.reference_url", ParamKind::Url)?;
            }
            if file.review.is_some() || file.audit.is_some() {
                return Err(invalid(
                    "kind",
                    "a qa loadout has no review and no audit section",
                ));
            }
        }
        LoadoutKind::Audit => {
            for role in [
                LoadoutRole::Planner,
                LoadoutRole::Worker,
                LoadoutRole::Integrator,
            ] {
                if agents_with(file, role).len() != 1 {
                    return Err(invalid(
                        "agents",
                        "an audit loadout has exactly one planner, one worker and one integrator",
                    ));
                }
            }
            if file.agents.len() != 3 {
                return Err(invalid(
                    "agents",
                    "an audit loadout has exactly three Agents",
                ));
            }
            for agent in &file.agents {
                if agent
                    .tools
                    .iter()
                    .any(|tool| CHANGING_TOOLS[..2].contains(&tool.as_str()))
                {
                    return Err(invalid(
                        format!("agents.{}.tools", agent.id),
                        "audit Agents are read-only: no write_file or edit_file",
                    ));
                }
            }
            let Some(audit) = &file.audit else {
                return Err(invalid("audit", "an audit loadout has an audit section"));
            };
            if audit.min_areas < MIN_AUDIT_AREAS
                || audit.max_areas > MAX_AUDIT_AREAS
                || audit.min_areas > audit.max_areas
            {
                return Err(invalid(
                    "audit",
                    format!(
                        "min_areas and max_areas lie within {MIN_AUDIT_AREAS}-{MAX_AUDIT_AREAS}"
                    ),
                ));
            }
            if file.qa.is_some() {
                return Err(invalid("kind", "an audit loadout has no qa section"));
            }
        }
        LoadoutKind::Custom => {
            if writers > 1 {
                return Err(invalid("agents", "at most one writer"));
            }
            if file.qa.is_some() || file.audit.is_some() {
                return Err(invalid(
                    "kind",
                    "qa and audit sections belong to those kinds",
                ));
            }
        }
    }
    Ok(())
}

/// Every built-in loadout, parsed. A built-in that fails to parse is a bug
/// caught by this module's tests.
pub fn builtin_loadouts() -> Vec<Result<Loadout, LoadoutError>> {
    [FIX_YAML, QA_YAML, AUDIT_YAML]
        .into_iter()
        .map(|text| parse_loadout(text, LoadoutSource::Builtin))
        .collect()
}

/// The ids of the built-in loadouts.
pub const BUILTIN_LOADOUT_IDS: [&str; 3] = ["fix", "qa", "audit"];

/// One user loadout file as read: the loadout, or why it cannot be used.
#[derive(Debug, Clone)]
pub struct UserLoadoutEntry {
    pub path: PathBuf,
    pub loadout: Result<Loadout, LoadoutError>,
}

/// Read every `*.yaml` / `*.yml` file directly in `dir`, sorted by name. A
/// missing directory is empty. A file that reuses a built-in id is refused.
/// Symbolic links and files over [`MAX_LOADOUT_BYTES`] are refused.
pub fn load_user_loadouts(dir: &Path) -> std::io::Result<Vec<UserLoadoutEntry>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let yaml = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == "yaml" || ext == "yml");
        if yaml {
            paths.push(path);
        }
    }
    paths.sort();
    paths.truncate(MAX_USER_LOADOUTS);
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let loadout = read_user_loadout(&path);
        out.push(UserLoadoutEntry { path, loadout });
    }
    Ok(out)
}

fn read_user_loadout(path: &Path) -> Result<Loadout, LoadoutError> {
    let parse_error = |reason: String| LoadoutError::Parse {
        source_name: path.display().to_string(),
        reason,
    };
    let metadata = std::fs::symlink_metadata(path).map_err(|e| parse_error(e.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(parse_error("not a regular file".into()));
    }
    if metadata.len() > MAX_LOADOUT_BYTES as u64 {
        return Err(parse_error(format!(
            "the file is larger than {MAX_LOADOUT_BYTES} bytes"
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|e| parse_error(e.to_string()))?;
    let loadout = parse_loadout(
        &text,
        LoadoutSource::User {
            path: path.to_path_buf(),
        },
    )?;
    if BUILTIN_LOADOUT_IDS.contains(&loadout.file.id.as_str()) {
        return Err(invalid(
            "id",
            format!(
                "{:?} is a built-in loadout; give your loadout another id",
                loadout.file.id
            ),
        ));
    }
    Ok(loadout)
}

/// Values given for a run's parameters.
pub type ParamValues = BTreeMap<String, String>;

/// A loadout with every parameter replaced by its value.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLoadout {
    pub loadout: Loadout,
    pub params: ParamValues,
    /// Each Agent's model, by Agent id.
    pub agent_models: BTreeMap<String, ModelSpec>,
    pub reviewer_model: Option<ModelSpec>,
    /// Each check's argv (shell checks as `sh -c`), timeout in seconds, by
    /// check name. `detected` checks resolve to `None` here; the daemon
    /// fills them from the Session.
    pub checks: Vec<ResolvedCheck>,
    /// The prompt with `{task}` and the other placeholders replaced.
    pub prompt: String,
    pub warnings: Vec<LoadoutWarning>,
}

/// One resolved required check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCheck {
    pub name: String,
    /// `None` for `detected` (filled by the daemon) and `e2e` (expanded by
    /// workstream `e2e`).
    pub argv: Option<Vec<String>>,
    pub timeout_secs: u64,
    pub e2e: Option<E2eCheck>,
}

fn param_value(
    file: &LoadoutFile,
    values: &ParamValues,
    name: &str,
) -> Result<Option<String>, LoadoutError> {
    let declared = file.params.get(name).ok_or_else(|| LoadoutError::Param {
        param: name.into(),
        reason: "not declared".into(),
    })?;
    let value = values
        .get(name)
        .cloned()
        .or_else(|| declared.default.clone());
    match (&value, declared.required) {
        (None, true) => Err(LoadoutError::Param {
            param: name.into(),
            reason: format!(
                "required: pass --param {name}=... ({})",
                if declared.description.is_empty() {
                    "no description"
                } else {
                    declared.description.as_str()
                }
            ),
        }),
        (Some(text), _) => {
            match declared.kind {
                ParamKind::Model if ModelSpec::parse(text).is_none() => {
                    return Err(LoadoutError::Param {
                        param: name.into(),
                        reason: "write provider:model".into(),
                    })
                }
                ParamKind::Url
                    if !(text.starts_with("http://") || text.starts_with("https://")) =>
                {
                    return Err(LoadoutError::Param {
                        param: name.into(),
                        reason: "write an http:// or https:// URL".into(),
                    })
                }
                _ => {}
            }
            Ok(value)
        }
        (None, false) => Ok(None),
    }
}

fn resolve_model(
    file: &LoadoutFile,
    values: &ParamValues,
    model: &ParamOr<ModelSpec>,
) -> Result<ModelSpec, LoadoutError> {
    match model {
        ParamOr::Value(spec) => Ok(spec.clone()),
        ParamOr::Param { param } => {
            let text = param_value(file, values, param)?.ok_or_else(|| LoadoutError::Param {
                param: param.clone(),
                reason: "a model parameter needs a value".into(),
            })?;
            ModelSpec::parse(&text).ok_or_else(|| LoadoutError::Param {
                param: param.clone(),
                reason: "write provider:model".into(),
            })
        }
    }
}

fn resolve_text(
    file: &LoadoutFile,
    values: &ParamValues,
    value: &ParamOr<String>,
) -> Result<Option<String>, LoadoutError> {
    match value {
        ParamOr::Value(text) => Ok(Some(text.clone())),
        ParamOr::Param { param } => param_value(file, values, param),
    }
}

/// Resolve every parameter of `loadout` with `values`. Unknown parameter
/// names are refused, so a typo never silently runs with a default.
pub fn resolve_loadout(
    loadout: &Loadout,
    values: &ParamValues,
    task: &str,
    repo: &str,
) -> Result<ResolvedLoadout, LoadoutError> {
    let file = &loadout.file;
    if let Some(unknown) = values.keys().find(|key| !file.params.contains_key(*key)) {
        return Err(LoadoutError::Param {
            param: unknown.clone(),
            reason: format!("loadout {} declares no such parameter", file.id),
        });
    }
    if task.trim().is_empty() {
        return Err(LoadoutError::Param {
            param: "task".into(),
            reason: "the task is empty".into(),
        });
    }
    let mut agent_models = BTreeMap::new();
    for agent in &file.agents {
        let model = resolve_model(file, values, &agent.model)?;
        if let Some(reason) = agent.runtime.model_refusal(&model) {
            return Err(match &agent.model {
                ParamOr::Param { param } => LoadoutError::Param {
                    param: param.clone(),
                    reason,
                },
                ParamOr::Value(_) => invalid(format!("agents.{}.model", agent.id), reason),
            });
        }
        agent_models.insert(agent.id.clone(), model);
    }
    let reviewer_model = match &file.review {
        Some(review) => Some(resolve_model(file, values, &review.model)?),
        None => None,
    };
    let mut warnings = loadout.warnings.clone();
    if let Some(reviewer) = &reviewer_model {
        let same = file
            .agents
            .iter()
            .filter(|agent| agent.role == LoadoutRole::Writer)
            .any(|agent| {
                agent_models
                    .get(&agent.id)
                    .is_some_and(|writer| writer.same_model(reviewer))
            });
        if same && !warnings.iter().any(|w| w.code == "same_model_reviewer") {
            warnings.push(same_model_warning("review.model"));
        }
    }
    let mut checks = Vec::with_capacity(file.checks.len());
    for check in &file.checks {
        let argv = if let Some(argv) = &check.run.argv {
            Some(argv.clone())
        } else if let Some(shell) = &check.run.shell {
            let command =
                resolve_text(file, values, shell)?.ok_or_else(|| LoadoutError::Param {
                    param: format!("checks.{}", check.name),
                    reason: "the check's command parameter has no value".into(),
                })?;
            Some(vec!["sh".into(), "-c".into(), command])
        } else {
            None
        };
        checks.push(ResolvedCheck {
            name: check.name.clone(),
            argv,
            timeout_secs: check_timeout_secs(check)?,
            e2e: check.run.e2e.clone(),
        });
    }
    let (target_url, reference_url) = match &file.qa {
        Some(qa) => (
            resolve_text(file, values, &qa.target_url)?,
            match &qa.reference_url {
                Some(reference) => resolve_text(file, values, reference)?,
                None => None,
            },
        ),
        None => (None, None),
    };
    let prompt = file
        .prompt
        .replace("{task}", task)
        .replace("{repo}", repo)
        .replace("{target_url}", target_url.as_deref().unwrap_or("(none)"))
        .replace(
            "{reference_url}",
            reference_url.as_deref().unwrap_or("(none configured)"),
        );
    let mut params = values.clone();
    for (name, declared) in &file.params {
        if let (false, Some(default)) = (params.contains_key(name), &declared.default) {
            params.insert(name.clone(), default.clone());
        }
    }
    Ok(ResolvedLoadout {
        loadout: loadout.clone(),
        params,
        agent_models,
        reviewer_model,
        checks,
        prompt,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(id: &str) -> Loadout {
        builtin_loadouts()
            .into_iter()
            .map(|loadout| loadout.expect("built-in loadouts parse"))
            .find(|loadout| loadout.file.id == id)
            .expect("built-in exists")
    }

    #[test]
    fn every_builtin_parses_and_validates() {
        let loadouts: Vec<_> = builtin_loadouts()
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("built-ins parse");
        let ids: Vec<_> = loadouts.iter().map(|l| l.file.id.as_str()).collect();
        assert_eq!(ids, BUILTIN_LOADOUT_IDS);
        assert_eq!(builtin("fix").file.kind, LoadoutKind::Fix);
        assert_eq!(builtin("qa").file.kind, LoadoutKind::Qa);
        assert_eq!(builtin("audit").file.kind, LoadoutKind::Audit);
        assert!(builtin("audit").file.opt_in);
        for loadout in &loadouts {
            assert_eq!(loadout.file.sandbox.network, "egress");
            assert_eq!(loadout.file.sandbox.workload, "hardened");
            assert_eq!(loadout.digest.len(), 64);
        }
    }

    #[test]
    fn fix_resolves_models_and_warns_on_the_same_reviewer_model() {
        let fix = builtin("fix");
        let mut values = ParamValues::new();
        values.insert("writer_model".into(), "openrouter:qwen/qwen3-coder".into());
        values.insert(
            "reviewer_model".into(),
            "openrouter:openai/gpt-oss-120b".into(),
        );
        let resolved = resolve_loadout(&fix, &values, "fix the bug", "/repo").unwrap();
        assert_eq!(
            resolved.reviewer_model,
            ModelSpec::parse("openrouter:openai/gpt-oss-120b")
        );
        assert!(resolved
            .warnings
            .iter()
            .all(|w| w.code != "same_model_reviewer"));
        assert!(resolved.prompt.contains("fix the bug"));

        values.insert(
            "reviewer_model".into(),
            "openrouter:qwen/qwen3-coder".into(),
        );
        let resolved = resolve_loadout(&fix, &values, "fix the bug", "/repo").unwrap();
        assert!(resolved
            .warnings
            .iter()
            .any(|w| w.code == "same_model_reviewer"));
    }

    #[test]
    fn a_missing_required_model_is_a_parameter_error() {
        let fix = builtin("fix");
        let error = resolve_loadout(&fix, &ParamValues::new(), "task", "/repo").unwrap_err();
        assert!(matches!(error, LoadoutError::Param { .. }), "{error}");
    }

    #[test]
    fn an_unknown_parameter_is_refused() {
        let qa = builtin("qa");
        let mut values = ParamValues::new();
        values.insert("explorer_model".into(), "ollama:qwen3:32b".into());
        values.insert("target_url".into(), "http://localhost:3000".into());
        values.insert("refrence_url".into(), "http://localhost:3001".into());
        let error = resolve_loadout(&qa, &values, "explore", "/repo").unwrap_err();
        assert!(error.to_string().contains("refrence_url"), "{error}");
    }

    #[test]
    fn model_specs_split_at_the_first_colon() {
        let spec = ModelSpec::parse("ollama:qwen3:32b").unwrap();
        assert_eq!(
            (spec.provider.as_str(), spec.model.as_str()),
            ("ollama", "qwen3:32b")
        );
        assert!(ModelSpec::parse("qwen3").is_none());
        assert!(ModelSpec::parse("anthropic/claude:x").is_none());
    }

    #[test]
    fn durations_and_check_timeouts_are_bounded() {
        assert_eq!(parse_duration_secs("90s"), Some(90));
        assert_eq!(parse_duration_secs("3m"), Some(180));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("0m"), None);
        assert_eq!(parse_duration_secs("5d"), None);
        let check = LoadoutCheck {
            name: "tests".into(),
            run: CheckRun {
                detected: true,
                ..CheckRun::default()
            },
            timeout: Some("31m".into()),
        };
        assert!(check_timeout_secs(&check).is_err());
        let check = LoadoutCheck {
            timeout: None,
            ..check
        };
        assert_eq!(
            check_timeout_secs(&check).unwrap(),
            DEFAULT_CHECK_TIMEOUT_SECS
        );
    }

    /// The qa re-smoke's `qa-tight` loadout validated with a budget too
    /// small for one model call, and its run then failed with exit 5 and an
    /// unreadable message. Validation refuses what no model's context can
    /// fit; admission checks the model's real context (the daemon's run
    /// admission tests).
    #[test]
    fn a_tokens_budget_below_one_call_is_refused() {
        let text = QA_YAML.replace("tokens: 6000000", "tokens: 10000");
        let error = parse_loadout(&text, LoadoutSource::Builtin).unwrap_err();
        assert_eq!(
            error.to_string(),
            "loadout field budgets.agent.tokens: 10000 tokens is less than one model call of \
             Agent explorer needs: a context of at least 2048 tokens plus 8192 output tokens \
             (agents.explorer.max_output_tokens), at least 10240 tokens; raise it to at least \
             10240 or lower agents.explorer.max_output_tokens"
        );
        // Exactly the minimum is enough; a smaller output bound lowers it.
        assert!(parse_loadout(
            &QA_YAML.replace("tokens: 6000000", "tokens: 10240"),
            LoadoutSource::Builtin
        )
        .is_ok());
        let smaller = QA_YAML.replace("tokens: 6000000", "tokens: 5000").replace(
            "    writes: [\"axocoatl-qa/**\"]\n",
            "    writes: [\"axocoatl-qa/**\"]\n    max_output_tokens: 2048\n",
        );
        assert!(parse_loadout(&smaller, LoadoutSource::Builtin).is_ok());
        // The reviewer's own budget and bound.
        let text = FIX_YAML.replace("tokens: 1500000", "tokens: 9000");
        let error = parse_loadout(&text, LoadoutSource::Builtin).unwrap_err();
        assert!(
            error.to_string().starts_with(
                "loadout field budgets.reviewer.tokens: 9000 tokens is less than one \
                              model call of the reviewer needs"
            ),
            "{error}"
        );
        // An Agent's own budget is named as such.
        let fix = builtin("fix");
        let mut file = fix.file.clone();
        file.agents[0].budget = Some(LoadoutLimits {
            tokens: 100,
            ..file.budgets.agent.clone()
        });
        let error = validate_loadout(&file).unwrap_err().to_string();
        assert!(
            error.starts_with(&format!(
                "loadout field agents.{}.budget.tokens: 100 tokens",
                file.agents[0].id
            )),
            "{error}"
        );
        // With the model's context known (admission), the minimum is that
        // context plus the output bound.
        let budget = &call_budgets(&builtin("qa").file)[0];
        assert_eq!(budget.minimum(Some(32768)), 40960);
        let tight = CallBudget {
            tokens: 40000,
            ..budget.clone()
        };
        let error = tight
            .refusal(Some(32768), Some("ollama:qwen3-coder:axocoatl-launch"))
            .unwrap()
            .to_string();
        assert!(
            error.contains(
                "40000 tokens is less than one model call of Agent explorer \
                            (ollama:qwen3-coder:axocoatl-launch) needs: the model's 32768-token \
                            context plus 8192 output tokens"
            ),
            "{error}"
        );
        assert!(error.contains("at least 40960"), "{error}");
        assert!(tight.refusal(Some(30000), None).is_none());
    }

    #[test]
    fn a_loadout_cannot_weaken_the_sandbox() {
        let text = FIX_YAML.replace("network: egress", "network: bridge");
        let error = parse_loadout(&text, LoadoutSource::Builtin).unwrap_err();
        assert!(error.to_string().contains("sandbox.network"), "{error}");
        let text = FIX_YAML.replace("workload: hardened", "workload: image");
        let error = parse_loadout(&text, LoadoutSource::Builtin).unwrap_err();
        assert!(error.to_string().contains("sandbox.workload"), "{error}");
    }

    #[test]
    fn network_none_keeps_tools_from_reaching_hosts() {
        let text = QA_YAML.replace("network: egress", "network: none");
        let error = parse_loadout(&text, LoadoutSource::Builtin).unwrap_err();
        assert!(error.to_string().contains("reaches hosts"), "{error}");
        let text = FIX_YAML.replace("network: egress", "network: none");
        assert!(parse_loadout(&text, LoadoutSource::Builtin).is_ok());
    }

    #[test]
    fn unknown_fields_and_schemas_are_refused() {
        let text = FIX_YAML.replace("kind: fix", "kind: fix\nmystery: 1");
        assert!(parse_loadout(&text, LoadoutSource::Builtin).is_err());
        let text = FIX_YAML.replace(LOADOUT_SCHEMA, "axocoatl.loadout/9");
        let error = parse_loadout(&text, LoadoutSource::Builtin).unwrap_err();
        assert!(error.to_string().contains("schema"), "{error}");
    }

    #[test]
    fn a_qa_loadout_keeps_one_explorer_writing_only_reproductions() {
        let qa = builtin("qa");
        let mut file = qa.file.clone();
        file.agents[0].writes = Some(vec!["src/**".into()]);
        assert!(validate_loadout(&file).is_err());
        let mut file = qa.file.clone();
        let mut scout = file.agents[0].clone();
        scout.id = "scout".into();
        file.agents.push(scout);
        assert!(validate_loadout(&file).is_err());
    }

    #[test]
    fn external_programs_run_only_as_the_writer() {
        let mut file = builtin("fix").file.clone();
        file.agents[0].runtime = AgentRuntime::ClaudeCode;
        file.agents[0].tools.clear();
        assert!(validate_loadout(&file).is_ok());
        let mut file = builtin("audit").file.clone();
        file.agents[1].runtime = AgentRuntime::Codex;
        assert!(validate_loadout(&file).is_err());
    }

    /// An external writer's model is the model its program calls through
    /// the route Axocoatl adds: `anthropic:` for Claude Code, `openai:` for
    /// Codex. Any other provider is refused, literal or as a parameter, so
    /// the identity a run records is the one that ran.
    #[test]
    fn an_external_writer_names_the_provider_its_program_calls() {
        let fix = builtin("fix");
        let mut file = fix.file.clone();
        file.agents[0].runtime = AgentRuntime::Codex;
        file.agents[0].model =
            ParamOr::Value(ModelSpec::parse("openrouter:openai/gpt-5.5").unwrap());
        assert_eq!(
            validate_loadout(&file).unwrap_err().to_string(),
            "loadout field agents.writer.model: a codex writer calls openai's model API, so its \
             model is openai:<the program's model name>; openrouter:openai/gpt-5.5 names the \
             provider openrouter"
        );
        file.agents[0].model = ParamOr::Value(ModelSpec::parse("openai:gpt-5.5").unwrap());
        assert!(validate_loadout(&file).is_ok());

        // As a parameter, at resolution.
        let mut file = fix.file.clone();
        file.agents[0].runtime = AgentRuntime::ClaudeCode;
        let loadout = Loadout {
            file,
            ..fix.clone()
        };
        let mut values = ParamValues::new();
        values.insert("writer_model".into(), "openai:gpt-5.5".into());
        values.insert("reviewer_model".into(), "ollama:gpt-oss:120b".into());
        assert_eq!(
            resolve_loadout(&loadout, &values, "fix it", "/repo")
                .unwrap_err()
                .to_string(),
            "loadout parameter writer_model: a claude-code writer calls anthropic's model API, \
             so its model is anthropic:<the program's model name>; openai:gpt-5.5 names the \
             provider openai"
        );
        values.insert("writer_model".into(), "anthropic:claude-haiku-4-5".into());
        let resolved = resolve_loadout(&loadout, &values, "fix it", "/repo").unwrap();
        assert!(resolved
            .warnings
            .iter()
            .all(|w| w.code != "same_model_reviewer"));

        // A reviewer on OpenRouter running the external writer's model is the
        // same model: the warning is given at resolution and in validation.
        values.insert(
            "reviewer_model".into(),
            "openrouter:anthropic/claude-haiku-4.5".into(),
        );
        let resolved = resolve_loadout(&loadout, &values, "fix it", "/repo").unwrap();
        assert!(resolved
            .warnings
            .iter()
            .any(|w| w.code == "same_model_reviewer"));
        let mut file = loadout.file.clone();
        file.agents[0].model =
            ParamOr::Value(ModelSpec::parse("anthropic:claude-haiku-4-5").unwrap());
        file.review.as_mut().unwrap().model =
            ParamOr::Value(ModelSpec::parse("openrouter:anthropic/claude-haiku-4.5").unwrap());
        assert!(validate_loadout(&file)
            .unwrap()
            .iter()
            .any(|w| w.code == "same_model_reviewer"));

        // External writers have their own budgets, checked at admission.
        assert!(call_budgets(&file)
            .iter()
            .all(|budget| budget.runtime == AgentRuntime::Native));
        let external = external_call_budgets(&file);
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].runtime, AgentRuntime::ClaudeCode);
        assert_eq!(external[0].tokens_field, "budgets.agent.tokens");
        let floor = CallFloor {
            tokens: 1_000_000,
            words: "the model's whole 1000000-token context window".into(),
            lower: None,
        };
        let tight = CallBudget {
            tokens: 400_000,
            ..external[0].clone()
        };
        assert_eq!(
            tight
                .refusal_with(&floor, Some("anthropic:claude-sonnet-5-5"))
                .unwrap()
                .to_string(),
            "loadout field budgets.agent.tokens: 400000 tokens is less than one model call of \
             Agent writer (anthropic:claude-sonnet-5-5) needs: the model's whole \
             1000000-token context window, at least 1000000 tokens; raise it to at least 1000000"
        );
        assert_eq!(
            AgentRuntime::Native.model_refusal(&ModelSpec::parse("x:y").unwrap()),
            None
        );
    }

    #[test]
    fn audit_agents_are_read_only_and_areas_bounded() {
        let audit = builtin("audit");
        let mut file = audit.file.clone();
        file.audit.as_mut().unwrap().max_areas = 9;
        assert!(validate_loadout(&file).is_err());
        let mut file = audit.file.clone();
        file.agents[1].tools.push("write_file".into());
        assert!(validate_loadout(&file).is_err());
    }

    #[test]
    fn user_loadouts_cannot_reuse_a_builtin_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fix.yaml"), FIX_YAML).unwrap();
        let custom = FIX_YAML.replace("id: fix", "id: fix-local");
        std::fs::write(dir.path().join("mine.yml"), custom).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let entries = load_user_loadouts(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].loadout.is_err(), "fix.yaml reuses a built-in id");
        assert_eq!(entries[1].loadout.as_ref().unwrap().file.id, "fix-local");
        assert!(load_user_loadouts(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }
}
