//! Whether two `provider:model` names are the same model. Owner: core.
//!
//! A model reached through its vendor's own API and the same model reached
//! through OpenRouter carry different names: Claude Code's
//! `anthropic:claude-haiku-4-5` (or its alias `anthropic:haiku`) is
//! OpenRouter's `openrouter:anthropic/claude-haiku-4.5`, and Codex's
//! `openai:gpt-5.5` is `openrouter:openai/gpt-5.5`. The same-model reviewer
//! warning compares [`model_key`]s, so a reviewer on OpenRouter running an
//! external writer's model is warned about like any other same-model pair.

/// One of Claude Code's model aliases: a name `--model` takes that Claude
/// Code turns into a Claude API model id before it calls the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeCodeAlias {
    pub alias: &'static str,
    /// The model the alias runs.
    pub model: &'static str,
    /// How the pinned version resolves the alias, when it is not always
    /// [`Self::model`]: every model it can run has the same window and
    /// output limit as [`Self::model`].
    pub resolves: Option<&'static str>,
}

const fn alias(alias: &'static str, model: &'static str) -> ClaudeCodeAlias {
    ClaudeCodeAlias {
        alias,
        model,
        resolves: None,
    }
}

/// Claude Code 2.1.292's model aliases and the Claude API model each runs:
/// the `aliases` of the model catalog bundled in the pinned build, which it
/// resolves them by for the Claude API when no `ANTHROPIC_DEFAULT_*_MODEL`
/// variable is set (a run sets none) and the API served it no model list of
/// its own (its route allows only `POST /v1/messages`). A trailing `[1m]`
/// (`sonnet[1m]`, `opus[1m]`, `fable[1m]`) asks for the 1M-token window.
/// `default` is not here: what it runs is the account's or organization's
/// default model, which nothing outside the program can know.
pub const CLAUDE_CODE_ALIASES: [ClaudeCodeAlias; 6] = [
    alias("opus", "claude-opus-5-5"),
    alias("sonnet", "claude-sonnet-5-5"),
    alias("haiku", "claude-haiku-4-5"),
    alias("fable", "claude-fable-5-1"),
    ClaudeCodeAlias {
        alias: "best",
        model: "claude-fable-5-1",
        resolves: Some("claude-fable-5-1, or claude-opus-5-5 for an account without Fable"),
    },
    ClaudeCodeAlias {
        alias: "opusplan",
        model: "claude-sonnet-5-5",
        resolves: Some("claude-sonnet-5-5, and claude-opus-5-5 in plan mode"),
    },
];

/// The suffix with which Claude Code asks for a model's 1M-token window.
pub const ONE_MILLION_SUFFIX: &str = "[1m]";

/// `model` without a trailing [`ONE_MILLION_SUFFIX`] (any letter case), and
/// whether it had one.
pub fn split_one_million(model: &str) -> (&str, bool) {
    let model = model.trim();
    match model.len().checked_sub(ONE_MILLION_SUFFIX.len()) {
        Some(at)
            if model.is_char_boundary(at)
                && model[at..].eq_ignore_ascii_case(ONE_MILLION_SUFFIX) =>
        {
            (model[..at].trim(), true)
        }
        _ => (model, false),
    }
}

/// The Claude Code alias `name` is (letter case ignored), if any.
pub fn claude_code_alias(name: &str) -> Option<&'static ClaudeCodeAlias> {
    let name = name.trim();
    CLAUDE_CODE_ALIASES
        .iter()
        .find(|alias| alias.alias.eq_ignore_ascii_case(name))
}

/// Model family words of Claude model ids.
const CLAUDE_FAMILIES: [&str; 6] = ["opus", "sonnet", "haiku", "fable", "mythos", "instant"];

/// A canonical Claude model name with its family first and its version
/// after it: `claude-3-5-haiku` (the older order of the Claude API and
/// OpenRouter) and `claude-haiku-3-5` are both `claude-haiku-3-5`. A name
/// that is not `claude-` with exactly one family word is left as it is.
fn claude_family_version(name: String) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.first() != Some(&"claude") {
        return name;
    }
    let rest = &parts[1..];
    let families: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|part| CLAUDE_FAMILIES.contains(part))
        .collect();
    let [family] = families.as_slice() else {
        return name;
    };
    let number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let numbers = rest.iter().copied().filter(|part| number(part));
    let others = rest
        .iter()
        .copied()
        .filter(|part| part != family && !number(part));
    std::iter::once("claude")
        .chain(std::iter::once(*family))
        .chain(numbers)
        .chain(others)
        .collect::<Vec<_>>()
        .join("-")
}

