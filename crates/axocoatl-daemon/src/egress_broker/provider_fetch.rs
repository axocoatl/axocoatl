//! The provider-fetch check: request bodies on a route to a model API that
//! would make the provider reach other hosts, or run tools itself, are
//! refused.
//!
//! A route checks method, path and query; it does not read bodies. On a
//! model API that leaves a way past the Session's egress policy: a request
//! the route allows can ask the provider to fetch a URL (an image, a
//! document, a file), to search the web, to connect to an MCP server, or to
//! run a tool on its own servers, and the provider then reaches hosts the
//! Session never could, with what the request put in a query string or a
//! search, and returns what it found. Claude Code 2.1.292's WebSearch tool
//! asks Anthropic for its server-side `web_search` this way, and Codex
//! 0.160.1 offers OpenAI's hosted `web_search` with live access on every
//! request unless it is turned off.
//!
//! A route that checks bodies ([`super::rules::ProviderFetch::Refused`])
//! reads each request body whole (at most [`MAX_CHECKED_BODY_BYTES`]) before
//! anything is recorded as allowed or sent, and [`check`] decides from the
//! endpoint's request format ([`Dialect::for_path`]):
//!
//! - **Tools are allowed by type, never refused by type.** Only tools the
//!   client runs pass: Anthropic custom tools (no `type`, or `custom`) and
//!   the client tools `bash_*`, `text_editor_*`, `computer_*` and `memory_*`
//!   (a date after the name); OpenAI `function`, `custom`, `local_shell`,
//!   `apply_patch`, `shell` without a hosted environment, and `tool_search`
//!   with `execution: "client"`; Chat Completions `function` and `custom`;
//!   Gemini tools with only `functionDeclarations`. Every other tool, such
//!   as `web_search_*`, `web_fetch_*`, `code_execution_*`, `mcp`,
//!   `file_search`, `code_interpreter`, `image_generation`,
//!   `computer_use_preview`, `googleSearch` or `urlContext`, or a type
//!   this check has never seen, is refused.
//! - `mcp_servers` (Anthropic's MCP connector) that is not empty, any
//!   OpenRouter `plugins` entry that is not `enabled: false`, and
//!   `web_search_options`.
//! - Content the provider fetches from a URL: an Anthropic `source` of
//!   type `url` (or of a type other than `base64`, `text`, `content` and
//!   `file`), an `image_url`, `video_url`, `audio_url` or `document_url`
//!   that is not a `data:` URL, any `file_url`, a `file_data` string with a
//!   URL scheme other than `data:`, and a Gemini `fileData.fileUri` outside
//!   Gemini's own Files API.
//! - A model that searches the web by itself: an OpenRouter `:online`
//!   variant, OpenAI's `*-search-preview`, `*-search-api` and
//!   `*-deep-research` models, and OpenRouter's `perplexity/` models.
//! - An OpenAI stored prompt (`prompt` with an `id`), which can bring tools
//!   and inputs the request does not show.
//!
//! These are looked for anywhere in the body except inside the parts the
//! provider does not act on: JSON schemas (`input_schema`, `parameters`,
//! `schema`, `json_schema`, `response_schema` and their Gemini spellings),
//! `metadata` and `client_metadata`, the arguments the model gave a tool
//! call (`input` of a `*tool_use` block, `action` of a `*shell_call`), and
//! the definitions of allowed tools.
//!
//! A body is read with `strict_json`: one that is not JSON, that
//! repeats a key in one object, or that nests deeper than
//! [`MAX_CHECKED_DEPTH`] is refused, since the provider could read it
//! otherwise than the check did. A body to an endpoint of no format the
//! check knows is refused too: the check cannot tell what it asks for.
//! What a request asks for in its headers (such as `anthropic-beta`) or
//! through settings stored with the provider (an account's default
//! plugins, a preset) is not seen.

use super::strict_json::{self, Value};

/// Largest request body a checking route reads; a larger one is refused
/// with `413`.
pub const MAX_CHECKED_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Deepest nesting of arrays and objects the check reads.
pub const MAX_CHECKED_DEPTH: usize = 512;

/// The request format of a model endpoint, from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Anthropic's Messages API: `…/messages`, `…/messages/count_tokens`.
    Messages,
    /// OpenAI's Responses API: `…/responses`, `…/responses/compact` and
    /// `…/responses/input_tokens`, also as the ChatGPT backend's Codex API
    /// serves it (`/backend-api/codex/responses`).
    Responses,
    /// Chat Completions: `…/chat/completions` and `…/completions`, as
    /// OpenAI, OpenRouter and Gemini's OpenAI-compatible API serve it.
    ChatCompletions,
    /// Gemini: `…/models/<model>:generateContent`,
    /// `:streamGenerateContent` and `:countTokens`.
    GenerateContent,
}

