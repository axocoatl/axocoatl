//! Contract probes: two cheap requests on the person's own server and model
//! that check the guarantees native execution relies on, instead of trusting
//! the audited source alone.
//!
//! - Over-long refusal: one user message of about four times the observed
//!   context in bytes, sent with `truncate:false`, `shift:false` and the
//!   observed `num_ctx`, must be refused (a status that is not 2xx and an
//!   `{"error"}` body) without any generation record.
//! - Output stop: a short prompt with `num_predict:1` must finish
//!   (`done:true`, `done_reason` `stop` or `length`) after at most one
//!   generated token, with a prompt count between zero and the context.
//!
//! Both send the same `num_ctx` and `shift:false` as inference (and as the
//! observation's load request), so neither makes Ollama reload the model.
//! A pass is kept for this process per server, version, model digest and
//! context; a new Ollama version is probed again before it serves.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use axocoatl_llm::{
    transport::{validated_endpoint, RESPONSE_TIMEOUT},
    ProviderError,
};
use serde_json::{json, Value};

use super::{invalid, PROVIDER};

/// Longest probe answer read. A refusal or a one-token answer is far smaller.
const MAX_PROBE_RESPONSE_BYTES: usize = 256 * 1024;

/// The stop probe's prompt: any short prompt works; this one invites a
/// one-word answer, so a stop before the limit is also a pass.
pub const STOP_PROBE_PROMPT: &str = "Reply with the single word: ready";

/// What a passed probe pair is valid for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProbeKey {
    pub(crate) base_url: String,
    pub(crate) server_version: String,
    pub(crate) model_digest: String,
    pub(crate) context_tokens: usize,
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

const OVER_LONG: &str = "over-long prompt refusal (truncate:false)";
const STOP: &str = "output limit (num_predict)";

/// Run both probes unless they already passed for `key`.
pub(crate) async fn ensure_contract(
    client: &reqwest::Client,
    key: ProbeKey,
    model: &str,
) -> Result<(), ProviderError> {
    if has_passed(&key) {
        return Ok(());
    }
    over_long_probe(client, &key, model).await?;
    stop_probe(client, &key, model).await?;
    tracing::info!(
        model,
        server_version = %key.server_version,
        context = key.context_tokens,
        "native Ollama contract probes passed"
    );
    remember_passed(key);
    Ok(())
}

fn probe_body(model: &str, content: &str, context: usize) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "stream": true,
        "truncate": false,
        "shift": false,
        "options": {"num_ctx": context, "num_predict": 1},
    })
}

/// Post a probe and read its status and NDJSON (or JSON) records, bounded.
async fn post(
    client: &reqwest::Client,
    base_url: &str,
    body: &Value,
) -> Result<(u16, Vec<Value>), String> {
    let url = validated_endpoint(base_url, "api/chat", PROVIDER).map_err(|e| e.to_string())?;
    let mut response = client
        .post(url)
        .json(body)
        .timeout(RESPONSE_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("the probe request failed: {error}"))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("the probe answer could not be read: {error}"))?
    {
        if chunk.len() > MAX_PROBE_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(format!(
                "the probe answer is longer than {MAX_PROBE_RESPONSE_BYTES} bytes"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut records = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| format!("Ollama answered {status} with a body that is not JSON"))?;
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
) -> Result<(), ProviderError> {
    let version = &key.server_version;
    // "x " is at least one token per two bytes in every tokenizer seen, so
    // this is at least twice the context in tokens.
    let content = "x ".repeat(key.context_tokens.saturating_mul(2));
    let (status, records) = post(
        client,
        &key.base_url,
        &probe_body(model, &content, key.context_tokens),
    )
    .await
    .map_err(|detail| failed(OVER_LONG, model, version, detail))?;
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
        Some(_) if matches!(status, 404 | 429 | 499 | 503) => Err(failed(
            OVER_LONG,
            model,
            version,
            format!(
                "Ollama answered {status} ({}), which is not a refusal of the prompt's length; \
                 try again when the server is idle",
                error_text(&records).unwrap_or_default()
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

async fn stop_probe(
    client: &reqwest::Client,
    key: &ProbeKey,
    model: &str,
) -> Result<(), ProviderError> {
    let version = &key.server_version;
    let (status, records) = post(
        client,
        &key.base_url,
        &probe_body(model, STOP_PROBE_PROMPT, key.context_tokens),
    )
    .await
    .map_err(|detail| failed(STOP, model, version, detail))?;
    if !(200..300).contains(&status) || error_text(&records).is_some() {
        return Err(failed(
            STOP,
            model,
            version,
            format!(
                "a one-token request was refused (status {status}: {})",
                error_text(&records).unwrap_or_default()
            ),
        ));
    }
    let done: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("done").and_then(Value::as_bool) == Some(true))
        .collect();
    let [last] = done.as_slice() else {
        return Err(failed(
            STOP,
            model,
            version,
            format!(
                "a one-token request ended with {} final records instead of one",
                done.len()
            ),
        ));
    };
    if !records
        .last()
        .is_some_and(|record| std::ptr::eq(record, *last))
    {
        return Err(failed(
            STOP,
            model,
            version,
            "a one-token request went on after its final record",
        ));
    }
    let reason = last.get("done_reason").and_then(Value::as_str);
    if !matches!(reason, Some("stop" | "length")) {
        return Err(failed(
            STOP,
            model,
            version,
            format!("a one-token request finished with done_reason {reason:?}"),
        ));
    }
    // Ollama omits a zero count.
    let generated = match last.get("eval_count") {
        None => 0,
        Some(count) => count.as_u64().ok_or_else(|| {
            failed(
                STOP,
                model,
                version,
                "a one-token request reported a malformed eval_count",
            )
        })?,
    };
    if generated > 1 {
        return Err(failed(
            STOP,
            model,
            version,
            format!(
                "asked for at most 1 token (num_predict:1), the model generated {generated}; \
                 Axocoatl cannot bound this model's output"
            ),
        ));
    }
    let prompt = last.get("prompt_eval_count").and_then(Value::as_u64);
    match prompt {
        Some(prompt) if prompt > 0 && prompt < key.context_tokens as u64 => Ok(()),
        _ => Err(failed(
            STOP,
            model,
            version,
            format!(
                "a one-token request reported a prompt count of {prompt:?}, not between 0 and the \
                 {}-token context",
                key.context_tokens
            ),
        )),
    }
}