/// An Anthropic model's name as [`model_key`] writes it: a Claude Code
/// alias (`haiku`) as the model it runs (`claude-haiku-4-5`), a `[1m]`
/// window suffix left out, then [`canonical_name`] and
/// [`claude_family_version`].
fn anthropic_name(model: &str) -> String {
    let (base, _) = split_one_million(model);
    let base = claude_code_alias(base).map_or(base, |alias| alias.model);
    claude_family_version(canonical_name(base))
}

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
/// (Ollama's local names, say), which matches only itself. A Claude model
/// (`anthropic:` or `openrouter:anthropic/`) is keyed by its family and
/// version, `anthropic/claude-haiku-4-5`, whichever order its id writes them
/// in, and on `anthropic:` a Claude Code alias (`anthropic:haiku`) is keyed
/// as the model it runs ([`CLAUDE_CODE_ALIASES`]; `best` and `opusplan` as
/// the model they run by default) and a `[1m]` window suffix is left out.
pub fn model_key(provider: &str, model: &str) -> String {
    let provider = provider.trim().to_ascii_lowercase();
    let model = model.trim();
    if provider == "openrouter" {
        let base = model.split(':').next().unwrap_or(model);
        return match base.split_once('/') {
            Some((vendor, name)) if !vendor.is_empty() && !name.is_empty() => {
                let vendor = vendor.to_ascii_lowercase();
                let name = if vendor == "anthropic" {
                    claude_family_version(canonical_name(name))
                } else {
                    canonical_name(name)
                };
                format!("{vendor}/{name}")
            }
            _ => format!("openrouter:{}", model.to_ascii_lowercase()),
        };
    }
    match VENDOR_APIS
        .iter()
        .find(|(api, _)| *api == provider)
        .map(|(_, vendor)| *vendor)
    {
        Some("anthropic") => format!("anthropic/{}", anthropic_name(model)),
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
    fn a_claude_code_alias_is_the_model_it_runs_through_any_provider() {
        // The re-smoke's pair: a Claude Code writer on the alias and a
        // reviewer on OpenRouter's id of the model the alias runs, both ways.
        assert!(same_model(
            "anthropic",
            "haiku",
            "openrouter",
            "anthropic/claude-haiku-4.5"
        ));
        assert!(same_model(
            "openrouter",
            "anthropic/claude-haiku-4.5",
            "anthropic",
            "haiku"
        ));
        // The alias, the full id, a dated snapshot and OpenRouter's id share
        // one family and version key.
        for (provider, model) in [
            ("anthropic", "haiku"),
            ("anthropic", "HAIKU"),
            ("anthropic", "claude-haiku-4-5"),
            ("anthropic", "claude-haiku-4-5-20251001"),
            ("anthropic", "claude-haiku-4-5[1m]"),
            ("openrouter", "anthropic/claude-haiku-4.5"),
            ("openrouter", "anthropic/claude-4.5-haiku"),
        ] {
            assert_eq!(
                model_key(provider, model),
                "anthropic/claude-haiku-4-5",
                "{provider}:{model}"
            );
        }
        for (alias, model) in [
            ("opus", "anthropic/claude-opus-5.5"),
            ("sonnet", "anthropic/claude-sonnet-5.5"),
            ("sonnet[1m]", "anthropic/claude-sonnet-5.5"),
            ("opus[1M]", "anthropic/claude-opus-5.5"),
            ("fable", "anthropic/claude-fable-5.1"),
            ("best", "anthropic/claude-fable-5.1"),
            ("opusplan", "anthropic/claude-sonnet-5.5"),
        ] {
            assert!(
                same_model("anthropic", alias, "openrouter", model),
                "{alias}"
            );
            assert!(
                same_model("openrouter", model, "anthropic", alias),
                "{alias}"
            );
        }
        // The older order of a Claude id is the same family and version.
        assert!(same_model(
            "anthropic",
            "claude-3-5-haiku-20241022",
            "openrouter",
            "anthropic/claude-3.5-haiku"
        ));
        assert_eq!(
            model_key("anthropic", "claude-3-7-sonnet-latest"),
            "anthropic/claude-sonnet-3-7"
        );
        // Every alias names a model; `default` is no alias.
        for alias in &CLAUDE_CODE_ALIASES {
            assert_eq!(claude_code_alias(alias.alias), Some(alias));
            assert!(alias.model.starts_with("claude-"));
        }
        assert_eq!(claude_code_alias("default"), None);
        assert_eq!(split_one_million(" opus[1M] "), ("opus", true));
        assert_eq!(split_one_million("opus"), ("opus", false));
    }

    #[test]
    fn a_claude_code_alias_is_not_another_model() {
        assert!(!same_model(
            "anthropic",
            "haiku",
            "openrouter",
            "anthropic/claude-sonnet-4.5"
        ));
        assert!(!same_model(
            "anthropic",
            "sonnet",
            "openrouter",
            "anthropic/claude-sonnet-4.5"
        ));
        assert!(!same_model(
            "anthropic",
            "haiku",
            "anthropic",
            "claude-haiku-5-5"
        ));
        // An alias is Claude Code's, not OpenRouter's or another vendor's.
        assert_ne!(
            model_key("openrouter", "anthropic/haiku"),
            model_key("anthropic", "claude-haiku-4-5")
        );
        assert!(!same_model("openai", "haiku", "anthropic", "haiku"));
        assert_eq!(model_key("anthropic", "default"), "anthropic/default");
        // A name with two family words is left as it is.
        assert_eq!(
            model_key("anthropic", "claude-opus-sonnet-4"),
            "anthropic/claude-opus-sonnet-4"
        );
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
