//! Whether two `provider:model` names are the same model. Owner: core.
//!
//! A model reached through its vendor's own API and the same model reached
//! through OpenRouter carry different names: Claude Code's
//! `anthropic:claude-haiku-4-5` is OpenRouter's
//! `openrouter:anthropic/claude-haiku-4.5`, and Codex's `openai:gpt-5.5` is
//! `openrouter:openai/gpt-5.5`. The same-model reviewer warning compares
//! [`model_key`]s, so a reviewer on OpenRouter running an external writer's
//! model is warned about like any other same-model pair.

/// The OpenRouter vendor prefix of each provider whose model names are its
/// vendor's own API ids.
const VENDOR_APIS: [(&str, &str); 4] = [
    ("anthropic", "anthropic"),
    ("openai", "openai"),
    ("gemini", "google"),
    ("mistral", "mistralai"),
];

/// A model's name with what does not change the model left out: lowercase,
/// `.` written as `-` (Anthropic's API writes `claude-haiku-4-5` for
/// OpenRouter's `claude-haiku-4.5`), and a trailing `-latest` or snapshot
/// date (`-20251001`, `-2025-10-01`) removed.
fn canonical_name(name: &str) -> String {
    let mut name = name.trim().to_ascii_lowercase().replace('.', "-");
    if let Some(stripped) = name.strip_suffix("-latest") {
        name = stripped.to_string();
    }
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = name.split('-').collect();
    let keep = match parts.as_slice() {
        [rest @ .., year, month, day]
            if !rest.is_empty() && digits(year, 4) && digits(month, 2) && digits(day, 2) =>
        {
            rest.len()
        }
        [rest @ .., date] if !rest.is_empty() && digits(date, 8) => rest.len(),
        _ => parts.len(),
    };
    parts[..keep].join("-")
}

/// The name two providers' names of one model share: `vendor/model` for a
/// vendor's own API (`anthropic`, `openai`, `gemini`, `mistral`) and for
/// OpenRouter (whose ids are `vendor/model`, a `:variant` suffix such as
/// `:nitro` or `:free` left out), with the model written as
/// [`canonical_name`] does; `provider:model` for every other provider
/// (Ollama's local names, say), which matches only itself.
pub fn model_key(provider: &str, model: &str) -> String {
    let provider = provider.trim().to_ascii_lowercase();
    let model = model.trim();
    if provider == "openrouter" {
        let base = model.split(':').next().unwrap_or(model);
        return match base.split_once('/') {
            Some((vendor, name)) if !vendor.is_empty() && !name.is_empty() => {
                format!("{}/{}", vendor.to_ascii_lowercase(), canonical_name(name))
            }
            _ => format!("openrouter:{}", model.to_ascii_lowercase()),
        };
    }
    match VENDOR_APIS
        .iter()
        .find(|(api, _)| *api == provider)
        .map(|(_, vendor)| *vendor)
    {
        Some(vendor) => format!("{vendor}/{}", canonical_name(model)),
        None => format!("{provider}:{}", model.to_ascii_lowercase()),
    }
}

/// Whether `provider_a:model_a` and `provider_b:model_b` name the same
/// model ([`model_key`]).
pub fn same_model(provider_a: &str, model_a: &str, provider_b: &str, model_b: &str) -> bool {
    model_key(provider_a, model_a) == model_key(provider_b, model_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_api_model_is_the_same_model_on_openrouter() {
        // Claude Code's model id and OpenRouter's.
        assert!(same_model(
            "anthropic",
            "claude-haiku-4-5",
            "openrouter",
            "anthropic/claude-haiku-4.5"
        ));
        // A dated snapshot is the same model.
        assert!(same_model(
            "anthropic",
            "claude-haiku-4-5-20251001",
            "openrouter",
            "anthropic/claude-haiku-4.5"
        ));
        // Codex's model id and OpenRouter's, a routing variant included.
        assert!(same_model(
            "openai",
            "gpt-5.5",
            "openrouter",
            "openai/gpt-5.5"
        ));
        assert!(same_model(
            "openai",
            "gpt-5.5",
            "openrouter",
            "openai/gpt-5.5:nitro"
        ));
        assert!(same_model(
            "openai",
            "gpt-5.6-sol-2026-07-09",
            "openrouter",
            "openai/gpt-5.6-sol"
        ));
        assert!(same_model(
            "gemini",
            "gemini-2.5-pro",
            "openrouter",
            "google/gemini-2.5-pro"
        ));
    }

    #[test]
    fn different_models_and_local_names_stay_different() {
        assert!(!same_model(
            "openai",
            "gpt-5.5",
            "openrouter",
            "openai/gpt-5.4"
        ));
        assert!(!same_model(
            "anthropic",
            "claude-haiku-4-5",
            "openrouter",
            "openai/claude-haiku-4.5"
        ));
        assert!(!same_model("openai", "gpt-5.5", "anthropic", "gpt-5.5"));
        // Ollama's names are local: only the same name is the same model.
        assert!(same_model("ollama", "qwen3:32b", "ollama", "qwen3:32b"));
        assert!(!same_model(
            "ollama",
            "gpt-oss:120b",
            "openrouter",
            "openai/gpt-oss-120b"
        ));
        // A version is not a date.
        assert!(!same_model("openai", "gpt-5", "openai", "gpt-5-2"));
        assert_eq!(model_key("openai", "gpt-5"), "openai/gpt-5");
        assert_eq!(model_key("ollama", "Qwen3:32B"), "ollama:qwen3:32b");
    }
}