impl Dialect {
    /// The format of requests to `path` (without its query), or `None` for
    /// an endpoint the check does not know.
    pub fn for_path(path: &str) -> Option<Self> {
        let ends = |suffix: &str| path.ends_with(suffix);
        if ends("/messages") || ends("/messages/count_tokens") {
            Some(Self::Messages)
        } else if ends("/responses")
            || ends("/responses/compact")
            || ends("/responses/input_tokens")
        {
            Some(Self::Responses)
        } else if ends("/chat/completions") || ends("/completions") {
            Some(Self::ChatCompletions)
        } else {
            let last = path.rsplit('/').next().unwrap_or_default();
            match last.split_once(':') {
                Some((model, "generateContent" | "streamGenerateContent" | "countTokens"))
                    if !model.is_empty() =>
                {
                    Some(Self::GenerateContent)
                }
                _ => None,
            }
        }
    }
}

/// Why a body is refused: what it asks for, and where. Never holds a value
/// from the body other than a tool type, plugin id or model name in a
/// plain spelling ([`shown`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// What the body asks for, such as `tool type web_search_20250305, which
    /// the provider runs itself`.
    pub construct: String,
    /// Where: a JSON path such as `$.tools[0]`, or `$` for the whole body.
    pub at: String,
}

/// Check a request body sent to `path` on a route that checks bodies. An
/// empty body passes.
pub fn check(path: &str, body: &[u8]) -> Result<(), Refused> {
    if body
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        return Ok(());
    }
    let Some(dialect) = Dialect::for_path(path) else {
        return Err(Refused {
            construct: "a request body to an endpoint whose request format the check does not \
                        know, so it cannot tell what the body asks the provider to do"
                .into(),
            at: "$".into(),
        });
    };
    check_dialect(dialect, body)
}

/// [`check`] for a body of `dialect`.
pub fn check_dialect(dialect: Dialect, body: &[u8]) -> Result<(), Refused> {
    let value = strict_json::parse(body, MAX_CHECKED_DEPTH).map_err(|error| Refused {
        construct: format!(
            "a body the check cannot read as one JSON value ({error}), which the provider might \
             read otherwise"
        ),
        at: "$".into(),
    })?;
    let mut walk = Walk {
        dialect,
        path: Vec::new(),
    };
    walk.value(&value)
}

/// A tool type, plugin id or model name as a refusal shows it: only a
/// plain spelling of at most 80 characters, never anything that could carry
/// data from the body.
pub fn shown(text: &str) -> String {
    if !text.is_empty()
        && text.len() <= 80
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:/@".contains(&byte))
    {
        text.to_string()
    } else {
        "(not shown)".into()
    }
}

/// Keys whose values the provider does not act on as content: schemas,
/// metadata.
const OPAQUE_KEYS: [&str; 14] = [
    "input_schema",
    "parameters",
    "parameters_json_schema",
    "parametersJsonSchema",
    "schema",
    "json_schema",
    "output_schema",
    "response_schema",
    "responseSchema",
    "response_json_schema",
    "responseJsonSchema",
    "metadata",
    "client_metadata",
    "arguments",
];

