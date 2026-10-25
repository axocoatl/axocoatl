//! Hosted provider keys kept in the secret store rather than in the
//! configuration: `providers.<name>.api_key_secret: <secret>` names a secret
//! under `{data root}/secrets/` (written by `axocoatl connect openrouter`, or
//! `axocoatl secret set`). The daemon reads each one once, when it starts,
//! into the provider's `api_key` ([`resolve_provider_key_secrets`]); the
//! configuration file keeps only the name, and so does the copy the daemon
//! compares a reload with. A key that is named but not stored leaves the
//! provider unregistered, as an empty `api_key` does, with a warning that
//! says how to store it. No value is ever formatted into a message.

use std::path::Path;

use axocoatl_config::{shown_secret_name, AxocoatlConfig, ProviderCredentials, SecretString};

use crate::secret_store;

/// The secret `axocoatl connect openrouter` stores OpenRouter's key as.
pub const OPENROUTER_SECRET: &str = "openrouter";

/// Where a configured provider's key comes from, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// `api_key` holds a value (written in the file, or from `${VARIABLE}`).
    Config,
    /// `api_key_secret` names a secret, stored or not. A name that
    /// [`never_a_provider_key`] refuses for this provider counts as not
    /// stored. Show `name` with [`shown_secret_name`]: a key written where
    /// the name belongs must not be printed.
    Secret { name: String, stored: bool },
    /// Neither: the provider is not registered.
    Missing,
}

/// Where `provider`'s key (`credentials`) comes from in the data root
/// `data_dir`. A name the store cannot answer for counts as not stored.
pub fn key_source(provider: &str, credentials: &ProviderCredentials, data_dir: &Path) -> KeySource {
    match credentials.api_key_secret.as_deref() {
        Some(name) => KeySource::Secret {
            name: name.to_string(),
            stored: never_a_provider_key(provider, name).is_none()
                && secret_store::has_secret(data_dir, name).unwrap_or(false),
        },
        None if credentials.api_key.is_empty() => KeySource::Missing,
        None => KeySource::Config,
    }
}

/// How to store the key a provider's `api_key_secret` names. A name that
/// may be a key is not repeated; the advice is then to write a name there.
pub fn how_to_store_key(provider: &str, name: &str) -> String {
    let shown = shown_secret_name(name);
    if shown != name {
        return format!(
            "write the name of a secret in providers.{provider}.api_key_secret, never the key \
             itself (delete a key written there from the file and its backups), then {}",
            how_to_store_key(
                provider,
                if provider == "openrouter" {
                    OPENROUTER_SECRET
                } else {
                    provider
                }
            )
        );
    }
    if provider == "openrouter" {
        if name == OPENROUTER_SECRET {
            "run `axocoatl connect openrouter`, which signs you in to OpenRouter in the browser \
             and stores the key it issues without showing it"
                .to_string()
        } else {
            format!(
                "run `axocoatl connect openrouter --secret {name}`, which signs you in to \
                 OpenRouter in the browser and stores the key it issues without showing it"
            )
        }
    } else {
        format!(
            "pipe the key into `axocoatl secret set {name}` (it reads standard input) or name a \
             variable with `--from-env`"
        )
    }
}

/// Why the secret `name` never holds `provider`'s API key, if it is one of
/// the secrets another command owns: the receipt signing key, the Claude
/// Code sign-in (`axocoatl connect claude-code`), the ChatGPT sign-in
/// (`axocoatl connect codex`), and, for any provider but OpenAI, Codex's
/// OpenAI API key (`codex-openai`). Such a secret is never
/// sent as that provider's key, and a command that stores a provider key
/// refuses to store it there (it would replace that secret) or remove it.
pub fn never_a_provider_key(provider: &str, name: &str) -> Option<String> {
    if crate::receipt_key::is_receipt_key_name(name) {
        return Some(crate::receipt_key::NEVER_A_ROUTE_CREDENTIAL.to_string());
    }
    if name == crate::external_agent::codex::CODEX_CHATGPT_SECRET {
        return Some(
            "it holds the ChatGPT sign-in `axocoatl connect codex` stores for Codex's route, not \
             a provider API key"
                .to_string(),
        );
    }
    if name == crate::external_agent::claude_code::CLAUDE_CODE_SECRET {
        return Some(
            "it holds the Claude Code sign-in `axocoatl connect claude-code` stores, not a \
             provider API key"
                .to_string(),
        );
    }
    if name == crate::external_agent::codex::CODEX_SECRET && provider != "openai" {
        return Some(format!(
            "it holds the OpenAI API key Codex's route sends (codex-openai), not a key for \
             providers.{provider}"
        ));
    }
    None
}

