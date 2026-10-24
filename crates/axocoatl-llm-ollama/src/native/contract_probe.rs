//! Contract probes: cheap requests on the person's own server and model that
//! check the guarantees native execution relies on, instead of trusting the
//! audited source alone.
//!
//! - Over-long refusal: one user message of about four times the observed
//!   context in bytes, sent with `truncate:false`, `shift:false` and the
//!   observed `num_ctx`, must be refused (a status that is not 2xx and an
//!   `{"error"}` body) without any generation record.
//! - Output stop: a short prompt with `num_predict:1` must finish
//!   (`done:true`, `done_reason` `stop` or `length`) after at most one
//!   generated token, with a prompt count between zero and the context.
//! - Long output: a prompt that asks for hundreds of lines, with
//!   `num_predict` [`LONG_OUTPUT_TOKENS`], must finish after at most that
//!   many generated tokens, report exactly that many when the limit ended
//!   it (`length`), and never stream more pieces of text than the tokens it
//!   reports. A runner that decodes speculatively, accepting several drafted
//!   tokens at a time, is where a bound could be overshot or a count could
//!   fall short.
//! - Tool call, for a model that reports tool calling: asked to call the one
//!   tool it is offered, within [`TOOL_CALL_TOKENS`], the answer is read the
//!   way inference reads one ([`super::NativeResponse`]) and must call that
//!   tool, with a call id and a string argument. A call to a tool it was not
//!   offered next to that one is no failure: inference answers such a call
//!   with a tool error and lets the model go on. This holds whichever
//!   template Ollama renders the model with (its own renderer, a Go
//!   template, the model's chat template in llama-server, or Harmony).
//!
//! A probe Ollama does not answer (a request that fails, an error status,
//! an answer that ends before its final record) is not a failed guarantee:
//! the error says what Ollama did, and nothing is kept, so the next try
//! probes again.
//!
//! All send the same `num_ctx` and `shift:false` as inference (and as the
//! observation's load request), so none makes Ollama reload the model, and
//! the same [`PROBE_SEED`] at the model's own sampling settings, so a probe
//! gives the same answer each time it runs on a server and model.
//! A pass is kept for this process per server, version, model digest,
//! context and tool calling; a new Ollama version is probed again before it
//! serves. A failed probe refuses the model on that server, for every Agent
//! and run that would use it.
//!
//! Images are not probed. Every call reserves the model's whole context
//! plus its output limit, whatever its prompt holds, and llama-server
//! counts an image's tokens in the prompt it checks against the context:
//! on Ollama 0.40.8, with `qwen3.5:0.8b` loaded at a 4,096-token context, a
//! 64x64 image counted 11 prompt tokens, 4,090 tokens of text were
//! generated from, and the same text with the image (4,101 tokens) was
//! refused under `truncate:false`. An image therefore never takes a call past
//! what it reserves, and the output limit is probed above.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axocoatl_llm::{transport::validated_endpoint, ChatRequest, ProviderError, ToolDefinition};
use serde_json::{json, Value};

use super::{invalid, NativeResponse, PROVIDER};

/// Longest probe answer read. A refusal or a one-token answer is far smaller,
/// and a bounded long or tool-call answer is a few kilobytes.
const MAX_PROBE_RESPONSE_BYTES: usize = 256 * 1024;

/// The stop probe's prompt: any short prompt works; this one invites a
/// one-word answer, so a stop before the limit is also a pass.
pub const STOP_PROBE_PROMPT: &str = "Reply with the single word: ready";

/// The long-output probe's prompt: an answer far longer than
/// [`LONG_OUTPUT_TOKENS`], so the limit, not the model, ends it.
pub const LONG_OUTPUT_PROBE_PROMPT: &str =
    "Write the numbers from 1 to 300, one per line. Write only the numbers.";

/// The long-output probe's `num_predict`.
pub const LONG_OUTPUT_TOKENS: usize = 64;

/// The tool-call probe's prompt.
pub const TOOL_PROBE_PROMPT: &str = "Call the report_ready tool with word set to ready.";

/// The one tool the tool-call probe offers.
pub const PROBE_TOOL: &str = "report_ready";

