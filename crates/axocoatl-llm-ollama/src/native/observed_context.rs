//! Resolve an explicit context setting from a loaded local runner. This records
//! observations, not immutable model weights or a permanent server default.

use super::{
    invalid, local_client, metadata_request, reports_cloud_disabled, NativeOllamaConfig,
    NativeOllamaProvider, VERSION,
};
use axocoatl_llm::ProviderError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Retain this alongside the native execution configuration. The context is the
/// runner's reported per-request context, not the model's architectural maximum.
/// A later request uses this value explicitly as `num_ctx`; aliases and server
/// defaults can otherwise change after these point-in-time observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOllamaContextObservation {
    pub schema_version: u32,
    pub base_url: String,
    pub requested_model: String,
    pub resolved_model: String,
    pub model_digest: String,
    pub server_version: String,
    pub context_tokens: usize,
    pub observed_at_ms: u64,
    pub load_acknowledged_at: String,
    pub tools_supported: bool,
    pub thinking_supported: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct ModelIdentity {
    name: String,
    digest: String,
}

// Ollama v0.20.6 types/model/name.go: ParseName merges these name defaults,
// EqualFold compares all four components, and DisplayShortest omits the default
// host/namespace. This deliberately does not interpret @digest as a weight pin.
fn model_key(value: &str) -> Result<String, ProviderError> {
    if value.is_empty() || value.len() > 256 || !value.is_ascii() {
        return Err(invalid("unsupported local model name"));
    }
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    let (base, tag) = match value.rfind(':') {
        Some(index) if value.rfind('/').is_none_or(|slash| index > slash) => {
            (&value[..index], &value[index + 1..])
        }
        _ => (value, "latest"),
    };
    let parts: Vec<_> = base.split('/').collect();
    let (host, namespace, model) = match parts.as_slice() {
        [model] => ("registry.ollama.ai", "library", *model),
        [namespace, model] => ("registry.ollama.ai", *namespace, *model),
        [host, namespace, model] => (*host, *namespace, *model),
        _ => return Err(invalid("unsupported local model name")),
    };
    for (kind, part) in [host, namespace, model, tag].iter().enumerate() {
        let max = if kind == 0 { 350 } else { 80 };
        if part.is_empty()
            || part.len() > max
            || !part.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphanumeric()
                    || byte == b'_'
                    || (index > 0
                        && (byte == b'-'
                            || (byte == b'.' && kind != 1)
                            || (byte == b':' && kind == 0)))
            })
        {
            return Err(invalid("unsupported local model name"));
        }
    }
    Ok(format!("{host}/{namespace}/{model}:{tag}").to_ascii_lowercase())
}

fn identity(value: &Value, requested_model: &str) -> Result<ModelIdentity, ProviderError> {
    let expected = model_key(requested_model)?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("local model list was not reported"))?;
    let mut selected = None;
    for record in models {
        let name = record.get("name").and_then(Value::as_str);
        let model = record.get("model").and_then(Value::as_str);
        let name_matches = name.and_then(|name| model_key(name).ok()).as_deref() == Some(&expected);
        let model_matches =
            model.and_then(|model| model_key(model).ok()).as_deref() == Some(&expected);
        if !name_matches && !model_matches {
            continue;
        }
        if !name_matches || !model_matches || selected.is_some() {
            return Err(invalid("ambiguous local model identity"));
        }
        let digest = record
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("local model digest was not reported"))?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("invalid local model digest"));
        }
        selected = Some(ModelIdentity {
            name: name.expect("matching name was present").to_owned(),
            digest: digest.to_ascii_lowercase(),
        });
    }
    selected.ok_or_else(|| invalid("the exact local model was not listed"))
}