/// Anthropic tools the client runs: `bash_`, `text_editor_`, `computer_`
/// and `memory_`, each followed by an eight-digit date.
fn anthropic_client_type(kind: &str) -> bool {
    ["bash_", "text_editor_", "computer_", "memory_"]
        .iter()
        .any(|prefix| {
            kind.strip_prefix(prefix).is_some_and(|date| {
                date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
}

/// Whether `text` is a `data:` URL (after leading whitespace, in any case).
fn is_data_url(text: &str) -> bool {
    text.trim_start()
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

/// Whether `text` starts with a URL scheme (`letter *(letter / digit / + /
/// - / .) :`). Base64 data never does.
fn has_scheme(text: &str) -> bool {
    let text = text.trim_start();
    match text.find(':') {
        Some(end) if end > 0 => {
            let scheme = &text[..end];
            scheme.as_bytes()[0].is_ascii_alphabetic()
                && scheme
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
        }
        _ => false,
    }
}

/// What a model searches the web by itself, when it does.
fn web_model(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.ends_with(":online") {
        Some("an :online model, for which OpenRouter searches the web")
    } else if lower.contains("search-preview") || lower.contains("search-api") {
        Some("a search model, which searches the web")
    } else if lower.contains("deep-research") {
        Some("a deep research model, which searches the web")
    } else if lower.starts_with("perplexity/") {
        Some("a Perplexity model, which searches the web")
    } else {
        None
    }
}

/// Gemini's own Files API, whose files the provider does not fetch from
/// elsewhere.
const GEMINI_FILES_PREFIX: &str = "https://generativelanguage.googleapis.com/";

enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

struct Walk<'a> {
    dialect: Dialect,
    path: Vec<Step<'a>>,
}

/// Longest JSON path a refusal shows.
const MAX_PATH_CHARS: usize = 240;

impl<'a> Walk<'a> {
    fn at(&self) -> String {
        let mut text = String::from("$");
        for step in &self.path {
            match step {
                Step::Index(index) => text.push_str(&format!("[{index}]")),
                Step::Key(key) => {
                    let plain = !key.is_empty()
                        && key.len() <= 64
                        && key
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
                    if plain {
                        text.push('.');
                        text.push_str(key);
                    } else {
                        text.push_str("[?]");
                    }
                }
            }
            if text.len() > MAX_PATH_CHARS {
                text.truncate(MAX_PATH_CHARS);
                text.push('…');
                break;
            }
        }
        text
    }

    fn refuse(&self, construct: impl Into<String>) -> Result<(), Refused> {
        Err(Refused {
            construct: construct.into(),
            at: self.at(),
        })
    }

    fn value(&mut self, value: &'a Value) -> Result<(), Refused> {
        match value {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.path.push(Step::Index(index));
                    self.value(item)?;
                    self.path.pop();
                }
                Ok(())
            }
            Value::Object(entries) => {
                let kind = value.get("type").and_then(Value::as_str);
                for (key, child) in entries {
                    self.path.push(Step::Key(key));
                    self.entry(kind, key, child)?;
                    self.path.pop();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn entry(&mut self, kind: Option<&str>, key: &'a str, child: &'a Value) -> Result<(), Refused> {
        // Inside the body, never at its top, where `input` is the request's
        // own content in the Responses format.
        let nested = self.path.len() > 1;
        match key {
            _ if OPAQUE_KEYS.contains(&key) => Ok(()),
            // The arguments the model wrote for a tool call.
            "input" if nested && kind.is_some_and(|kind| kind.ends_with("tool_use")) => Ok(()),
            "action" if nested && kind.is_some_and(|kind| kind.ends_with("shell_call")) => Ok(()),
            "tools" => self.tools(child),
            "mcp_servers" => match child {
                Value::Null => Ok(()),
                Value::Array(servers) if servers.is_empty() => Ok(()),
                _ => self.refuse("MCP servers the provider connects to (mcp_servers)"),
            },
            "plugins" => self.plugins(child),
            "web_search_options" => match child {
                Value::Null => Ok(()),
                _ => self.refuse("web search by the provider (web_search_options)"),
            },
            "image_url" | "video_url" | "audio_url" | "document_url" => {
                let remote = match child {
                    Value::String(url) => !is_data_url(url),
                    Value::Object(_) => child
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|url| !is_data_url(url)),
                    _ => false,
                };
                if remote {
                    self.refuse(format!(
                        "content the provider fetches from a URL ({key} that is not a data: URL)"
                    ))
                } else {
                    self.value(child)
                }
            }
            "file_url" => match child {
                Value::Null => Ok(()),
                _ => self.refuse("a file the provider fetches from a URL (file_url)"),
            },
            "file_data" | "fileData" => match child {
                Value::String(data) if has_scheme(data) && !is_data_url(data) => self.refuse(
                    "a file the provider fetches from a URL (file_data that is a URL, not data)",
                ),
                Value::Object(_) => {
                    for name in ["fileUri", "file_uri"] {
                        if let Some(uri) = child.get(name).and_then(Value::as_str) {
                            let inside = uri
                                .trim_start()
                                .get(..GEMINI_FILES_PREFIX.len())
                                .is_some_and(|prefix| {
                                    prefix.eq_ignore_ascii_case(GEMINI_FILES_PREFIX)
                                });
                            if !inside {
                                self.path.push(Step::Key(name));
                                return self.refuse(
                                    "a file the provider fetches from a URL (a file URI outside \
                                     Gemini's Files API)",
                                );
                            }
                        }
                    }
                    self.value(child)
                }
                _ => self.value(child),
            },
            "source"
                if matches!(self.dialect, Dialect::Messages)
                    && matches!(child.get("type"), Some(Value::String(_))) =>
            {
                match child.get("type").and_then(Value::as_str) {
                    Some("base64" | "text" | "content" | "file") => self.value(child),
                    Some("url") => {
                        self.path.push(Step::Key("type"));
                        self.refuse(
                            "an image or document the provider fetches from a URL (source type \
                             url)",
                        )
                    }
                    Some(other) => {
                        self.path.push(Step::Key("type"));
                        self.refuse(format!(
                            "a content source of type {}, which the check does not know",
                            shown(other)
                        ))
                    }
                    None => Ok(()),
                }
            }
            "model" => match child.as_str().and_then(web_model) {
                Some(why) => self.refuse(format!(
                    "model {}: {why}",
                    shown(child.as_str().unwrap_or_default())
                )),
                None => self.value(child),
            },
            "models" => {
                if let Value::Array(models) = child {
                    for (index, model) in models.iter().enumerate() {
                        if let Some(why) = model.as_str().and_then(web_model) {
                            self.path.push(Step::Index(index));
                            return self.refuse(format!(
                                "model {}: {why}",
                                shown(model.as_str().unwrap_or_default())
                            ));
                        }
                    }
                }
                self.value(child)
            }
            "prompt" if child.get("id").is_some() => self.refuse(
                "a stored prompt (prompt with an id), which can bring tools and inputs the \
                 request does not show",
            ),
            _ => self.value(child),
        }
    }

    fn plugins(&mut self, plugins: &'a Value) -> Result<(), Refused> {
        match plugins {
            Value::Null => Ok(()),
            Value::Array(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    if entry.get("enabled") == Some(&Value::Bool(false)) {
                        continue;
                    }
                    self.path.push(Step::Index(index));
                    let id = entry
                        .get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| "(no id)".to_string(), shown);
                    return self.refuse(format!(
                        "OpenRouter plugin {id}, which the provider runs itself (only \
                         enabled: false passes)"
                    ));
                }
                Ok(())
            }
            _ => self.refuse("plugins in a form the check does not read"),
        }
    }

    fn tools(&mut self, tools: &'a Value) -> Result<(), Refused> {
        let Value::Array(entries) = tools else {
            return match tools {
                Value::Null => Ok(()),
                _ => self.refuse("tools in a form the check does not read"),
            };
        };
        for (index, tool) in entries.iter().enumerate() {
            self.path.push(Step::Index(index));
            if let Err(construct) = self.tool(tool) {
                return self.refuse(construct);
            }
            self.path.pop();
        }
        Ok(())
    }

    /// Whether `tool` is one the client runs, by its type.
    fn tool(&self, tool: &Value) -> Result<(), String> {
        let Value::Object(fields) = tool else {
            return Err("a tool that is not an object".into());
        };
        let kind = match tool.get("type") {
            None => None,
            Some(Value::String(kind)) => Some(kind.as_str()),
            Some(_) => return Err("a tool whose type is not a string".into()),
        };
        let runs_itself =
            |kind: &str| format!("tool type {}, which the provider runs itself", shown(kind));
        match (self.dialect, kind) {
            (Dialect::Messages, None | Some("custom")) => Ok(()),
            (Dialect::Messages, Some(kind)) if anthropic_client_type(kind) => Ok(()),
            (Dialect::Responses, Some("function" | "custom" | "local_shell" | "apply_patch")) => {
                Ok(())
            }
            // The local shell tool; one with an environment may run where
            // the provider hosts it.
            (Dialect::Responses, Some("shell")) => match tool.get("environment") {
                None | Some(Value::Null) => Ok(()),
                Some(environment)
                    if environment.get("type").and_then(Value::as_str) == Some("local") =>
                {
                    Ok(())
                }
                Some(_) => Err(
                    "tool type shell with an environment the provider may host, which the \
                     check does not allow"
                        .into(),
                ),
            },
            (Dialect::Responses, Some("tool_search")) => {
                if tool.get("execution").and_then(Value::as_str) == Some("client") {
                    Ok(())
                } else {
                    Err(
                        "tool type tool_search without execution: client, which the provider \
                         runs itself"
                            .into(),
                    )
                }
            }
            (Dialect::ChatCompletions, Some("function" | "custom")) => Ok(()),
            (Dialect::GenerateContent, None) => {
                match fields.iter().find(|(name, _)| {
                    !matches!(
                        name.as_str(),
                        "functionDeclarations" | "function_declarations"
                    )
                }) {
                    None => Ok(()),
                    Some((name, _)) => Err(format!(
                        "tool {}, which the provider runs itself",
                        shown(name)
                    )),
                }
            }
            (_, Some(kind)) => Err(runs_itself(kind)),
            (_, None) => Err(
                "a tool without a type, which this request format does not run on the client"
                    .into(),
            ),
        }
    }
}

#[cfg(test)]
#[path = "provider_fetch_tests.rs"]
mod tests;
