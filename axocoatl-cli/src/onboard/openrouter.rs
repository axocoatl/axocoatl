//! Step 3D: OpenRouter, for the writer and the reviewer, or the reviewer
//! alone. Two separate models (the reviewer must differ), and the key:
//! one connected in the browser before (`axocoatl connect openrouter`,
//! stored in Axocoatl's secret store), one the configuration or
//! `--openrouter-key-env` names, `OPENROUTER_API_KEY`, or else, at a
//! terminal, a browser sign-in (the default: OpenRouter's OAuth PKCE flow,
//! the same as `axocoatl connect openrouter`, which stores the key it
//! issues as a secret), an environment variable or a hidden prompt. A key
//! is never shown or logged. Before anything is written, OpenRouter
//! accepts the key and lists both models (a sign-in checks both itself).

use std::path::Path;

use axocoatl_daemon::provider_secrets::{key_source, KeySource, OPENROUTER_SECRET};
use axocoatl_daemon::secret_store;

use super::config::{KeyRef, ModelRef, Provider};
use super::ui::Ui;
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

/// The key Step 3D uses: where the configuration reads it from, and the
/// value to check with OpenRouter (`None` when a sign-in just checked it,
/// or with `--plan`). (No `Debug`: it holds the key.)
pub(crate) struct Key {
    pub(crate) reference: KeyRef,
    pub(crate) check: Option<zeroize::Zeroizing<String>>,
}

/// How OpenRouter's keys start: only such a value is sent to OpenRouter.
const KEY_PREFIX: &str = "sk-or-";

/// Whether Axocoatl's secret store under `data_dir` holds the OpenRouter
/// key secret `name`: the check `axocoatl doctor` makes.
fn stored(data_dir: &Path, name: &str) -> bool {
    let credentials = axocoatl_config::ProviderCredentials {
        api_key: Default::default(),
        api_key_secret: Some(name.to_string()),
        base_url: None,
        fallback: None,
        first_token_timeout_secs: None,
        idle_timeout_secs: None,
    };
    matches!(
        key_source("openrouter", &credentials, data_dir),
        KeySource::Secret { stored: true, .. }
    )
}

/// The key the secret `name` holds, or why it cannot be used.
fn read_stored(data_dir: &Path, name: &str) -> Result<zeroize::Zeroizing<String>, String> {
    let lock = secret_store::lock_secrets(data_dir, false)
        .map_err(|error| error.to_string())?
        .ok_or("it is not stored")?;
    let value = lock
        .read(name)
        .map_err(|error| error.to_string())?
        .ok_or("it is not stored")?;
    let text = std::str::from_utf8(&value).map_err(|_| "it is not text")?;
    let key = zeroize::Zeroizing::new(text.trim().to_string());
    if !key.starts_with(KEY_PREFIX) {
        return Err(format!(
            "it does not hold an OpenRouter key (they start with {KEY_PREFIX}), so it was not \
             sent to OpenRouter"
        ));
    }
    Ok(key)
}

/// The secret that holds a key connected in the browser, which this step
/// uses without asking: the one the configuration names, else (when it
/// names no key) the one `axocoatl connect openrouter` stores, when
/// Axocoatl's secret store holds it.
fn connected(o: &Onboarding) -> Option<String> {
    let data_dir = o.data_dir();
    if let Some(name) = &o.kept.openrouter_key_secret {
        if stored(&data_dir, name) {
            return Some(name.clone());
        }
    }
    (o.kept.openrouter_key.is_none() && stored(&data_dir, OPENROUTER_SECRET))
        .then(|| OPENROUTER_SECRET.to_string())
}

/// The command that signs in again and stores the key as the secret `name`,
/// pointing the configuration at it.
fn connect_command(name: &str) -> String {
    if name == OPENROUTER_SECRET {
        "axocoatl connect openrouter".to_string()
    } else {
        format!("axocoatl connect openrouter --secret {name}")
    }
}

