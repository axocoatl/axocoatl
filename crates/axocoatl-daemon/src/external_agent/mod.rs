//! External coding agents (Claude Code CLI, Codex CLI) run as programs inside
//! a locked-down Session: network egress, `--harden`, the non-root writer
//! user. Model traffic goes through routes whose credential the daemon adds
//! from the secret store (the container holds a placeholder); the Session's
//! certificate authority is trusted through `NODE_EXTRA_CA_CERTS` /
//! `SSL_CERT_FILE`; every model call lands in the network record; the
//! program's JSON output is parsed into the Session record.
//!
//! Starts from the adapter removed in b922287 (`git show b922287`): its
//! Codex app-server captures are kept as fixtures and still parse
//! ([`codex::parse_output`]).
//!
//! How an external writer runs (see `session_port.rs`, the activation
//! factory in `bootstrap_native_activation.rs`, and `run_external_activation`
//! in `bootstrap_external_agent.rs`):
//!
//! - Its definition is an ordinary retained Agent definition whose
//!   `provider` names the runtime (`claude-code`, `codex`; see
//!   [`external_agent_config`]) and whose tools are exactly `[bash]`: the
//!   program brings its own tools and runs commands, so the host captures
//!   the checkout before and after it as a writer's shell, and judges the
//!   write scope from those captures, exactly as for a native writer.
//! - Its activation is admitted, granted and captured like a native one.
//!   The tool loop makes one model call; that call is the program run. It
//!   reserves everything the grant still allows (tokens and cost), runs the
//!   program through the in-sandbox supervisor as the writer user, with the
//!   egress credential of that activation (proxy, route placeholders and the
//!   Session's trust files), and settles with the usage the program
//!   reports. With no report the usage stays incomplete and the reservation
//!   stays charged.
//! - The program's own model calls are the route requests of that credential.
//!   They are counted while it runs; when they reach the invocations the
//!   grant still allows, the program is stopped.
//! - Its output is parsed into [`ExternalItem`]s: the final text becomes the
//!   activation's answer, and a bounded work log (assistant text, tool calls
//!   and results as previews, usage, errors) is recorded in the activation's
//!   stream.
//!
//! Owner: workstream `agents`.

pub mod claude_code;
pub mod codex;
pub mod recipe_images;

use axocoatl_config::loadout::AgentRuntime;
use axocoatl_config::EgressRouteYaml;
use axocoatl_core::{AgentConfig, AgentRole, ChatMessage, MessageContent, MessageRole};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ExternalAgentError {
    #[error("external agent: {0}")]
    Invalid(String),
}

/// One external activation to run inside the Session container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActivationRequest {
    pub session_id: String,
    pub turn_id: String,
    pub node_id: String,
    pub runtime: AgentRuntime,
    pub model: String,
    /// The activation's full input (system instructions and request).
    pub prompt: String,
    /// Paths the program may change (checked by the activation's captures).
    pub writes: Option<Vec<String>>,
    pub timeout_ms: u64,
}

/// One item of the program's output, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalItem {
    AssistantText {
        text: String,
    },
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        output: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_microunits: Option<u64>,
    },
    Error {
        message: String,
    },
}

/// What an external activation produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActivationResult {
    pub final_answer: Option<String>,
    pub items: Vec<ExternalItem>,
    pub exit_code: Option<i32>,
    /// The program's own usage report; complete only when it reported one.
    pub usage_complete: bool,
}

impl ExternalActivationResult {
    /// The usage the program reported, if any: `(input, output, cost)`.
    pub fn usage(&self) -> Option<(u64, u64, Option<u64>)> {
        self.items.iter().rev().find_map(|item| match item {
            ExternalItem::Usage {
                input_tokens,
                output_tokens,
                cost_microunits,
            } => Some((*input_tokens, *output_tokens, *cost_microunits)),
            _ => None,
        })
    }