/// The tool-call probe's `num_predict`: room for a thinking model's
/// reasoning before its call, still a bounded cost. On Ollama 0.40.8, at
/// each model's own sampling settings, the call came after 23 tokens for
/// qwen3-coder:30b, 34 to 44 for gpt-oss:20b, 82 to 113 for qwen3:0.6b, 96
/// to 167 for qwen3.5:0.8b and 195 to 309 for qwen3:4b.
pub const TOOL_CALL_TOKENS: usize = 1024;

/// The seed every probe samples with. With it, Ollama 0.40.8 gave the same
/// answer to the tool-call probe five times out of five for each model
/// measured, at that model's own temperature (the one inference uses unless
/// an Agent sets its own), so a pass or a failure is the same when the probe
/// runs again.
pub const PROBE_SEED: u64 = 42;

/// Which contract probe a `/api/chat` request is, and what Ollama 0.40
/// answers to it, for test servers that stand in for Ollama. Tests only
/// (the `contract-probe-stand-in` feature).
#[cfg(any(test, feature = "contract-probe-stand-in"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractProbe {
    /// The over-long prompt (`"x "` repeated): refused.
    OverLong,
    /// The one-token request ([`STOP_PROBE_PROMPT`]).
    Stop,
    /// The bounded long answer ([`LONG_OUTPUT_PROBE_PROMPT`]).
    LongOutput,
    /// The request to call [`PROBE_TOOL`] ([`TOOL_PROBE_PROMPT`]).
    ToolCall,
}

#[cfg(any(test, feature = "contract-probe-stand-in"))]
impl ContractProbe {
    /// The probe a parsed `/api/chat` body is, or `None` for any other request.
    pub fn of(body: &Value) -> Option<Self> {
        if body.get("truncate") != Some(&Value::Bool(false)) {
            return None;
        }
        match body.pointer("/messages/0/content")?.as_str()? {
            STOP_PROBE_PROMPT => Some(Self::Stop),
            LONG_OUTPUT_PROBE_PROMPT => Some(Self::LongOutput),
            TOOL_PROBE_PROMPT => Some(Self::ToolCall),
            content if content.starts_with("x x ") => Some(Self::OverLong),
            _ => None,
        }
    }

    /// The HTTP status, content type and body Ollama 0.40 answers to this
    /// probe `body` with: the shapes Ollama 0.40.8 gave (a refusal, one token,
    /// the limit reached, and the call in the final record, as gpt-oss:20b
    /// answers).
    pub fn answer(self, body: &Value) -> (u16, &'static str, String) {
        let model = &body["model"];
        let records = match self {
            Self::OverLong => {
                return (
                    400,
                    "application/json",
                    json!({"error": "request exceeds the available context size, try \
                                     increasing it"})
                    .to_string(),
                )
            }
            Self::Stop => vec![json!({"model": model, "created_at": "2026-10-19T00:00:00Z",
                "message": {"role": "assistant", "content": "ready"}, "done": true,
                "done_reason": "length", "prompt_eval_count": 14, "eval_count": 1})],
            Self::LongOutput => vec![
                json!({"model": model, "created_at": "2026-10-19T00:00:00Z",
                    "message": {"role": "assistant", "content": "1\n2\n3\n"}, "done": false}),
                json!({"model": model, "created_at": "2026-10-19T00:00:00Z",
                    "message": {"role": "assistant", "content": ""}, "done": true,
                    "done_reason": "length", "prompt_eval_count": 33,
                    "eval_count": body.pointer("/options/num_predict")}),
            ],
            Self::ToolCall => vec![json!({"model": model, "created_at": "2026-10-19T00:00:00Z",
                "message": {"role": "assistant", "content": "", "tool_calls": [{
                    "id": "call_probe", "function": {"index": 0, "name": PROBE_TOOL,
                    "arguments": {"word": "ready"}}}]},
                "done": true, "done_reason": "stop", "prompt_eval_count": 138,
                "eval_count": 31})],
        };
        let mut ndjson = String::new();
        for record in records {
            ndjson.push_str(&record.to_string());
            ndjson.push('\n');
        }
        (200, "application/x-ndjson", ndjson)
    }
}

/// What a passed probe set is valid for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProbeKey {
    pub(crate) base_url: String,
    pub(crate) server_version: String,
    pub(crate) model_digest: String,
    pub(crate) context_tokens: usize,
    /// The model reports tool calling, so the tool-call probe runs.
    pub(crate) tools: bool,
}

fn passed() -> &'static Mutex<HashSet<ProbeKey>> {
    static PASSED: OnceLock<Mutex<HashSet<ProbeKey>>> = OnceLock::new();
    PASSED.get_or_init(Default::default)
}

