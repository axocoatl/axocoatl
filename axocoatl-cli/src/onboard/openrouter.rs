//! Step 3D: OpenRouter, for the writer and the reviewer, or the reviewer
//! alone. Two separate models (the reviewer must differ), the key from an
//! environment variable or a hidden prompt (never shown or logged), and a
//! check before anything is written: OpenRouter accepts the key and lists
//! both models.

use super::config::{KeyRef, ModelRef, Provider};
use super::{Choice, Onboarding};

pub(crate) const OPENROUTER_API: &str = "https://openrouter.ai/api/v1";

/// The writer onboarding suggests: a coding model.
pub(crate) const DEFAULT_WRITER: &str = "qwen/qwen3-coder";
/// The reviewer onboarding suggests: another vendor's model, the family of
/// the reviewer Axocoatl has measured (gpt-oss 120B).
pub(crate) const DEFAULT_REVIEWER: &str = "openai/gpt-oss-120b";

/// What OpenRouter said about the key and the models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyCheck {
    Accepted,
    Refused(u16),
    Unchecked(String),
}

/// Ask OpenRouter whether it accepts `key` (`GET /key`).
pub(crate) async fn check_key(api: &str, key: &str) -> KeyCheck {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => return KeyCheck::Unchecked(error.to_string()),
    };
    match client
        .get(format!("{}/key", api.trim_end_matches('/')))
        .bearer_auth(key)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => KeyCheck::Accepted,
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
            KeyCheck::Refused(response.status().as_u16())
        }
        Ok(response) => KeyCheck::Unchecked(format!("OpenRouter answered {}", response.status())),
        Err(error) => KeyCheck::Unchecked(error.without_url().to_string()),
    }
}

/// The model ids OpenRouter lists (`GET /models`), or why they could not be
/// read.
pub(crate) async fn model_ids(api: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(format!("{}/models", api.trim_end_matches('/')))
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("OpenRouter answered {}", response.status()));
    }
    let models: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
    Ok(models
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect())
}

fn flag_model(spec: &Option<String>) -> Option<String> {
    spec.as_deref()
        .and_then(|spec| ModelRef::parse(spec).ok())
        .filter(|model| model.provider == Provider::OpenRouter)
        .map(|model| model.model)
}

/// The key: `--openrouter-key-env VAR`, else (at a terminal)
/// `OPENROUTER_API_KEY` when set, else a hidden prompt.
fn key(o: &mut Onboarding) -> Option<(KeyRef, zeroize::Zeroizing<String>)> {
    if o.ui.plan() {
        let variable = o
            .args
            .openrouter_key_env
            .clone()
            .unwrap_or_else(|| "OPENROUTER_API_KEY".to_string());
        o.ui.say(format!(
            "  would read the OpenRouter key from ${variable}, or ask for it at a hidden prompt, \
             and check it and both models with OpenRouter"
        ));
        return Some((
            KeyRef::Env(variable),
            zeroize::Zeroizing::new(String::new()),
        ));
    }
    if let Some(variable) = o.args.openrouter_key_env.clone() {
        return match std::env::var(&variable)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            Some(value) => {
                o.ui.say(format!(
                    "  OpenRouter key: from ${variable} (the configuration names the variable)"
                ));
                Some((
                    KeyRef::Env(variable),
                    zeroize::Zeroizing::new(value.trim().to_string()),
                ))
            }
            None => {
                o.step_left(format!(
                    "Export {variable} with your OpenRouter key in this shell, then run \
                     `axocoatl onboard` again"
                ));
                None
            }
        };
    }
    if let Some(value) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        if o.ui.consent(
            "Use the OpenRouter key in $OPENROUTER_API_KEY? The configuration names the variable, \
             not the key, so start Axocoatl from a shell that has it.",
            true,
            false,
        ) {
            return Some((
                KeyRef::Env("OPENROUTER_API_KEY".to_string()),
                zeroize::Zeroizing::new(value.trim().to_string()),
            ));
        }
    }
    match o
        .ui
        .secret("OpenRouter API key (hidden; stored in your owner-only Axocoatl configuration)")
    {
        Some(value) => Some((KeyRef::Value(value.clone()), value)),
        None => {
            o.step_left(
                "Give onboarding your OpenRouter key: export OPENROUTER_API_KEY in this shell and \
                 run `axocoatl onboard --openrouter-key-env OPENROUTER_API_KEY`",
            );
            None
        }
    }
}

/// Step 3D. With `with_writer`, the writer runs on OpenRouter too.
pub(crate) async fn run(o: &mut Onboarding, choice: &mut Choice, with_writer: bool) {
    if !o.ui.consent(
        "Use OpenRouter credits, with no BYOK provider keys connected? (Native Sessions do not \
         support BYOK)",
        true,
        false,
    ) {
        o.step_left("Native OpenRouter needs OpenRouter credit billing without BYOK provider keys");
        return;
    }
    let writer = if with_writer {
        Some(match flag_model(&o.args.writer_model) {
            Some(model) => model,
            None => {
                o.ui.input("Model for the writer (vendor/model)", DEFAULT_WRITER)
            }
        })
    } else {
        None
    };
    let writer_name = writer.clone().or_else(|| {
        choice
            .writer_model
            .as_ref()
            .map(|model| model.model.clone())
    });
    let mut reviewer = match flag_model(&o.args.reviewer) {
        Some(model) => model,
        None => o.ui.input(
            "Model for the reviewer (must be a different model)",
            if writer_name.as_deref() == Some(DEFAULT_REVIEWER) {
                DEFAULT_WRITER
            } else {
                DEFAULT_REVIEWER
            },
        ),
    };
    while writer_name.as_deref() == Some(reviewer.as_str()) {
        o.ui.fail(format!(
            "The reviewer must be a different model from the writer ({reviewer}): a second model \
             checks the writer's change"
        ));
        if !o.ui.interactive() {
            o.step_left("Choose a different reviewer: --reviewer openrouter:VENDOR/MODEL");
            return;
        }
        reviewer = o.ui.input(
            "Model for the reviewer (a different model)",
            DEFAULT_REVIEWER,
        );
    }
    let Some((key_ref, key)) = key(o) else {
        return;
    };
    if !o.ui.plan() {
        match check_key(&o.openrouter_api, &key).await {
            KeyCheck::Accepted => o.ui.ok("OpenRouter accepted the key"),
            KeyCheck::Refused(status) => {
                o.step_left(format!(
                    "OpenRouter refused the key ({status}): check it at \
                     https://openrouter.ai/settings/keys, then run `axocoatl onboard` again"
                ));
                return;
            }
            KeyCheck::Unchecked(why) => o.ui.warn(format!("The key could not be checked: {why}")),
        }
        match model_ids(&o.openrouter_api).await {
            Ok(ids) => {
                let mut missing = false;
                for model in writer.iter().chain(std::iter::once(&reviewer)) {
                    if ids.iter().any(|id| id == model) {
                        o.ui.ok(format!("OpenRouter lists {model}"));
                    } else {
                        missing = true;
                        o.step_left(format!(
                            "OpenRouter does not list {model}: choose a model from \
                             https://openrouter.ai/models"
                        ));
                    }
                }
                if missing {
                    return;
                }
            }
            Err(why) => {
                o.ui.warn(format!("The model list could not be read: {why}"))
            }
        }
    }
    choice.openrouter_key = Some(key_ref);
    if let Some(writer) = writer {
        choice.writer_model = Some(ModelRef::new(Provider::OpenRouter, &writer));
    }
    choice.reviewer_model = Some(ModelRef::new(Provider::OpenRouter, &reviewer));
}