    /// The last error the program reported.
    pub fn last_error(&self) -> Option<&str> {
        self.items.iter().rev().find_map(|item| match item {
            ExternalItem::Error { message } => Some(message.as_str()),
            _ => None,
        })
    }

    /// The run produced an answer and exited cleanly.
    pub fn succeeded(&self) -> bool {
        self.final_answer.is_some() && self.exit_code == Some(0)
    }
}

/// The most bytes of one tool call's arguments or one result kept.
pub const MAX_PREVIEW_BYTES: usize = 2 * 1024;
/// The most bytes of one assistant text or error message kept.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// The most bytes of the final answer kept.
pub const MAX_ANSWER_BYTES: usize = 256 * 1024;
/// The most items one run keeps; more are summarized in one error item.
pub const MAX_ITEMS: usize = 4096;
/// The program's stdout is written to a file in the container bounded to
/// this many bytes (the program is stopped when it writes more).
pub const MAX_OUTPUT_FILE_BYTES: usize = 16 * 1024 * 1024;
/// What the host reads back of that file: all of it when it fits, else its
/// first and last halves around a marker line.
pub const MAX_READ_BACK_BYTES: usize = 8 * 1024 * 1024;
/// The longest prompt sent on the program's stdin.
pub const MAX_PROMPT_BYTES: usize = 4 * 1024 * 1024;
/// The marker line the wrapper prints in place of output it left out.
pub const TRUNCATED_MARKER_TYPE: &str = "axocoatl_truncated";

/// The definition `provider` that names an external runtime.
pub fn runtime_provider(runtime: AgentRuntime) -> Option<&'static str> {
    match runtime {
        AgentRuntime::Native => None,
        AgentRuntime::ClaudeCode => Some(claude_code::PROVIDER),
        AgentRuntime::Codex => Some(codex::PROVIDER),
    }
}

/// The external runtime a definition's `provider` names, if any.
pub fn runtime_for_provider(provider: &str) -> Option<AgentRuntime> {
    match provider {
        claude_code::PROVIDER => Some(AgentRuntime::ClaudeCode),
        codex::PROVIDER => Some(AgentRuntime::Codex),
        _ => None,
    }
}

/// A program model name: 1 to 256 printable characters, not starting with
/// `-` (it is passed as an argument).
pub fn validate_model(model: &str) -> Result<(), ExternalAgentError> {
    if model.is_empty()
        || model.len() > 256
        || model.starts_with('-')
        || model.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ExternalAgentError::Invalid(format!(
            "{model:?} is not a model name for an external agent"
        )));
    }
    Ok(())
}

/// The retained definition of an external writer, from the configuration
/// core builds for the loadout Agent: `provider` names the runtime, `model`
/// is the program's model, tools are exactly `[bash]` (the program brings
/// its own; the host captures the checkout as a writer's shell), the role is
/// the autonomous lead, and no per-call sampling or token budget applies
/// (the grant bounds the run). Instructions, writes, id and name are kept.
///
/// Core calls this from its inline-definition path for an Agent whose
/// `runtime` is not `native`.
pub fn external_agent_config(
    mut config: AgentConfig,
    runtime: AgentRuntime,
    model: &str,
) -> Result<AgentConfig, ExternalAgentError> {
    let provider = runtime_provider(runtime).ok_or_else(|| {
        ExternalAgentError::Invalid("a native Agent is not an external agent".into())
    })?;
    validate_model(model)?;
    config.provider = provider.into();
    config.model = model.into();
    config.tools = vec!["bash".into()];
    config.role = AgentRole::Autonomous;
    config.token_budget = None;
    config.sampling = Default::default();
    config.max_tool_rounds = None;
    validate_external_config(&config)?;
    Ok(config)
}

