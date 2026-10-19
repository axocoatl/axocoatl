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
//!   `apply_patch`, `shell` without a hosted environment, `tool_search`
//!   with `execution: "client"`, and a `namespace` whose tools all pass;
//!   Chat Completions `function` and `custom`;
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
//! - xAI Live Search (`search_parameters` with a `mode` other than `off`),
//!   audio the provider fetches (`input_audio` holding a URL), and Vercel AI
//!   Gateway `providerOptions` for anything but the gateway itself, which it
//!   passes through to the provider.
//! - Any body to an `agents` or `conversations` endpoint (Mistral's run the
//!   tools and connectors an agent stored with Mistral carries).
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
//!
//! The route to Ollama on this computer (`sandbox.egress.host_ollama`) is
//! checked by [`check_ollama`] instead: Ollama's own endpoints, its
//! compatible ones by their formats, and a cloud model in `model` or in the
//! path of `GET /v1/models/{model}`.
//!
//! The AI SDK's own protocol to the Vercel AI Gateway ([`ai_sdk_gateway`]),
//! what `gateway()` of the `ai` package sends, is not one of the formats:
//! the broker refuses a body in it with its own reason
//! ([`AI_SDK_GATEWAY_REFUSED`]) and names the gateway's OpenAI-compatible
//! API instead.

use super::strict_json::{self, Value};

/// Largest request body a checking route reads; a larger one is refused
/// with `413`.
pub const MAX_CHECKED_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Deepest nesting of arrays and objects the check reads.
pub const MAX_CHECKED_DEPTH: usize = 512;

/// The Vercel AI Gateway's host.
pub const VERCEL_AI_GATEWAY_HOST: &str = "ai-gateway.vercel.sh";

/// The reason a request in the AI SDK's own gateway protocol is refused
/// with ([`ai_sdk_gateway`]).
pub const AI_SDK_GATEWAY_REFUSED: &str = "ai_sdk_gateway_refused";

/// Whether a request to `path` (without its query) on `host` is in the AI
/// SDK's own protocol to the Vercel AI Gateway: `/v<N>/ai/…` on
/// `ai-gateway.vercel.sh`. `gateway()` of the `ai` package
/// (`@ai-sdk/gateway`), and a plain model id string in the AI SDK, send
/// `POST /v4/ai/language-model` with `@ai-sdk/gateway` 4.0.112 and `ai`
/// 7.0.139, and `e2e init` writes that client into `e2e.config.ts` by
/// default (e2e 0.18.0). Vercel documents the gateway's Chat Completions,
/// Responses and Anthropic Messages APIs, not this protocol, whose version
/// and body format change between releases of the package, so the check
/// does not read it: such a body would otherwise be refused as a format the
/// check does not know, and the broker refuses it naming the gateway's
/// OpenAI-compatible API instead (`Route::ai_sdk_gateway_hint`).
pub fn ai_sdk_gateway(host: &str, path: &str) -> bool {
    if host != VERCEL_AI_GATEWAY_HOST {
        return false;
    }
    let mut segments = path.strip_prefix('/').unwrap_or(path).split('/');
    let version = segments.next().unwrap_or_default();
    let numbered = version.strip_prefix('v').is_some_and(|number| {
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    });
    numbered && segments.next() == Some("ai")
}

/// Whether `body` holds nothing but JSON whitespace: the check passes it.
pub fn blank(body: &[u8]) -> bool {
    body.iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
}

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
    /// Ollama's own API: `/api/chat`, `/api/generate`, `/api/embed`,
    /// `/api/embeddings` and `/api/show`, on the route to Ollama on this
    /// computer only ([`check_ollama`]).
    OllamaNative,
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
    if blank(body) {
        return Ok(());
    }
    // Mistral's Agents and Conversations APIs run its built-in tools
    // (`web_search`, `web_search_premium`, `code_interpreter`,
    // `image_generation`, `document_library`) and connectors, also ones an
    // agent stored with Mistral carries, which a request names only by its
    // `agent_id` (https://docs.mistral.ai/studio-api/connectors/conversations).
    // `/v1/agents/completions` ends like a Chat Completions path, so these
    // endpoints are refused by name, on any host.
    if path
        .split('/')
        .any(|segment| matches!(segment, "agents" | "conversations"))
    {
        return Err(Refused {
            construct: "a request to an agents or conversations endpoint, whose agent can run \
                        tools the provider runs itself (web search, code execution, connectors), \
                        stored with the provider where the body does not show them"
                .into(),
            at: "$".into(),
        });
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
    check_parsed(dialect, false, body)
}