fn has_passed(key: &ProbeKey) -> bool {
    passed()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .contains(key)
}

/// Remember that `key` passed, for a retained observation whose probes passed
/// when it was made on this same server version.
pub(crate) fn remember_passed(key: ProbeKey) {
    passed()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(key);
}

/// The guarantee a failed probe showed does not hold, by name.
fn failed(guarantee: &str, model: &str, version: &str, detail: impl AsRef<str>) -> ProviderError {
    invalid(format!(
        "Ollama {version} with {model} failed the {guarantee} check, so native Sessions refuse \
         this model on this server: {}",
        detail.as_ref()
    ))
}

/// How Ollama left a probe unanswered.
enum Unanswered {
    /// The request was not sent, or its answer not read, in time or at all.
    Request(String),
    /// A status that is not 2xx, or one that does not refuse the over-long
    /// prompt for its length, with Ollama's message.
    Status(u16, String),
    /// The answer ended before its final record, with Ollama's message if
    /// it sent one.
    Unfinished(Option<String>),
}

/// A probe Ollama did not answer: the request failed, Ollama answered with
/// an error status, or its answer ended before its final record (a model
/// runner that stopped, out of memory for one; on Ollama 0.40.8 a request to
/// a runner that had stopped got an answer without one). That shows nothing
/// about the guarantee, so the model is not refused for it: the error says
/// what Ollama did, and the probes run again on the next try.
fn unanswered(guarantee: &str, model: &str, version: &str, how: Unanswered) -> ProviderError {
    let check = format!("the {guarantee} check of {model} on Ollama {version}");
    match how {
        Unanswered::Request(error) => ProviderError::Network(format!(
            "{error}, so {check} did not run; try again once Ollama answers"
        )),
        Unanswered::Status(status, message) => ProviderError::ApiError {
            provider: PROVIDER.into(),
            status,
            message: format!("{message}; {check} did not run: try again once Ollama answers"),
        },
        Unanswered::Unfinished(message) => ProviderError::IncompleteStream {
            provider: PROVIDER.into(),
            message: format!(
                "Ollama ended its answer in {check} before its final record{}, so the check did \
                 not run; Ollama's log says why (a model runner that stopped, for one): try again",
                message.map(|text| format!(" ({text})")).unwrap_or_default()
            ),
        },
    }
}

/// Why a probe's request or answer failed: Ollama did not answer it, or
/// answered with something that is not a bounded NDJSON answer.
enum PostError {
    Unanswered(String),
    Invalid(String),
}

impl PostError {
    fn into_error(self, guarantee: &str, model: &str, version: &str) -> ProviderError {
        match self {
            Self::Unanswered(error) => {
                unanswered(guarantee, model, version, Unanswered::Request(error))
            }
            Self::Invalid(detail) => failed(guarantee, model, version, detail),
        }
    }
}

const OVER_LONG: &str = "over-long prompt refusal (truncate:false)";
const STOP: &str = "output limit (num_predict)";
const LONG_OUTPUT: &str = "long output limit and count (num_predict, eval_count)";
const TOOL_CALL: &str = "tool call";

/// Run the probes unless they already passed for `key` (the tool-call probe
/// only for a model that reports tool calling), each answer within `timeout`
/// (the observation's load timeout: a model Ollama has unloaded since is
/// loaded again first).
pub(crate) async fn ensure_contract(
    client: &reqwest::Client,
    key: ProbeKey,
    model: &str,
    timeout: Duration,
) -> Result<(), ProviderError> {
    if has_passed(&key) {
        return Ok(());
    }
    over_long_probe(client, &key, model, timeout).await?;
    stop_probe(client, &key, model, timeout).await?;
    long_output_probe(client, &key, model, timeout).await?;
    if key.tools {
        tool_call_probe(client, &key, model, timeout).await?;
    }
    tracing::info!(
        model,
        server_version = %key.server_version,
        context = key.context_tokens,
        tools = key.tools,
        "native Ollama contract probes passed"
    );
    remember_passed(key);
    Ok(())
}

fn probe_body(model: &str, content: &str, context: usize, num_predict: usize) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "stream": true,
        "truncate": false,
        "shift": false,
        "options": {"num_ctx": context, "num_predict": num_predict, "seed": PROBE_SEED},
    })
}