/// Read every `api_key_secret` in `config` from the secret store under
/// `data_dir` into its provider's `api_key`. Returns one warning per key
/// that could not be read (never a value, and never a name that may be a
/// key); those providers keep an empty key and are not registered.
pub fn resolve_provider_key_secrets(config: &mut AxocoatlConfig, data_dir: &Path) -> Vec<String> {
    let providers = &mut config.providers;
    let mut warnings = Vec::new();
    for (provider, credentials) in [
        ("openai", providers.openai.as_mut()),
        ("anthropic", providers.anthropic.as_mut()),
        ("gemini", providers.gemini.as_mut()),
        ("mistral", providers.mistral.as_mut()),
        ("openrouter", providers.openrouter.as_mut()),
    ] {
        let Some(credentials) = credentials else {
            continue;
        };
        let Some(name) = credentials.api_key_secret.clone() else {
            continue;
        };
        let shown = shown_secret_name(&name);
        match read_key(provider, &name, data_dir) {
            Ok(Some(key)) => credentials.api_key = key,
            Ok(None) => warnings.push(format!(
                "providers.{provider}.api_key_secret names the secret {shown}, which is not \
                 stored, so {provider} is not configured; {}",
                how_to_store_key(provider, &name)
            )),
            Err(reason) => warnings.push(format!(
                "providers.{provider}.api_key_secret names the secret {shown}, which cannot be \
                 used ({reason}), so {provider} is not configured"
            )),
        }
    }
    warnings
}

