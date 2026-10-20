//! What a refused Ollama metadata request reports: its HTTP status and
//! Ollama's own message, bounded and with control characters removed,
//! instead of one generic sentence for every failure. A model Ollama does
//! not have is [`ProviderError::ModelNotFound`], which
//! [`native_ollama_error_hint`] turns into the `ollama pull` that fixes it;
//! any other status is [`ProviderError::ApiError`], so a busy server (503)
//! or a failed model load (500) is retried like any other server error.

use axocoatl_llm::ProviderError;
use serde_json::Value;

use super::PROVIDER;

/// Longest Ollama error message kept, in bytes.
pub(crate) const MAX_OLLAMA_MESSAGE_BYTES: usize = 1024;

/// Ollama's own message in an error `body`: the `error` field of its JSON
/// (`{"error":"model 'x' not found"}`), else the text itself. Control
/// characters become spaces, runs of spaces one, and the message is cut at
/// [`MAX_OLLAMA_MESSAGE_BYTES`] on a character boundary.
pub(crate) fn ollama_message(body: &str) -> String {
    let text = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());
    let mut cleaned = String::with_capacity(text.len().min(MAX_OLLAMA_MESSAGE_BYTES));
    for character in text.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if character == ' ' && (cleaned.is_empty() || cleaned.ends_with(' ')) {
            continue;
        }
        if cleaned.len() + character.len_utf8() > MAX_OLLAMA_MESSAGE_BYTES {
            cleaned.push('…');
            break;
        }
        cleaned.push(character);
    }
    cleaned.trim_end().to_owned()
}

/// The error of a metadata request that Ollama answered with the HTTP
/// `status` and `body`. `model` is the model the request named, if any: a
/// 404 about it is [`ProviderError::ModelNotFound`] (a 404 for an endpoint
/// an older server lacks is not). Anything else keeps the status and
/// Ollama's message.
pub(crate) fn status_error(status: u16, model: Option<&str>, body: &str) -> ProviderError {
    if let (404, Some(model)) = (status, model) {
        return ProviderError::ModelNotFound {
            provider: PROVIDER.into(),
            model: model.chars().take(256).collect(),
        };
    }
    let message = ollama_message(body);
    ProviderError::ApiError {
        provider: PROVIDER.into(),
        status,
        message: if message.is_empty() {
            "Ollama gave no reason".into()
        } else {
            message
        },
    }
}

/// What to do about `error` from a native Ollama server, when Axocoatl
/// knows: `ollama pull` for a model Ollama does not have, and for a model
/// Ollama could not load, that it may need more memory than is free.
pub fn native_ollama_error_hint(error: &ProviderError) -> Option<String> {
    match error {
        ProviderError::ModelNotFound { provider, model } if provider == PROVIDER => Some(format!(
            "Ollama does not have {model}: run `ollama pull {model}`"
        )),
        ProviderError::ApiError {
            provider,
            status: 500,
            message,
        } if provider == PROVIDER && {
            let lower = message.to_ascii_lowercase();
            lower.contains("unable to load") || lower.contains("memory")
        } =>
        {
            Some(
                "Ollama could not load the model: it may need more memory than this computer \
                 has free; close what else uses it, or choose a smaller model"
                    .into(),
            )
        }
        _ => None,
    }
}

/// `error` in words with what to do about it, when Axocoatl knows
/// ([`native_ollama_error_hint`]).
pub fn describe_native_ollama_error(error: &ProviderError) -> String {
    match native_ollama_error_hint(error) {
        Some(hint) => format!("{error}. {hint}"),
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollamas_own_message_is_kept_bounded_and_clean() {
        assert_eq!(
            ollama_message(r#"{"error":"unable to load full model on GPU"}"#),
            "unable to load full model on GPU"
        );
        assert_eq!(ollama_message("404 page not found\n"), "404 page not found");
        assert_eq!(
            ollama_message("{\"error\":\"line one\\n\\u001b[31mline two\"}"),
            "line one [31mline two"
        );
        let long = format!(r#"{{"error":"{}"}}"#, "é".repeat(2000));
        let kept = ollama_message(&long);
        assert!(kept.len() <= MAX_OLLAMA_MESSAGE_BYTES + '…'.len_utf8());
        assert!(kept.ends_with('…'));
        assert_eq!(ollama_message(""), "");
    }

    #[test]
    fn a_missing_model_is_model_not_found_and_other_statuses_keep_theirs() {
        let error = status_error(
            404,
            Some("qwen3-coder:30b"),
            r#"{"error":"model 'qwen3-coder:30b' not found"}"#,
        );
        assert!(
            matches!(&error, ProviderError::ModelNotFound { model, .. } if model == "qwen3-coder:30b")
        );
        assert_eq!(
            describe_native_ollama_error(&error),
            "Model not found: qwen3-coder:30b on ollama. Ollama does not have qwen3-coder:30b: \
             run `ollama pull qwen3-coder:30b`"
        );
        // A 404 for an endpoint, not a model, is the status it was.
        let error = status_error(404, None, "404 page not found");
        assert_eq!(
            error.to_string(),
            "ollama API error: 404 - 404 page not found"
        );
        let error = status_error(
            500,
            Some("gpt-oss:120b"),
            r#"{"error":"unable to load full model on GPU"}"#,
        );
        assert_eq!(
            error.to_string(),
            "ollama API error: 500 - unable to load full model on GPU"
        );
        assert!(native_ollama_error_hint(&error)
            .unwrap()
            .contains("more memory than this computer has free"));
        let error = status_error(503, None, "");
        assert_eq!(
            error.to_string(),
            "ollama API error: 503 - Ollama gave no reason"
        );
        assert!(native_ollama_error_hint(&error).is_none());
    }
}