/// The runtime of a retained external definition, refusing any shape
/// [`external_agent_config`] does not produce.
pub fn validate_external_config(config: &AgentConfig) -> Result<AgentRuntime, ExternalAgentError> {
    let runtime = runtime_for_provider(&config.provider).ok_or_else(|| {
        ExternalAgentError::Invalid(format!(
            "{} does not name an external runtime",
            config.provider
        ))
    })?;
    validate_model(&config.model)?;
    if config.role != AgentRole::Autonomous
        || config.tools != ["bash"]
        || config.token_budget.is_some()
        || config.sampling.max_tokens.is_some()
        || config.sampling.response_format.is_some()
    {
        return Err(ExternalAgentError::Invalid(format!(
            "the {} writer's definition must be the autonomous writer with tools [bash] and no \
             per-call limits",
            config.provider
        )));
    }
    if let Some(writes) = &config.writes {
        axocoatl_session::path_scope::validate_write_scope(writes)
            .map_err(|reason| ExternalAgentError::Invalid(format!("writes: {reason}")))?;
    }
    Ok(runtime)
}

/// The provider profile retained with an external definition: what program
/// the definition runs. Evidence only; the definition is the authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalRuntimeProfile {
    pub schema_version: u32,
    pub runtime: AgentRuntime,
    pub model: String,
    /// The npm package the recipe installs, and its pinned version.
    pub package: String,
    pub version: String,
    /// The recipe whose image holds the program.
    pub recipe: String,
}

impl ExternalRuntimeProfile {
    pub fn for_runtime(runtime: AgentRuntime, model: &str) -> Result<Self, ExternalAgentError> {
        validate_model(model)?;
        let (package, version, recipe) = match runtime {
            AgentRuntime::ClaudeCode => (
                claude_code::CLAUDE_CODE_PACKAGE,
                claude_code::CLAUDE_CODE_VERSION,
                "claude-code",
            ),
            AgentRuntime::Codex => (codex::CODEX_PACKAGE, codex::CODEX_VERSION, "codex"),
            AgentRuntime::Native => {
                return Err(ExternalAgentError::Invalid(
                    "a native Agent has no external runtime profile".into(),
                ))
            }
        };
        Ok(Self {
            schema_version: 1,
            runtime,
            model: model.into(),
            package: package.into(),
            version: version.into(),
            recipe: recipe.into(),
        })
    }
}

/// The egress routes (with credential names) a runtime needs. Core merges
/// them into a loadout Session's policy (`loadout::egress::loadout_policy`);
/// each `credential` resolves from the config's `credentials`, else from the
/// secret store (`secret_store::credential_source`). The paths are the ones
/// the pinned programs were seen to call against a recording fake API
/// (Claude Code 2.1.292 with an OAuth token: `POST /v1/messages?beta=true`;
/// Codex 0.160.1 through its HTTP provider: `POST /v1/responses`); anything
/// else is refused and recorded with a hint.
pub fn routes_for(runtime: AgentRuntime) -> Result<Vec<EgressRouteYaml>, ExternalAgentError> {
    let (host, credential, placeholder, path) = match runtime {
        AgentRuntime::Native => return Ok(Vec::new()),
        AgentRuntime::ClaudeCode => (
            claude_code::API_HOST,
            claude_code::CLAUDE_CODE_SECRET,
            claude_code::TOKEN_PLACEHOLDER_ENV,
            "/v1/messages",
        ),
        AgentRuntime::Codex => (
            codex::API_HOST,
            codex::CODEX_SECRET,
            codex::TOKEN_PLACEHOLDER_ENV,
            "/v1/responses",
        ),
    };
    let route = EgressRouteYaml {
        host: host.into(),
        ports: None,
        credential: Some(credential.into()),
        inject: Some(axocoatl_config::RouteInjectYaml {
            basic: None,
            header: Some("Authorization".into()),
            format: Some("Bearer {}".into()),
        }),
        bindings: Some(vec![axocoatl_config::RouteForYaml::Agent]),
        upstream_ca: None,
        access: None,
        rules: vec![axocoatl_config::RouteRuleYaml {
            methods: vec!["POST".into()],
            path: path.into(),
            query: Default::default(),
        }],
        env_placeholders: vec![placeholder.into()],
        allow_encoded_responses: false,
        allow_set_cookie: false,
        max_request_bytes: 64 * 1024 * 1024,
    };
    Ok(vec![route])
}