/// The connected key `name`, read to be checked (never shown), or, when it
/// cannot be used, the step left.
fn use_connected(o: &mut Onboarding, name: &str, again: &str) -> Option<Key> {
    match read_stored(&o.data_dir(), name) {
        Ok(value) => {
            o.ui.say(format!(
                "  OpenRouter key: the one you connected, stored as secret {name} (never shown)"
            ));
            Some(Key {
                reference: KeyRef::Secret(name.to_string()),
                check: Some(value),
            })
        }
        Err(why) => {
            o.step_left(format!(
                "The OpenRouter key secret {name} cannot be used ({why}): run `{}` to sign in \
                 again (it replaces the stored key), then run `{again}`",
                connect_command(name)
            ));
            None
        }
    }
}

/// What "Update this setup" does with the key the configuration already
/// has (`api_key`: a key, or `${VARIABLE}`): `Some` to use it, `None` to
/// ask for one (or, without a terminal, leave the step).
fn kept_key(o: &mut Onboarding) -> Option<Option<Key>> {
    let kept = o.kept.openrouter_key.clone()?;
    let variable = kept
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .map(str::to_string);
    match variable {
        Some(variable) => {
            let value = std::env::var(&variable)
                .ok()
                .filter(|value| !value.trim().is_empty());
            match value {
                Some(value)
                    if !o.ui.interactive()
                        || o.ui.consent(
                            &format!(
                                "Keep reading the OpenRouter key from ${variable}, as your \
                                 configuration does? (The Always-On Service cannot read shell \
                                 variables; No asks for another way to give the key.)"
                            ),
                            true,
                            false,
                        ) =>
                {
                    o.ui.say(format!(
                        "  OpenRouter key: from ${variable}, as your configuration says"
                    ));
                    Some(Some(Key {
                        reference: KeyRef::Env(variable),
                        check: Some(zeroize::Zeroizing::new(value.trim().to_string())),
                    }))
                }
                Some(_) => None,
                None if o.ui.interactive() => None,
                None => {
                    o.step_left(format!(
                        "Export {variable} with your OpenRouter key (your configuration reads it \
                         from there) and run `axocoatl onboard` again, or run it in a terminal to \
                         sign in to OpenRouter in your browser instead"
                    ));
                    Some(None)
                }
            }
        }
        None => {
            if o.ui.interactive()
                && !o.ui.consent(
                    "Keep the OpenRouter key in your configuration? (No asks for a new one.)",
                    true,
                    false,
                )
            {
                return None;
            }
            o.ui.say("  OpenRouter key: the one in your configuration");
            Some(Some(Key {
                reference: KeyRef::Value(kept.clone()),
                check: Some(kept),
            }))
        }
    }
}

/// Where an OpenRouter key is made, and its credits added.
pub(crate) const KEYS_PAGE: &str = "https://openrouter.ai/settings/keys";
pub(crate) const CREDITS_PAGE: &str = "https://openrouter.ai/settings/credits";
/// Where an OpenRouter account's BYOK keys are listed.
pub(crate) const BYOK_PAGE: &str = "https://openrouter.ai/settings/integrations";

/// What BYOK is, said before onboarding asks about it.
fn byok_explained() -> String {
    format!(
        "Native Sessions pay for OpenRouter with your OpenRouter credits, so Axocoatl can count \
         what each call costs. BYOK (\"bring your own key\") is another provider's API key, such \
         as OpenAI's or Anthropic's, added to your OpenRouter account at {BYOK_PAGE}: OpenRouter \
         then sends calls to that provider on your key and the provider bills you, so Axocoatl \
         cannot count the cost, and native Sessions refuse it. A new OpenRouter account has no \
         BYOK keys."
    )
}