/// Post a probe and read its status and NDJSON (or JSON) records, bounded.
async fn post(
    client: &reqwest::Client,
    base_url: &str,
    body: &Value,
    timeout: Duration,
) -> Result<(u16, Vec<Value>), PostError> {
    let url = validated_endpoint(base_url, "api/chat", PROVIDER)
        .map_err(|e| PostError::Invalid(e.to_string()))?;
    let mut response = client
        .post(url)
        .json(body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| PostError::Unanswered(format!("the probe request failed: {error}")))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        PostError::Unanswered(format!("the probe answer could not be read: {error}"))
    })? {
        if chunk.len() > MAX_PROBE_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(PostError::Invalid(format!(
                "the probe answer is longer than {MAX_PROBE_RESPONSE_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut records = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = serde_json::from_slice(line).map_err(|_| {
            PostError::Invalid(format!(
                "Ollama answered {status} with a body that is not JSON"
            ))
        })?;
        records.push(value);
    }
    Ok((status, records))
}

fn error_text(records: &[Value]) -> Option<String> {
    records.iter().find_map(|record| {
        record
            .get("error")
            .and_then(Value::as_str)
            .map(|text| text.chars().take(200).collect())
    })
}

async fn over_long_probe(
    client: &reqwest::Client,
    key: &ProbeKey,
    model: &str,
    timeout: Duration,
) -> Result<(), ProviderError> {
    let version = &key.server_version;
    // "x " is at least one token per two bytes in every tokenizer seen, so
    // this is at least twice the context in tokens.
    let content = "x ".repeat(key.context_tokens.saturating_mul(2));
    let (status, records) = post(
        client,
        &key.base_url,
        &probe_body(model, &content, key.context_tokens, 1),
        timeout,
    )
    .await
    .map_err(|error| error.into_error(OVER_LONG, model, version))?;
    if records
        .iter()
        .any(|record| record.get("eval_count").is_some())
    {
        return Err(failed(
            OVER_LONG,
            model,
            version,
            format!(
                "a prompt of about {} tokens, more than the {}-token context, was generated from \
                 instead of refused",
                key.context_tokens * 2,
                key.context_tokens
            ),
        ));
    }
    if (200..300).contains(&status) {
        return Err(failed(
            OVER_LONG,
            model,
            version,
            format!(
                "a prompt longer than the {}-token context was accepted (status {status}) instead \
                 of refused",
                key.context_tokens
            ),
        ));
    }
    match error_text(&records) {
        // Ollama answered with a refusal, but not one of the prompt's length:
        // the probe proves nothing about the guarantee.
        Some(message) if matches!(status, 404 | 429 | 499 | 503) => Err(unanswered(
            OVER_LONG,
            model,
            version,
            Unanswered::Status(
                status,
                format!(
                    "{message}, which is not a refusal of the prompt's length (try again when \
                     the server is idle)"
                ),
            ),
        )),
        Some(_) => Ok(()),
        None => Err(failed(
            OVER_LONG,
            model,
            version,
            format!("Ollama answered {status} without an error message"),
        )),
    }
}

/// Whether Ollama answered a probe that should have been generated: a 2xx
/// status and a final record. An error status, or an answer that ends
/// before its final record, leaves the probe [`unanswered`]. An error record
/// inside the answer does too, unless `error_fails` (the tool-call probe:
/// Ollama ends an answer with one when it cannot parse the model's call),
/// in which case the guarantee failed. `what` names the request.
fn answered(
    status: u16,
    records: &[Value],
    (guarantee, model, version): (&str, &str, &str),
    what: &str,
    error_fails: bool,
) -> Result<(), ProviderError> {
    let error = error_text(records);
    if !(200..300).contains(&status) {
        let message = error.unwrap_or_else(|| "Ollama gave no reason".into());
        return Err(unanswered(
            guarantee,
            model,
            version,
            Unanswered::Status(status, message),
        ));
    }
    let done = records
        .iter()
        .any(|record| record.get("done").and_then(Value::as_bool) == Some(true));
    match error {
        Some(message) if error_fails => Err(failed(
            guarantee,
            model,
            version,
            format!("{what} ended with an error: {message}"),
        )),
        Some(message) => Err(unanswered(
            guarantee,
            model,
            version,
            Unanswered::Unfinished(Some(message)),
        )),
        None if !done => Err(unanswered(
            guarantee,
            model,
            version,
            Unanswered::Unfinished(None),
        )),
        None => Ok(()),
    }
}