/// The value of secret `name`, `None` when it is not stored.
fn read_key(provider: &str, name: &str, data_dir: &Path) -> Result<Option<SecretString>, String> {
    if let Some(reason) = never_a_provider_key(provider, name) {
        return Err(reason);
    }
    if shown_secret_name(name) != name {
        // Not a secret name the store takes, or one that reads as a key.
        return Ok(None);
    }
    let Some(lock) = secret_store::lock_secrets(data_dir, false).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let Some(value) = lock.read(name).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    // The store keeps UTF-8 text only (`normalize_secret_value`).
    let text = std::str::from_utf8(&value)
        .map_err(|_| "the stored value is not text".to_string())?
        .to_owned();
    Ok(Some(SecretString::from(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_root() -> tempfile::TempDir {
        let directory = crate::private_tempdir_test_support::private_tempdir();
        crate::AxocoatlDaemon::initialize_data_root(directory.path()).unwrap();
        directory
    }

    fn named(name: &str) -> ProviderCredentials {
        ProviderCredentials {
            api_key: SecretString::default(),
            api_key_secret: Some(name.to_string()),
            base_url: None,
            fallback: None,
            first_token_timeout_secs: None,
            idle_timeout_secs: None,
        }
    }

    #[test]
    fn a_named_key_is_read_from_the_store_and_a_missing_one_is_named_without_a_value() {
        let root = data_root();
        secret_store::set_secret(root.path(), OPENROUTER_SECRET, b"sk-or-v1-stored\n").unwrap();
        let mut config = AxocoatlConfig::default();
        config.providers.openrouter = Some(named(OPENROUTER_SECRET));
        config.providers.anthropic = Some(named("anthropic-key"));
        config.providers.openai = Some(ProviderCredentials {
            api_key: "sk-in-config".into(),
            api_key_secret: None,
            base_url: None,
            fallback: None,
            first_token_timeout_secs: None,
            idle_timeout_secs: None,
        });

        assert_eq!(
            key_source(
                "openrouter",
                config.providers.openrouter.as_ref().unwrap(),
                root.path()
            ),
            KeySource::Secret {
                name: OPENROUTER_SECRET.into(),
                stored: true
            }
        );
        assert_eq!(
            key_source(
                "anthropic",
                config.providers.anthropic.as_ref().unwrap(),
                root.path()
            ),
            KeySource::Secret {
                name: "anthropic-key".into(),
                stored: false
            }
        );
        assert_eq!(
            key_source(
                "openai",
                config.providers.openai.as_ref().unwrap(),
                root.path()
            ),
            KeySource::Config
        );

        let warnings = resolve_provider_key_secrets(&mut config, root.path());
        let openrouter = config.providers.openrouter.as_ref().unwrap();
        assert_eq!(openrouter.api_key.expose_secret(), "sk-or-v1-stored");
        assert_eq!(
            openrouter.api_key_secret.as_deref(),
            Some(OPENROUTER_SECRET)
        );
        assert!(config
            .providers
            .anthropic
            .as_ref()
            .unwrap()
            .api_key
            .is_empty());
        assert_eq!(
            config
                .providers
                .openai
                .as_ref()
                .unwrap()
                .api_key
                .expose_secret(),
            "sk-in-config"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("providers.anthropic.api_key_secret"));
        assert!(warnings[0].contains("axocoatl secret set anthropic-key"));
        assert!(!warnings.concat().contains("sk-or-v1-stored"));
    }

    #[test]
    fn a_missing_openrouter_key_says_to_connect() {
        let root = data_root();
        let mut config = AxocoatlConfig::default();
        config.providers.openrouter = Some(named(OPENROUTER_SECRET));
        let warnings = resolve_provider_key_secrets(&mut config, root.path());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("run `axocoatl connect openrouter`"),
            "{}",
            warnings[0]
        );
        assert_eq!(
            how_to_store_key("openrouter", "work"),
            "run `axocoatl connect openrouter --secret work`, which signs you in to OpenRouter \
             in the browser and stores the key it issues without showing it"
        );
    }

    #[test]
    fn another_commands_secret_is_never_sent_as_a_provider_key() {
        let root = data_root();
        for name in [
            crate::receipt_key::RECEIPT_KEY_SECRET,
            crate::external_agent::codex::CODEX_CHATGPT_SECRET,
            crate::external_agent::claude_code::CLAUDE_CODE_SECRET,
            crate::external_agent::codex::CODEX_SECRET,
        ] {
            secret_store::set_secret(root.path(), name, b"never-sent-value").unwrap();
            let mut config = AxocoatlConfig::default();
            config.providers.openrouter = Some(named(name));
            config.providers.anthropic = Some(named(name));
            let warnings = resolve_provider_key_secrets(&mut config, root.path());
            for credentials in [&config.providers.openrouter, &config.providers.anthropic] {
                assert!(credentials.as_ref().unwrap().api_key.is_empty(), "{name}");
            }
            assert_eq!(warnings.len(), 2, "{name}: {warnings:?}");
            for warning in &warnings {
                assert!(warning.contains("cannot be used"), "{warning}");
                assert!(!warning.contains("never-sent-value"));
            }
            assert_eq!(
                key_source(
                    "openrouter",
                    config.providers.openrouter.as_ref().unwrap(),
                    root.path()
                ),
                KeySource::Secret {
                    name: name.into(),
                    stored: false
                }
            );
            assert!(never_a_provider_key("openrouter", name).is_some(), "{name}");
        }
        // Codex's OpenAI key (codex-openai) is OpenAI's key.
        let codex_openai = crate::external_agent::codex::CODEX_SECRET;
        assert_eq!(never_a_provider_key("openai", codex_openai), None);
        let mut config = AxocoatlConfig::default();
        config.providers.openai = Some(named(codex_openai));
        assert!(resolve_provider_key_secrets(&mut config, root.path()).is_empty());
        assert_eq!(
            config
                .providers
                .openai
                .as_ref()
                .unwrap()
                .api_key
                .expose_secret(),
            "never-sent-value"
        );
    }

    /// A key written where the secret's name belongs is never repeated in
    /// a warning or in the advice, even when the store would take it as a
    /// name.
    #[test]
    fn a_name_that_may_be_a_key_is_never_repeated() {
        let root = data_root();
        for (provider, key) in [
            ("mistral", "FAKEmistralKEY0123456789abcdef"),
            ("gemini", "AIzaFAKEgeminiKEY0123456789abcd"),
            (
                "openrouter",
                "sk-or-v1-FAKE0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab",
            ),
        ] {
            let mut config = AxocoatlConfig::default();
            match provider {
                "mistral" => config.providers.mistral = Some(named(key)),
                "gemini" => config.providers.gemini = Some(named(key)),
                _ => config.providers.openrouter = Some(named(key)),
            }
            let warnings = resolve_provider_key_secrets(&mut config, root.path());
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            let advice = how_to_store_key(provider, key);
            assert!(
                warnings[0].contains(axocoatl_config::SECRET_NAME_NOT_SHOWN),
                "{}",
                warnings[0]
            );
            for text in [&warnings[0], &advice] {
                assert!(!text.contains("FAKE"), "{text}");
                assert!(text.contains("never the key"), "{text}");
            }
        }
    }
}