/// What `--plan` says about the key, and the key Enter at each question
/// would give.
fn plan_key(o: &Onboarding) -> Key {
    let say = |what: String| {
        o.ui.say(format!(
            "  would {what}, and check it and both models with OpenRouter"
        ))
    };
    let (what, reference) = if let Some(variable) = o.args.openrouter_key_env.clone() {
        (
            format!("read the OpenRouter key from ${variable}"),
            KeyRef::Env(variable),
        )
    } else if let Some(name) = connected(o) {
        (
            format!("use the OpenRouter key you connected (secret {name}, never shown)"),
            KeyRef::Secret(name),
        )
    } else if let Some(kept) = o.kept.openrouter_key.clone() {
        match kept
            .strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
        {
            Some(variable) => (
                format!(
                    "keep reading the OpenRouter key from ${variable}, as your configuration does"
                ),
                KeyRef::Env(variable.to_string()),
            ),
            None => (
                "keep the OpenRouter key in your configuration".to_string(),
                KeyRef::Value(kept),
            ),
        }
    } else if std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        (
            "ask to use the OpenRouter key in $OPENROUTER_API_KEY".to_string(),
            KeyRef::Env("OPENROUTER_API_KEY".to_string()),
        )
    } else {
        (
            format!(
                "offer to sign in to OpenRouter in your browser (the key it issues is stored as \
                 secret {OPENROUTER_SECRET}, never shown), or to read the key from an \
                 environment variable or a hidden prompt"
            ),
            KeyRef::Secret(OPENROUTER_SECRET.to_string()),
        )
    };
    say(what);
    Key {
        reference,
        check: None,
    }
}

/// The key: `--openrouter-key-env VAR`; else a key connected in the
/// browser (the secret the configuration names, or the one `axocoatl
/// connect openrouter` stores), or the key or `${VARIABLE}` an existing
/// configuration has; else `OPENROUTER_API_KEY` when it is set; else, at a
/// terminal, a browser sign-in, an environment variable or a hidden
/// prompt, as the person chooses. `again`: the `axocoatl onboard` command
/// that repeats the models chosen, for the step left without a key;
/// `writer` and `reviewer`: the models a sign-in checks first.
async fn key(o: &mut Onboarding, again: &str, writer: Option<&str>, reviewer: &str) -> Option<Key> {
    if o.ui.plan() {
        return Some(plan_key(o));
    }
    if o.args.openrouter_key_env.is_none() {
        if let Some(name) = o
            .kept
            .openrouter_key_secret
            .clone()
            .filter(|name| !stored(&o.data_dir(), name))
        {
            o.ui.warn(format!(
                "Your configuration reads the OpenRouter key from the secret {}, which is not \
                 stored",
                axocoatl_config::shown_secret_name(&name)
            ));
        }
        if let Some(name) = connected(o) {
            return use_connected(o, &name, again);
        }
        if let Some(kept) = kept_key(o) {
            return kept;
        }
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
                Some(Key {
                    reference: KeyRef::Env(variable),
                    check: Some(zeroize::Zeroizing::new(value.trim().to_string())),
                })
            }
            None => {
                o.step_left(format!(
                    "Export {variable} with your OpenRouter key in this shell, then run `{again} \
                     --openrouter-key-env {variable}`"
                ));
                None
            }
        };
    }
    let declined_kept_variable = o
        .kept
        .openrouter_key
        .as_deref()
        .is_some_and(|kept| kept == "${OPENROUTER_API_KEY}");
    if let Some(value) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty() && !declined_kept_variable)
    {
        if o.ui.consent(
            "Use the OpenRouter key in $OPENROUTER_API_KEY? The configuration names the variable, \
             not the key, so start Axocoatl from a shell that has it.",
            true,
            false,
        ) {
            return Some(Key {
                reference: KeyRef::Env("OPENROUTER_API_KEY".to_string()),
                check: Some(zeroize::Zeroizing::new(value.trim().to_string())),
            });
        }
    }
    if !o.ui.interactive() {
        o.step_left(format!(
            "Sign in to OpenRouter in your browser with `axocoatl connect openrouter`, which \
             stores the key without showing it (add credits at {CREDITS_PAGE}), then run \
             `{again}`"
        ));
        return None;
    }
    let ways = vec![
        "Sign in to OpenRouter in your browser (recommended)".to_string(),
        "Read it from an environment variable (the configuration names the variable, not the \
         key)"
            .to_string(),
        "Enter it at a hidden prompt (stored in your owner-only configuration)".to_string(),
    ];
    match o
        .ui
        .select("How should Axocoatl get your OpenRouter key?", &ways, 0)
    {
        0 => sign_in(o, again, writer, reviewer).await,
        1 => from_variable(o, again),
        _ => from_prompt(o, again),
    }
}