/// Check a request sent to `path` on the route to Ollama on this computer
/// (`sandbox.egress.host_ollama`), by its body and, for the one allowed path
/// that names a model (`GET`/`HEAD /v1/models/{model}`), by its path. An
/// empty body passes.
///
/// Ollama (0.20.6 and 0.40.6 alike) with its cloud features on (the default)
/// sends a request for a cloud model to `ollama.com`, prompt and all,
/// without a pull first: on `/api/chat`, `/api/generate`, `/api/embed`,
/// `/api/show` and its OpenAI- and Anthropic-compatible endpoints, for any
/// model whose tag (after the last `:` past the last `/`), trimmed and in any
/// case, is `cloud` or ends in `-cloud` (`gpt-oss:120b-cloud`,
/// `kimi-k2.5:cloud`), whatever host or namespace the name has (seen by
/// running the 0.20.6 binary with its proxy pointed at a recorder;
/// `internal/modelref` is unchanged in 0.40.6). Such a `model` is refused
/// anywhere in the body. `GET /v1/models/{model}` has no body: Ollama 0.40.6
/// proxies it to ollama.com for a cloud name (`server/cloud_proxy.go`
/// `cloudModelPathPassthroughMiddleware`), so a cloud name in the path,
/// percent-decoded as Ollama decodes it, is refused too. The compatible
/// endpoints get the checks of their formats besides; Ollama itself refuses
/// image and file URLs there. A name with another registry host does not
/// make an inference request fetch it (that takes `/api/pull` or
/// `/api/create`, which the route does not allow). A local copy of a cloud
/// model under a plain name is not seen here: the daemon opens the route
/// only to an Ollama that reports its cloud features off.
pub fn check_ollama(path: &str, body: &[u8]) -> Result<(), Refused> {
    if let Some(name) = path.strip_prefix("/v1/models/") {
        // Decoded once, as Ollama's router does, and once more, so that an
        // escape of an escape is refused as well.
        let once = percent_decoded(name);
        let twice = percent_decoded(&once);
        let cloud = [name, once.as_str(), twice.as_str()]
            .into_iter()
            .find(|model| ollama_cloud_model(model))
            .map(|model| shown(model.trim()));
        if let Some(model) = cloud {
            return Err(Refused {
                construct: format!("model {model}: a cloud model, which Ollama runs on ollama.com"),
                at: "path".into(),
            });
        }
    }
    if blank(body) {
        return Ok(());
    }
    let dialect = match path {
        "/api/chat" | "/api/generate" | "/api/embed" | "/api/embeddings" | "/api/show" => {
            Dialect::OllamaNative
        }
        "/v1/chat/completions" | "/v1/completions" | "/v1/embeddings" => Dialect::ChatCompletions,
        "/v1/responses" => Dialect::Responses,
        "/v1/messages" => Dialect::Messages,
        _ => {
            return Err(Refused {
                construct: "a request body to an Ollama endpoint the check does not know".into(),
                at: "$".into(),
            })
        }
    };
    check_parsed(dialect, true, body)
}