fn loaded_context(value: &Value, expected: &ModelIdentity) -> Result<usize, ProviderError> {
    let actual = identity(value, &expected.name)?;
    if actual != *expected {
        return Err(invalid(
            "loaded model identity changed during context observation",
        ));
    }
    let record = value["models"]
        .as_array()
        .expect("identity verified model array")
        .iter()
        .find(|record| record.get("name").and_then(Value::as_str) == Some(&expected.name))
        .ok_or_else(|| invalid("exact loaded model record was not reported"))?;
    if record.pointer("/details/format").and_then(Value::as_str) != Some("gguf") {
        return Err(invalid("loaded runner is not the verified GGUF model"));
    }
    let context = record
        .get("context_length")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid("loaded per-request context was not reported"))?;
    if !(2048..=16 * 1024 * 1024).contains(&context) {
        return Err(invalid(
            "loaded context is outside the native execution profile",
        ));
    }
    Ok(context)
}

async fn profile(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> Result<(bool, bool), ProviderError> {
    let version = metadata_request(client, base_url, "api/version", None).await?;
    if version.get("version").and_then(Value::as_str) != Some(VERSION) {
        return Err(invalid(
            "native execution requires the audited Ollama 0.20.6 server",
        ));
    }
    let status = metadata_request(client, base_url, "api/status", None).await?;
    if !reports_cloud_disabled(&status) {
        return Err(invalid(
            "native execution requires server-reported cloud-disabled mode",
        ));
    }
    let shown =
        metadata_request(client, base_url, "api/show", Some(json!({"model": model}))).await?;
    let remote = ["remote_host", "remote_model"].iter().any(|key| {
        shown
            .get(key)
            .is_some_and(|value| value.as_str() != Some(""))
    });
    let caps = shown
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("model capabilities were not reported"))?;
    if remote
        || shown.pointer("/details/format").and_then(Value::as_str) != Some("gguf")
        || !caps
            .iter()
            .any(|value| value.as_str() == Some("completion"))
        || caps.iter().any(|value| value.as_str() == Some("image"))
    {
        return Err(invalid(
            "context observation requires a local GGUF completion model",
        ));
    }
    Ok((
        caps.iter().any(|value| value.as_str() == Some("tools")),
        caps.iter().any(|value| value.as_str() == Some("thinking")),
    ))
}

fn load_acknowledgement(value: &Value, model: &str) -> Result<String, ProviderError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("invalid model load acknowledgement"))?;
    if value.get("model").and_then(Value::as_str) != Some(model)
        || value.get("done").and_then(Value::as_bool) != Some(true)
        || value.get("done_reason").and_then(Value::as_str) != Some("load")
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "model"
                    | "created_at"
                    | "done"
                    | "done_reason"
                    | "response"
                    | "thinking"
                    | "total_duration"
                    | "load_duration"
                    | "prompt_eval_count"
                    | "prompt_eval_duration"
                    | "eval_count"
                    | "eval_duration"
            )
        })
        || ["response", "thinking"].iter().any(|key| {
            value
                .get(key)
                .is_some_and(|value| value.as_str() != Some(""))
        })
        || [
            "prompt_eval_count",
            "prompt_eval_duration",
            "eval_count",
            "eval_duration",
        ]
        .iter()
        .any(|key| {
            value
                .get(key)
                .is_some_and(|value| value.as_u64() != Some(0))
        })
    {
        return Err(invalid(
            "server did not acknowledge a generation-free model load",
        ));
    }
    let created_at = value
        .get("created_at")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| invalid("model load acknowledgement has no timestamp"))?;
    Ok(created_at.to_owned())
}