/// The browser sign-in `axocoatl connect openrouter` runs: OpenRouter's
/// sign-in page opened in the browser (its address printed too), the key
/// it issues checked and stored as the secret `openrouter`, never shown.
async fn sign_in(
    o: &mut Onboarding,
    again: &str,
    writer: Option<&str>,
    reviewer: &str,
) -> Option<Key> {
    use crate::connect_cmd::openrouter::{
        connected_lines, onboarding_sign_in, sign_in_for_onboarding, Connected,
    };
    let data_dir = o.data_dir();
    let given = o.openrouter_sign_in.take();
    let result = {
        let mut out = Said::new(&o.ui);
        match given {
            Some((browser, endpoints)) => {
                onboarding_sign_in(
                    &data_dir,
                    writer,
                    Some(reviewer),
                    browser,
                    endpoints,
                    again,
                    &mut out,
                    &crate::connect_cmd::Interrupt::default(),
                )
                .await
            }
            None => {
                sign_in_for_onboarding(&data_dir, writer, Some(reviewer), false, again, &mut out)
                    .await
            }
        }
    };
    match result {
        Ok(connected) => {
            // The models were shown as they were checked.
            let shown = Connected {
                models: Vec::new(),
                ..connected.clone()
            };
            let mut lines = connected_lines(&shown).into_iter();
            if let Some(first) = lines.next() {
                o.ui.ok(first);
            }
            for line in lines {
                o.ui.say(format!("    {line}"));
            }
            Some(Key {
                reference: KeyRef::Secret(connected.secret),
                check: None,
            })
        }
        Err(failure) => {
            let mut message = failure.message.chars();
            let message = message
                .next()
                .map(|first| first.to_uppercase().chain(message).collect::<String>())
                .unwrap_or_default();
            o.step_left(message);
            None
        }
    }
}

/// The key from an environment variable the person names.
fn from_variable(o: &mut Onboarding, again: &str) -> Option<Key> {
    let variable = o.ui.input(
        "Environment variable that holds the key",
        "OPENROUTER_API_KEY",
    );
    if !super::is_variable_name(&variable) {
        o.step_left(format!(
            "{variable:?} is not an environment variable name: run `{again}` again and name one, \
             such as OPENROUTER_API_KEY"
        ));
        return None;
    }
    match std::env::var(&variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(value) => {
            o.ui.say(format!(
                "  OpenRouter key: from ${variable} (the configuration names the variable)"
            ));
            Some(Key {
                reference: KeyRef::Env(variable),
                check: Some(zeroize::Zeroizing::new(value.trim().to_string())),
            })
        }
        None => {
            o.step_left(format!(
                "Export {variable} with your OpenRouter key in this shell, then run `{again} \
                 --openrouter-key-env {variable}`"
            ));
            None
        }
    }
}

/// The key typed at a hidden prompt, stored in the owner-only
/// configuration.
fn from_prompt(o: &mut Onboarding, again: &str) -> Option<Key> {
    o.ui.say(format!(
        "  An OpenRouter API key starts with sk-or-. Create one at {KEYS_PAGE} (sign in or sign \
         up there) and add credits at {CREDITS_PAGE}; press Enter with no key to do it later."
    ));
    match o
        .ui
        .secret("OpenRouter API key (hidden; stored in your owner-only Axocoatl configuration)")
    {
        Some(value) => Some(Key {
            reference: KeyRef::Value(value.clone()),
            check: Some(value),
        }),
        None => {
            o.step_left(format!(
                "Run `{again}` and sign in to OpenRouter in your browser when it asks (add \
                 credits at {CREDITS_PAGE}), or create a key at {KEYS_PAGE} and enter it at its \
                 hidden prompt"
            ));
            None
        }
    }
}