/// The one `done` record a generated probe answer ends with, its
/// `done_reason` (`stop` or `length`) and the generated-token count it
/// reports (Ollama omits a zero count). `what` names the request.
fn finished<'a>(records: &'a [Value], what: &str) -> Result<(&'a Value, &'a str, u64), String> {
    let done: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("done").and_then(Value::as_bool) == Some(true))
        .collect();
    let [last] = done.as_slice() else {
        return Err(format!(
            "{what} ended with {} final records instead of one",
            done.len()
        ));
    };
    if !records
        .last()
        .is_some_and(|record| std::ptr::eq(record, *last))
    {
        return Err(format!("{what} went on after its final record"));
    }
    let reason = match last.get("done_reason").and_then(Value::as_str) {
        Some(reason @ ("stop" | "length")) => reason,
        reason => return Err(format!("{what} finished with done_reason {reason:?}")),
    };
    let generated = match last.get("eval_count") {
        None => 0,
        Some(count) => count
            .as_u64()
            .ok_or_else(|| format!("{what} reported a malformed eval_count"))?,
    };
    Ok((last, reason, generated))
}

/// The prompt count `last` reports is between zero and the context.
fn prompt_counted(last: &Value, key: &ProbeKey, what: &str) -> Result<(), String> {
    let prompt = last.get("prompt_eval_count").and_then(Value::as_u64);
    match prompt {
        Some(prompt) if prompt > 0 && prompt < key.context_tokens as u64 => Ok(()),
        _ => Err(format!(
            "{what} reported a prompt count of {prompt:?}, not between 0 and the {}-token context",
            key.context_tokens
        )),
    }
}

async fn stop_probe(
    client: &reqwest::Client,
    key: &ProbeKey,
    model: &str,
    timeout: Duration,
) -> Result<(), ProviderError> {
    const WHAT: &str = "a one-token request";
    let fail = |detail: String| failed(STOP, model, &key.server_version, detail);
    let (status, records) = post(
        client,
        &key.base_url,
        &probe_body(model, STOP_PROBE_PROMPT, key.context_tokens, 1),
        timeout,
    )
    .await
    .map_err(|error| error.into_error(STOP, model, &key.server_version))?;
    answered(
        status,
        &records,
        (STOP, model, &key.server_version),
        WHAT,
        false,
    )?;
    let (last, _, generated) = finished(&records, WHAT).map_err(fail)?;
    if generated > 1 {
        return Err(fail(format!(
            "asked for at most 1 token (num_predict:1), the model generated {generated}; \
             Axocoatl cannot bound this model's output"
        )));
    }
    prompt_counted(last, key, WHAT).map_err(fail)
}

async fn long_output_probe(
    client: &reqwest::Client,
    key: &ProbeKey,
    model: &str,
    timeout: Duration,
) -> Result<(), ProviderError> {
    const WHAT: &str = "a long answer";
    let fail = |detail: String| failed(LONG_OUTPUT, model, &key.server_version, detail);
    let limit = LONG_OUTPUT_TOKENS as u64;
    let (status, records) = post(
        client,
        &key.base_url,
        &probe_body(
            model,
            LONG_OUTPUT_PROBE_PROMPT,
            key.context_tokens,
            LONG_OUTPUT_TOKENS,
        ),
        timeout,
    )
    .await
    .map_err(|error| error.into_error(LONG_OUTPUT, model, &key.server_version))?;
    answered(
        status,
        &records,
        (LONG_OUTPUT, model, &key.server_version),
        WHAT,
        false,
    )?;
    let (last, reason, generated) = finished(&records, WHAT).map_err(fail)?;
    if generated > limit {
        return Err(fail(format!(
            "asked for at most {limit} tokens (num_predict:{limit}), the model generated \
             {generated}; Axocoatl cannot bound this model's output"
        )));
    }
    if reason == "length" && generated != limit {
        return Err(fail(format!(
            "the answer ended at its {limit}-token limit (done_reason length) but reported \
             {generated} generated tokens; Axocoatl cannot count this model's output"
        )));
    }
    // Every piece of text streamed before the final record is at least one
    // generated token, however many tokens a step accepts.
    let pieces = records[..records.len() - 1]
        .iter()
        .filter(|record| {
            ["/message/content", "/message/thinking"]
                .iter()
                .any(|field| {
                    record
                        .pointer(field)
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                })
        })
        .count() as u64;
    if pieces > generated {
        return Err(fail(format!(
            "the answer streamed {pieces} pieces of text but reported {generated} generated \
             tokens; Axocoatl cannot count this model's output"
        )));
    }
    prompt_counted(last, key, WHAT).map_err(fail)
}