/// The program's argv for one activation (no wrapper): `env` with the
/// runtime's fixed variables, then the program reading its prompt from
/// stdin.
pub fn program_argv(runtime: AgentRuntime, model: &str) -> Result<Vec<String>, ExternalAgentError> {
    let (env, argv): (&[(&str, &str)], Vec<String>) = match runtime {
        AgentRuntime::Native => {
            return Err(ExternalAgentError::Invalid(
                "a native Agent runs no program".into(),
            ))
        }
        AgentRuntime::ClaudeCode => (claude_code::ENV, claude_code::argv(model)?),
        AgentRuntime::Codex => (codex::ENV, codex::argv(model)?),
    };
    let mut command = vec!["env".to_string()];
    command.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
    command.extend(argv);
    Ok(command)
}

/// Refuses to run as root or without no-new-privileges (the hardened
/// workload and the supervisor's `--harden`); then runs `"$@"` with its
/// stdout in a private file bounded to `$1` bytes (the
/// program is stopped by a closed pipe when it writes more), then prints the
/// file, or, when it is longer than `$2`, its first and last halves around a
/// marker line, and exits with the program's status.
const OUTPUT_WRAPPER: &str = r#"set -u
if [ "$(id -u)" = 0 ]; then
  echo "axocoatl: an external agent never runs as root; this Session has no hardened workload users" >&2
  exit 126
fi
if [ -r /proc/self/status ] && ! grep -q '^NoNewPrivs:[[:space:]]*1' /proc/self/status; then
  echo "axocoatl: an external agent runs only under the supervisor's --harden" >&2
  exit 126
fi
limit=$1; keep=$2; shift 2
dir=$(mktemp -d /tmp/axocoatl-external.XXXXXX) || exit 125
{ "$@"; printf '%s' "$?" > "$dir/status"; } | head -c "$limit" > "$dir/out"
status=$(cat "$dir/status" 2>/dev/null || printf 141)
size=$(wc -c < "$dir/out" | tr -d ' ')
if [ "$size" -le "$keep" ]; then
  cat "$dir/out"
else
  half=$((keep / 2))
  head -c "$half" "$dir/out"
  printf '\n{"type":"axocoatl_truncated","bytes":%s,"kept":%s}\n' "$size" "$keep"
  tail -c "$half" "$dir/out" | tail -n +2
fi
if [ "$size" -ge "$limit" ]; then
  printf '\n{"type":"axocoatl_truncated","bytes":%s,"limit":%s}\n' "$size" "$limit"
fi
rm -rf "$dir"
exit "$status""#;

/// The full argv one activation runs through the supervisor: the bounded
/// output wrapper around [`program_argv`].
pub fn command_argv(runtime: AgentRuntime, model: &str) -> Result<Vec<String>, ExternalAgentError> {
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        OUTPUT_WRAPPER.to_string(),
        "sh".to_string(),
        MAX_OUTPUT_FILE_BYTES.to_string(),
        MAX_READ_BACK_BYTES.to_string(),
    ];
    argv.extend(program_argv(runtime, model)?);
    Ok(argv)
}

/// Parse a run's stdout for `runtime`.
pub fn parse_output(
    runtime: AgentRuntime,
    stdout: &[u8],
) -> Result<ExternalActivationResult, ExternalAgentError> {
    match runtime {
        AgentRuntime::ClaudeCode => claude_code::parse_output(stdout),
        AgentRuntime::Codex => codex::parse_output(stdout),
        AgentRuntime::Native => Err(ExternalAgentError::Invalid(
            "a native Agent has no program output".into(),
        )),
    }
}

/// `text` cut to at most `max` bytes on a character boundary, with a note
/// of what was left out.
pub fn bound_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} more bytes]", &text[..end], text.len() - end)
}