/// What the sign-in prints, said as onboarding's own lines (shown and
/// logged), two spaces in. It never prints the key.
struct Said<'a> {
    ui: &'a Ui,
    line: Vec<u8>,
}

impl<'a> Said<'a> {
    fn new(ui: &'a Ui) -> Self {
        Self {
            ui,
            line: Vec::new(),
        }
    }
}

impl std::io::Write for Said<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.ui.say(format!("  {}", String::from_utf8_lossy(&line)));
            } else {
                self.line.push(byte);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Said<'_> {
    fn drop(&mut self) {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.ui.say(format!("  {}", String::from_utf8_lossy(&line)));
        }
    }
}

/// Step 3D. With `with_writer`, the writer runs on OpenRouter too.
pub(crate) async fn run(o: &mut Onboarding, choice: &mut Choice, with_writer: bool) {
    if o.ui.interactive() {
        o.ui.say(format!("  {}", byok_explained()));
    }
    // The question `axocoatl connect openrouter` asks too.
    if !o
        .ui
        .consent(crate::connect_cmd::openrouter::BYOK_QUESTION, true, false)
    {
        o.step_left(format!(
            "Native Sessions use OpenRouter only with OpenRouter credits and no BYOK keys: remove \
             the BYOK keys at {BYOK_PAGE} (or use another OpenRouter account), then run `axocoatl \
             onboard` again; or choose Claude Code, Codex or a local model there"
        ));
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
    // Nothing is saved without a key: the command that gives the same
    // writer and models again.
    let mut again = vec!["axocoatl onboard".to_string()];
    match (&writer, &choice.writer_model, choice.external_writer) {
        (Some(writer), _, _) => again.push(format!(
            "--writer openrouter --writer-model openrouter:{writer}"
        )),
        (None, Some(model), _) if model.provider == Provider::Ollama => {
            again.push(format!("--writer ollama --writer-model {}", model.spec()))
        }
        // Claude Code or Codex writes; Codex asks again which account pays
        // (onboarding has no flag for it).
        (None, _, Some(external)) => {
            again.push(format!("--writer {}", external.id()));
            if let Some(model) = &choice.external_writer_model {
                again.push(format!(
                    "--writer-model {}",
                    super::host::shell_quote(model)
                ));
            }
        }
        _ => {}
    }
    again.push(format!("--reviewer openrouter:{reviewer}"));
    let again = again.join(" ");
    let Some(key) = key(o, &again, writer.as_deref(), &reviewer).await else {
        return;
    };
    // A sign-in checked the key and both models itself.
    if let Some(value) = key.check.as_ref().filter(|_| !o.ui.plan()) {
        match check_key(&o.openrouter_api, value).await {
            KeyCheck::Accepted => o.ui.ok("OpenRouter accepted the key"),
            KeyCheck::Refused(status) => {
                o.step_left(match &key.reference {
                    KeyRef::Secret(name) => format!(
                        "OpenRouter refused the key you connected (secret {name}, HTTP \
                         {status}): run `{}` to sign in again (it replaces the stored key), \
                         then run `{again}`",
                        connect_command(name)
                    ),
                    _ => format!(
                        "OpenRouter refused the key ({status}): check it at {KEYS_PAGE}, then \
                         run `{again}` in a terminal to enter another (or add \
                         --openrouter-key-env VAR)"
                    ),
                });
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
    choice.openrouter_key = Some(key.reference);
    if let Some(writer) = writer {
        choice.writer_model = Some(ModelRef::new(Provider::OpenRouter, &writer));
    }
    choice.reviewer_model = Some(ModelRef::new(Provider::OpenRouter, &reviewer));
}