/// Load the already installed selected model without a prompt or generation,
/// then observe its actual per-request context. No options, keep-alive override,
/// model download, automatic retry, or server-configuration mutation is sent.
/// Aliases, tags and runner identity must agree across bounded metadata reads.
/// Concurrent external model changes cannot be made atomic by these APIs; this
/// is point-in-time evidence and any detected disagreement is refused.
pub async fn observe_native_ollama_context(
    base_url: &str,
    model: &str,
) -> Result<NativeOllamaContextObservation, ProviderError> {
    model_key(model)?;
    let client = local_client(base_url)?;
    let capabilities = profile(&client, base_url, model).await?;
    let first = identity(
        &metadata_request(&client, base_url, "api/tags", None).await?,
        model,
    )?;
    // GenerateHandler returns done_reason=load before tokenization/completion
    // for the empty prompt. Omitting options preserves the server/model choice.
    let loaded = metadata_request(
        &client,
        base_url,
        "api/generate",
        Some(json!({"model": first.name, "stream": false})),
    )
    .await?;
    let load_acknowledged_at = load_acknowledgement(&loaded, &first.name)?;
    let context = loaded_context(
        &metadata_request(&client, base_url, "api/ps", None).await?,
        &first,
    )?;
    if profile(&client, base_url, model).await? != capabilities {
        return Err(invalid(
            "local model capabilities changed during context observation",
        ));
    }
    let second = identity(
        &metadata_request(&client, base_url, "api/tags", None).await?,
        model,
    )?;
    if first != second {
        return Err(invalid(
            "local model identity changed during context observation",
        ));
    }
    if loaded_context(
        &metadata_request(&client, base_url, "api/ps", None).await?,
        &second,
    )? != context
    {
        return Err(invalid("loaded context changed during observation"));
    }
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or_else(|| invalid("local context observation clock is unavailable"))?;
    Ok(NativeOllamaContextObservation {
        schema_version: 1,
        base_url: base_url.to_owned(),
        requested_model: model.to_owned(),
        resolved_model: first.name,
        model_digest: first.digest,
        server_version: VERSION.into(),
        context_tokens: context,
        observed_at_ms,
        load_acknowledged_at,
        tools_supported: capabilities.0,
        thinking_supported: capabilities.1,
    })
}

pub(super) async fn verify_identity(
    provider: &NativeOllamaProvider,
    observation: &NativeOllamaContextObservation,
) -> Result<(), ProviderError> {
    if profile(
        &provider.client,
        &observation.base_url,
        &observation.requested_model,
    )
    .await?
        != (observation.tools_supported, observation.thinking_supported)
    {
        return Err(invalid("observed local model capabilities changed"));
    }
    let actual = identity(
        &provider.metadata("api/tags", None).await?,
        &observation.requested_model,
    )?;
    if actual.name != observation.resolved_model || actual.digest != observation.model_digest {
        return Err(invalid(
            "observed local model identity changed; resolve a new execution profile",
        ));
    }
    Ok(())
}

impl NativeOllamaProvider {
    /// Use a retained context observation with caller-owned finite output and
    /// byte limits. Reconnection does not reload or infer a new context default.
    /// Identity is checked now and before each inference, while the observed
    /// context is sent explicitly even if the server's default has since changed.
    pub async fn connect_observed(
        observation: NativeOllamaContextObservation,
        max_output_tokens: usize,
        max_response_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if observation.schema_version != 1
            || observation.server_version != VERSION
            || model_key(&observation.requested_model)? != model_key(&observation.resolved_model)?
            || observation.load_acknowledged_at.is_empty()
            || observation.load_acknowledged_at.len() > 128
            || observation
                .load_acknowledged_at
                .chars()
                .any(char::is_control)
        {
            return Err(invalid("invalid retained native context observation"));
        }
        let mut provider = Self::connect(NativeOllamaConfig {
            base_url: observation.base_url.clone(),
            model: observation.requested_model.clone(),
            context_tokens: observation.context_tokens,
            max_output_tokens,
            max_response_bytes,
        })
        .await?;
        if provider.tools_supported != observation.tools_supported
            || provider.thinking_supported != observation.thinking_supported
        {
            return Err(invalid("observed local model capabilities changed"));
        }
        verify_identity(&provider, &observation).await?;
        provider.observed_context = Some(observation);
        Ok(provider)
    }
}

#[cfg(test)]
mod tests;