fn message_text(message: &ChatMessage) -> String {
    match &message.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                axocoatl_core::ContentPart::Text(text) => Some(text.as_str()),
                axocoatl_core::ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// What the host tells every external program about where it runs.
pub const SESSION_NOTE: &str = "You are running inside an Axocoatl Session container. Work in \
the current directory, which is the repository checkout. Your model API is the only network \
access; any other connection is refused and recorded. When you are done, end with a short \
summary of what you changed and what you checked.";

/// The prompt an external program gets on stdin, from the activation's
/// request: the host's note, the Agent's instructions (system messages),
/// earlier exchanges of this conversation, and the request itself.
pub fn prompt_text(messages: &[ChatMessage]) -> String {
    let mut sections = vec![format!("# Axocoatl\n{SESSION_NOTE}")];
    let system: Vec<String> = messages
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .map(message_text)
        .filter(|text| !text.trim().is_empty())
        .collect();
    if !system.is_empty() {
        sections.push(format!("# Instructions\n{}", system.join("\n\n")));
    }
    let rest: Vec<&ChatMessage> = messages
        .iter()
        .filter(|message| message.role != MessageRole::System)
        .collect();
    let last_user = rest
        .iter()
        .rposition(|message| message.role == MessageRole::User);
    for (index, message) in rest.iter().enumerate() {
        let text = message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        let heading = match (&message.role, Some(index) == last_user) {
            (MessageRole::User, true) => "# Task",
            (MessageRole::User, false) => "# Earlier request",
            (MessageRole::Assistant, _) => "# Your earlier answer",
            (MessageRole::Tool, _) => "# Earlier tool result",
            (MessageRole::System, _) => continue,
        };
        sections.push(format!("{heading}\n{text}"));
    }
    sections.join("\n\n")
}

/// The most bytes of the work log recorded in the activation's stream.
pub const MAX_WORK_LOG_BYTES: usize = 384 * 1024;

/// The activation's work log: one line per item (bounded previews), led by
/// what ran and how it ended. Recorded as the activation's reasoning stream;
/// the final answer is recorded as its text.
pub fn work_log(
    runtime: AgentRuntime,
    model: &str,
    result: &ExternalActivationResult,
    route_requests: u64,
    stopped: Option<&str>,
) -> Vec<String> {
    let program = match runtime {
        AgentRuntime::ClaudeCode => format!(
            "{} {}",
            claude_code::CLAUDE_CODE_PACKAGE,
            claude_code::CLAUDE_CODE_VERSION
        ),
        AgentRuntime::Codex => format!("{} {}", codex::CODEX_PACKAGE, codex::CODEX_VERSION),
        AgentRuntime::Native => "native".into(),
    };
    let mut lines = vec![format!(
        "[external agent] {program}, model {model}: exit {}, {route_requests} model request(s) \
         through the Session's route{}\n",
        result
            .exit_code
            .map_or_else(|| "unknown".to_string(), |code| code.to_string()),
        stopped.map_or_else(String::new, |reason| format!(", stopped: {reason}")),
    )];
    let mut used = lines[0].len();
    let answer = result.final_answer.as_deref();
    let mut left_out = 0usize;
    for item in &result.items {
        let line = match item {
            // The final answer is the activation's text, not its log.
            ExternalItem::AssistantText { text } if Some(text.as_str()) == answer => continue,
            ExternalItem::AssistantText { text } => {
                format!("[assistant] {}\n", bound_text(text, MAX_PREVIEW_BYTES))
            }
            ExternalItem::ToolCall { name, arguments } => format!(
                "[tool call] {name}: {}\n",
                bound_text(arguments, MAX_PREVIEW_BYTES)
            ),
            ExternalItem::ToolResult {
                name,
                output,
                is_error,
            } => format!(
                "[tool result] {name}{}: {}\n",
                if *is_error { " (error)" } else { "" },
                bound_text(output, MAX_PREVIEW_BYTES)
            ),
            ExternalItem::Usage {
                input_tokens,
                output_tokens,
                cost_microunits,
            } => format!(
                "[usage] {input_tokens} input and {output_tokens} output tokens{}, as the \
                 program reported\n",
                cost_microunits.map_or_else(String::new, |cost| format!(
                    ", ${}.{:06}",
                    cost / 1_000_000,
                    cost % 1_000_000
                ))
            ),
            ExternalItem::Error { message } => {
                format!("[error] {}\n", bound_text(message, MAX_PREVIEW_BYTES))
            }
        };
        if used + line.len() > MAX_WORK_LOG_BYTES {
            left_out += 1;
            continue;
        }
        used += line.len();
        lines.push(line);
    }
    if result.usage().is_none() {
        lines.push(
            "[usage] the program reported no usage; the whole reservation stays charged\n".into(),
        );
    }
    if left_out > 0 {
        lines.push(format!(
            "[work log] {left_out} more item(s) left out of the record\n"
        ));
    }
    lines
}

/// Shared JSON helpers for the parsers.
pub(crate) mod json {
    use serde_json::Value;

    pub(crate) fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
        path.iter()
            .try_fold(value, |value, key| value.get(*key))
            .and_then(Value::as_str)
    }

    pub(crate) fn u64_at(value: &Value, path: &[&str]) -> Option<u64> {
        path.iter()
            .try_fold(value, |value, key| value.get(*key))
            .and_then(Value::as_u64)
    }

    /// A compact preview of any JSON value.
    pub(crate) fn preview(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

/// Accumulates items under [`MAX_ITEMS`].
#[derive(Default)]
pub(crate) struct Items {
    items: Vec<ExternalItem>,
    dropped: usize,
}

impl Items {
    pub(crate) fn push(&mut self, item: ExternalItem) {
        let item = match item {
            ExternalItem::AssistantText { text } => ExternalItem::AssistantText {
                text: bound_text(&text, MAX_TEXT_BYTES),
            },
            ExternalItem::ToolCall { name, arguments } => ExternalItem::ToolCall {
                name: bound_text(&name, 256),
                arguments: bound_text(&arguments, MAX_PREVIEW_BYTES),
            },
            ExternalItem::ToolResult {
                name,
                output,
                is_error,
            } => ExternalItem::ToolResult {
                name: bound_text(&name, 256),
                output: bound_text(&output, MAX_PREVIEW_BYTES),
                is_error,
            },
            ExternalItem::Error { message } => ExternalItem::Error {
                message: bound_text(&message, MAX_TEXT_BYTES),
            },
            usage @ ExternalItem::Usage { .. } => usage,
        };
        if self.items.len() + 1 >= MAX_ITEMS && !matches!(item, ExternalItem::Usage { .. }) {
            self.dropped += 1;
            return;
        }
        self.items.push(item);
    }

    pub(crate) fn finish(mut self) -> Vec<ExternalItem> {
        if self.dropped > 0 {
            self.items.push(ExternalItem::Error {
                message: format!("{} more output item(s) left out", self.dropped),
            });
        }
        self.items
    }
}

/// Each non-empty line of `stdout` as JSON, or an error item for a line that
/// is not JSON (bounded) and the wrapper's truncation marker.
pub(crate) fn json_lines(stdout: &[u8], items: &mut Items) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    for (number, line) in stdout.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) if json::str_at(&value, &["type"]) == Some(TRUNCATED_MARKER_TYPE) => {
                items.push(ExternalItem::Error {
                    message: format!(
                        "the program's output was {} bytes; only its start and end were read",
                        json::u64_at(&value, &["bytes"]).unwrap_or(0)
                    ),
                });
            }
            Ok(value) => values.push(value),
            Err(_) => items.push(ExternalItem::Error {
                message: format!(
                    "output line {} is not JSON: {}",
                    number + 1,
                    bound_text(&String::from_utf8_lossy(line), 256)
                ),
            }),
        }
    }
    values
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