/// The tool the tool-call probe offers.
fn probe_tool() -> ToolDefinition {
    ToolDefinition {
        name: PROBE_TOOL.into(),
        description: "Report the word you were asked to report.".into(),
        parameters: json!({
            "type": "object",
            "properties": {"word": {"type": "string", "description": "The word to report."}},
            "required": ["word"],
        }),
        concurrency: Default::default(),
    }
}

/// Read a probe answer the way inference reads a response: its records in
/// order, then what ends it, with any refusal of its calls.
fn read_as_inference(
    answer: &mut NativeResponse,
    records: &[Value],
    status: u16,
) -> Result<(), ProviderError> {
    for record in records {
        answer.observe_counts(record);
        answer.record(record, status)?;
    }
    if let Some((_, error)) = answer.take_rejection() {
        return Err(error);
    }
    answer.finish()?;
    match answer.take_rejection() {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

async fn tool_call_probe(
    client: &reqwest::Client,
    key: &ProbeKey,
    model: &str,
    timeout: Duration,
) -> Result<(), ProviderError> {
    const WHAT: &str = "a request to call the offered tool";
    let fail = |detail: String| failed(TOOL_CALL, model, &key.server_version, detail);
    let limit = TOOL_CALL_TOKENS as u64;
    let mut request = ChatRequest::simple(TOOL_PROBE_PROMPT);
    request.tools.push(probe_tool());
    request.max_tokens = Some(TOOL_CALL_TOKENS);
    let mut body = probe_body(
        model,
        TOOL_PROBE_PROMPT,
        key.context_tokens,
        TOOL_CALL_TOKENS,
    );
    body["tools"] = crate::tools_json(&request.tools);
    let (status, records) = post(client, &key.base_url, &body, timeout)
        .await
        .map_err(|error| error.into_error(TOOL_CALL, model, &key.server_version))?;
    answered(
        status,
        &records,
        (TOOL_CALL, model, &key.server_version),
        WHAT,
        true,
    )?;
    let (_, reason, generated) = finished(&records, WHAT).map_err(fail)?;
    if generated > limit {
        return Err(fail(format!(
            "asked for at most {limit} tokens (num_predict:{limit}), the model generated \
             {generated}; Axocoatl cannot bound this model's output"
        )));
    }
    let mut answer = NativeResponse::new(
        model.to_owned(),
        request,
        MAX_PROBE_RESPONSE_BYTES,
        (key.context_tokens + TOOL_CALL_TOKENS) as u64,
    );
    let read = read_as_inference(&mut answer, &records, status);
    if answer.calls.is_empty() {
        return Err(fail(match (reason, read) {
            ("length", _) => format!(
                "asked to call {PROBE_TOOL}, the model reached its {limit}-token limit without \
                 a tool call"
            ),
            (_, Err(error)) => format!("the answer is not a tool call Axocoatl can run: {error}"),
            (_, Ok(())) => {
                format!("asked to call {PROBE_TOOL}, the model answered without a tool call")
            }
        }));
    }
    read.map_err(|error| fail(format!("the tool call cannot run: {error}")))?;
    let called = |call: &axocoatl_llm::ToolCall| {
        fail(format!(
            "asked to call {PROBE_TOOL} with a string word, the model called {} with {}",
            super::shown_name(&call.name),
            call.arguments
                .to_string()
                .chars()
                .take(200)
                .collect::<String>()
        ))
    };
    // A call to a tool it was not offered is answered with a tool error in
    // inference, and the model goes on; the offered tool must be called, and
    // each call to it must carry its argument.
    let offered: Vec<_> = answer
        .calls
        .iter()
        .filter(|call| call.name == PROBE_TOOL)
        .collect();
    if offered.is_empty() {
        return Err(called(&answer.calls[0]));
    }
    match offered
        .into_iter()
        .find(|call| !call.arguments.get("word").is_some_and(Value::is_string))
    {
        Some(call) => Err(called(call)),
        None => Ok(()),
    }
}