/// `text` with each `%XX` escape decoded; invalid UTF-8 is replaced.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if let Some(byte) = bytes
                .get(index + 1..index + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn check_parsed(dialect: Dialect, ollama: bool, body: &[u8]) -> Result<(), Refused> {
    let value = strict_json::parse(body, MAX_CHECKED_DEPTH).map_err(|error| Refused {
        construct: format!(
            "a body the check cannot read as one JSON value ({error}), which the provider might \
             read otherwise"
        ),
        at: "$".into(),
    })?;
    let mut walk = Walk {
        dialect,
        ollama,
        path: Vec::new(),
    };
    walk.value(&value)
}

/// Whether Ollama (0.20.6 and 0.40.6 alike) runs `model` on `ollama.com`:
/// its tag, trimmed and in any case, is `cloud` or ends in `-cloud`.
pub fn ollama_cloud_model(model: &str) -> bool {
    let name = model.trim();
    let name = name.split('@').next().unwrap_or(name);
    let last = name.rsplit('/').next().unwrap_or(name);
    last.rsplit_once(':').is_some_and(|(_, tag)| {
        let tag = tag.trim().to_ascii_lowercase();
        tag == "cloud" || tag.ends_with("-cloud")
    })
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
    } else if ["exa/", "parallel/", "tako/", "browserbase/"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        // The search and fetch providers the Vercel AI Gateway lists
        // (https://vercel.com/docs/ai-gateway/models-and-providers/web-search).
        Some("a model of a search or fetch provider, which reaches the web")
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
    /// The route to Ollama on this computer: `model` is checked for a cloud
    /// model instead of a web-searching one.
    ollama: bool,
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
            // Ollama's structured output: `"json"` or a JSON schema.
            "format" if self.dialect == Dialect::OllamaNative => Ok(()),
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
            // xAI Live Search, on Chat Completions and Responses: `mode` is
            // `off`, `on` or `auto`, and `on` when unset
            // (https://docs.x.ai/developers/rest-api-reference/inference/chat-completions,
            // https://docs.x.ai/docs/api-reference).
            "search_parameters" => match child {
                Value::Null => Ok(()),
                Value::Object(_) if child.get("mode").and_then(Value::as_str) == Some("off") => {
                    Ok(())
                }
                _ => self.refuse(
                    "live search by the provider (search_parameters with a mode other than off)",
                ),
            },
            // Audio the provider fetches: Mistral documents a public URL in
            // `input_audio.data`; base64 audio has no URL scheme.
            "input_audio" => {
                let remote = |value: &Value| {
                    value
                        .as_str()
                        .is_some_and(|data| has_scheme(data) && !is_data_url(data))
                };
                if remote(child)
                    || ["data", "url"]
                        .iter()
                        .any(|name| child.get(name).is_some_and(remote))
                {
                    self.refuse("audio the provider fetches from a URL (input_audio)")
                } else {
                    self.value(child)
                }
            }
            // The Vercel AI Gateway passes provider-specific options
            // (`providerOptions.anthropic`, `.openai`, …) through to the
            // provider, where some turn on its own search or grounding
            // (https://vercel.com/docs/ai-gateway/models-and-providers/provider-options):
            // only the gateway's own routing options pass.
            "providerOptions" => match child {
                Value::Null => Ok(()),
                Value::Object(entries) => {
                    for (name, options) in entries {
                        self.path.push(Step::Key(name));
                        if name != "gateway" {
                            return self.refuse(format!(
                                "provider options passed through to {} (providerOptions), which \
                                 can turn on its own search or fetch",
                                shown(name)
                            ));
                        }
                        self.value(options)?;
                        self.path.pop();
                    }
                    Ok(())
                }
                _ => self.refuse("providerOptions in a form the check does not read"),
            },
            // Mistral fetches a public `document_url` and `image_url`
            // (https://docs.mistral.ai/capabilities/OCR/document_qna,
            // https://docs.mistral.ai/capabilities/vision/), as OpenAI,
            // OpenRouter and xAI fetch image URLs.
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
            "model" if self.ollama => match child.as_str() {
                Some(model) if ollama_cloud_model(model) => self.refuse(format!(
                    "model {}: a cloud model, which Ollama runs on ollama.com",
                    shown(model.trim())
                )),
                _ => self.value(child),
            },
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
            if self.dialect == Dialect::Responses
                && tool.get("type").and_then(Value::as_str) == Some("namespace")
            {
                self.namespace(tool)?;
            } else if let Err(construct) = self.tool(tool) {
                return self.refuse(construct);
            }
            self.path.pop();
        }
        Ok(())
    }

    /// A Responses `namespace`: a named group of tools whose functions
    /// OpenAI's tool search loads; the provider runs nothing for the
    /// namespace itself (https://developers.openai.com/api/docs/guides/tools-tool-search).
    /// Codex 0.160.1 offers its tools this way, in an `additional_tools`
    /// input item, for the models its catalog marks `code_mode_only`
    /// (`gpt-6.1-sol`, `gpt-6-luna`). It passes when every tool in it does.
    fn namespace(&mut self, namespace: &'a Value) -> Result<(), Refused> {
        match namespace.get("tools") {
            Some(tools @ Value::Array(_)) => {
                self.path.push(Step::Key("tools"));
                self.tools(tools)?;
                self.path.pop();
                Ok(())
            }
            _ => self.refuse("a namespace without a list of tools, which the check does not read"),
        }
    }

    /// Whether `tool` is one the client runs, by its type. Every type a
    /// provider runs itself fails this, among them xAI's `web_search`,
    /// `x_search`, `code_interpreter` and `code_execution`
    /// (https://docs.x.ai/docs/guides/tools/overview), Mistral's
    /// `image_generation`, `document_library` and `connector` on Chat
    /// Completions (https://docs.mistral.ai/api/) and the Vercel AI Gateway's
    /// `vercel:perplexity_search`, `vercel:exa_search`,
    /// `vercel:parallel_search`, `vercel:tako_search`,
    /// `vercel:browserbase_search` and `vercel:browserbase_fetch`
    /// (https://vercel.com/docs/ai-gateway/models-and-providers/web-search).
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
            (Dialect::OllamaNative, Some("function")) => Ok(()),
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
