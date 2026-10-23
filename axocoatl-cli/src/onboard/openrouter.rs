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

/// The writer onboarding suggests: Qwen3-Coder 30B-A3B, the model the local
/// default `qwen3-coder:30b` runs, which several OpenRouter providers serve.
pub(crate) const DEFAULT_WRITER: &str = "qwen/qwen3-coder-30b-a3b-instruct";
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

/// Whether Axocoatl's secret store holds the OpenRouter key secret `name`:
/// the check `axocoatl doctor` makes. With `--plan`, read
/// without the store, which makes its directory owner-only before it reads.
fn stored(o: &Onboarding, name: &str) -> bool {
    if o.ui.plan() {
        return axocoatl_daemon::provider_secrets::never_a_provider_key("openrouter", name)
            .is_none()
            && super::stored_secret_names(o)
                .iter()
                .any(|stored| stored == name);
    }
    let credentials = axocoatl_config::ProviderCredentials {
        api_key: Default::default(),
        api_key_secret: Some(name.to_string()),
        base_url: None,
        fallback: None,
    };
    matches!(
        key_source("openrouter", &credentials, &o.data_dir()),
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
    if let Some(name) = &o.kept.openrouter_key_secret {
        if stored(o, name) {
            return Some(name.clone());
        }
    }
    (o.kept.openrouter_key.is_none() && stored(o, OPENROUTER_SECRET))
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
/// cannot be used, at a terminal a new browser sign-in if the person
/// agrees, else the step left.
async fn use_connected(
    o: &mut Onboarding,
    name: &str,
    again: &str,
    writer: Option<&str>,
    reviewer: &str,
) -> Option<Key> {
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
            let problem = format!("The OpenRouter key secret {name} cannot be used ({why})");
            sign_in_again(o, &problem, name, again, writer, reviewer).await
        }
    }
}

/// A key that cannot be used (`problem`): at a terminal, sign in to
/// OpenRouter in the browser again if the person agrees (the key it issues
/// is stored as secret [`OPENROUTER_SECRET`], and the configuration reads it
/// from there); else the step left, with the command that does it.
async fn sign_in_again(
    o: &mut Onboarding,
    problem: &str,
    name: &str,
    again: &str,
    writer: Option<&str>,
    reviewer: &str,
) -> Option<Key> {
    if o.ui.interactive() {
        o.ui.fail(problem);
        if o.ui.consent(
            &format!(
                "Sign in to OpenRouter in your browser now? OpenRouter issues a new key, stored \
                 as secret {OPENROUTER_SECRET} without being shown, and the configuration reads it \
                 from there."
            ),
            true,
            false,
        ) {
            return sign_in(o, again, writer, reviewer).await;
        }
    }
    o.step_left(format!(
        "{problem}: run `{}` to sign in again (it replaces the stored key), then run `{again}`",
        connect_command(name)
    ));
    None
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
            .filter(|name| !stored(o, name))
        {
            o.ui.warn(format!(
                "Your configuration reads the OpenRouter key from the secret {}, which is not \
                 stored",
                axocoatl_config::shown_secret_name(&name)
            ));
        }
        if let Some(name) = connected(o) {
            return use_connected(o, &name, again, writer, reviewer).await;
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
        connected_lines, onboarding_sign_in, sign_in_for_onboarding, Connected, Hooks,
    };
    let data_dir = o.data_dir();
    let given = o.openrouter_sign_in.take();
    let result = {
        // At a terminal, the end of the sign-in time asks whether to wait
        // again; each connection to the sign-in's loopback server goes to
        // the log.
        let ui = &o.ui;
        let host = &o.host;
        let ask = |time: std::time::Duration| {
            ui.consent(
                &crate::connect_cmd::openrouter::keep_waiting_question(time),
                true,
                false,
            )
        };
        let report = |line: &str| host.log(line);
        let hooks = Hooks {
            keep_waiting: ui
                .interactive()
                .then_some(&ask as &(dyn Fn(std::time::Duration) -> bool + Sync)),
            report: Some(&report),
        };
        let mut out = Said::new(ui);
        match given {
            Some((browser, endpoints)) => {
                onboarding_sign_in(
                    &data_dir,
                    writer,
                    Some(reviewer),
                    browser,
                    endpoints,
                    again,
                    hooks,
                    &mut out,
                    &crate::connect_cmd::Interrupt::default(),
                )
                .await
            }
            None => {
                sign_in_for_onboarding(
                    &data_dir,
                    writer,
                    Some(reviewer),
                    false,
                    again,
                    hooks,
                    &mut out,
                )
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
    let Some(mut key) = key(o, &again, writer.as_deref(), &reviewer).await else {
        return;
    };
    // A sign-in checked the key and both models itself.
    if let Some(value) = key.check.clone().filter(|_| !o.ui.plan()) {
        match check_key(&o.openrouter_api, &value).await {
            KeyCheck::Accepted => o.ui.ok("OpenRouter accepted the key"),
            KeyCheck::Refused(status) => {
                let (problem, name) = match &key.reference {
                    KeyRef::Secret(name) => (
                        format!(
                            "OpenRouter refused the key you connected (secret {name}, HTTP \
                             {status})"
                        ),
                        name.clone(),
                    ),
                    _ => (
                        format!("OpenRouter refused the key (HTTP {status})"),
                        OPENROUTER_SECRET.to_string(),
                    ),
                };
                let signed_in = if o.ui.interactive() {
                    sign_in_again(o, &problem, &name, &again, writer.as_deref(), &reviewer).await
                } else {
                    o.step_left(match &key.reference {
                        KeyRef::Secret(name) => format!(
                            "{problem}: run `{}` to sign in again (it replaces the stored key), \
                             then run `{again}`",
                            connect_command(name)
                        ),
                        _ => format!(
                            "{problem}: check it at {KEYS_PAGE}, then run `{again}` in a terminal \
                             to enter another or sign in in your browser (or add \
                             --openrouter-key-env VAR)"
                        ),
                    });
                    None
                };
                let Some(signed_in) = signed_in else {
                    return;
                };
                key = signed_in;
            }
            KeyCheck::Unchecked(why) => o.ui.warn(format!("The key could not be checked: {why}")),
        }
    }
    if key.check.is_some() && !o.ui.plan() {
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
    // Each model's endpoints, as a run admits them: one OpenRouter reports
    // unavailable right now, or none native Sessions can use, is said before
    // anything is written (at a terminal another model can be chosen).
    let value = key_value(o, &key);
    let mut writer = writer;
    if let Some(chosen) = writer.clone() {
        match usable_model(o, "writer", chosen, Some(&reviewer), &value).await {
            Some(model) => writer = Some(model),
            None => return,
        }
    }
    let avoid = writer.clone().or(writer_name);
    let Some(reviewer) = usable_model(o, "reviewer", reviewer, avoid.as_deref(), &value).await
    else {
        return;
    };
    choice.openrouter_key = Some(key.reference);
    if let Some(writer) = writer {
        choice.writer_model = Some(ModelRef::new(Provider::OpenRouter, &writer));
    }
    choice.reviewer_model = Some(ModelRef::new(Provider::OpenRouter, &reviewer));
}

/// The key a run would send, to read the endpoints as it does: the value
/// checked, or the stored secret a sign-in wrote. OpenRouter serves the
/// endpoint list without one too.
fn key_value(o: &Onboarding, key: &Key) -> zeroize::Zeroizing<String> {
    match (&key.check, &key.reference) {
        (Some(value), _) => value.clone(),
        (None, KeyRef::Secret(name)) if !o.ui.plan() => {
            read_stored(&o.data_dir(), name).unwrap_or_default()
        }
        _ => zeroize::Zeroizing::new(String::new()),
    }
}

/// Why `model` cannot run in native Sessions now, from its endpoints, read
/// as a run reads them: `Ok(None)` when it can, `Ok(Some(why))` when it
/// cannot (every endpoint that fits unavailable or degraded right now, or
/// none fits), `Err(why)` when they could not be read.
async fn endpoint_problem(api: &str, key: &str, model: &str) -> Result<Option<String>, String> {
    match axocoatl_llm_openai::observe_native_openrouter_endpoints(api, key, model).await {
        Ok(endpoints) => Ok(endpoints.refusal(model).map(|refusal| match refusal {
            axocoatl_llm::ProviderError::ApiError { message, .. }
            | axocoatl_llm::ProviderError::InvalidRequest { message, .. } => message,
            other => other.to_string(),
        })),
        Err(axocoatl_llm::ProviderError::InvalidRequest { message, .. }) => Ok(Some(message)),
        Err(error) => Err(error.to_string()),
    }
}

/// The OpenRouter models onboarding offers in place of one that cannot run
/// now, the first in this order that can run now: the default writer, other
/// coding models, and the default reviewer.
pub(crate) const SUGGESTED: [&str; 5] = [
    DEFAULT_WRITER,
    "z-ai/glm-4.6",
    "deepseek/deepseek-chat-v3.1",
    "qwen/qwen3-coder",
    DEFAULT_REVIEWER,
];

/// The first of [`SUGGESTED`] that is not `tried` nor `avoid` (the other
/// role's model) and that native Sessions can use now, or `None`.
async fn healthy_suggestion(
    api: &str,
    key: &str,
    tried: &[String],
    avoid: Option<&str>,
) -> Option<&'static str> {
    for candidate in SUGGESTED {
        if tried.iter().any(|tried| tried == candidate) || Some(candidate) == avoid {
            continue;
        }
        if matches!(endpoint_problem(api, key, candidate).await, Ok(None)) {
            return Some(candidate);
        }
    }
    None
}

/// `model` for `role`, once its endpoints show it can run in native
/// Sessions now. When it cannot, at a terminal another model is asked for
/// (Enter takes the first of [`SUGGESTED`] that can run now, never `avoid`,
/// the other role's model); without one, or under `--yes`, the step left
/// says why and names that model.
async fn usable_model(
    o: &mut Onboarding,
    role: &str,
    mut model: String,
    avoid: Option<&str>,
    key: &str,
) -> Option<String> {
    if o.ui.plan() {
        o.ui.say(format!(
            "  would check that OpenRouter has an endpoint for {model} ({role}) that native \
             Sessions can use now"
        ));
        return Some(model);
    }
    let mut tried = vec![model.clone()];
    loop {
        let why = match endpoint_problem(&o.openrouter_api, key, &model).await {
            Ok(None) => {
                o.ui.ok(format!(
                    "OpenRouter has an endpoint for {model} that native Sessions can use now"
                ));
                return Some(model);
            }
            Err(why) => {
                o.ui.warn(format!(
                    "The endpoints of {model} could not be read ({why}); a run checks them again"
                ));
                return Some(model);
            }
            Ok(Some(why)) => why,
        };
        let suggestion = healthy_suggestion(&o.openrouter_api, key, &tried, avoid)
            .await
            .unwrap_or_default();
        if !o.ui.interactive() {
            let flag = if role == "writer" {
                "--writer-model openrouter:VENDOR/MODEL"
            } else {
                "--reviewer openrouter:VENDOR/MODEL"
            };
            o.step_left(format!(
                "The {role} model cannot run now: {why} Run `axocoatl onboard` again later, or \
                 with another {role} model ({flag}{})",
                if suggestion.is_empty() {
                    String::new()
                } else {
                    format!(", such as openrouter:{suggestion}")
                }
            ));
            return None;
        }
        o.ui.fail(format!("The {role} model cannot run now: {why}"));
        let next = o.ui.input(
            &format!("Another model for the {role} (vendor/model; empty stops here)"),
            suggestion,
        );
        let next = next.trim().trim_start_matches("openrouter:").to_string();
        if next.is_empty() || Some(next.as_str()) == avoid || tried.contains(&next) {
            o.step_left(format!(
                "Choose another {role} model for OpenRouter, or run `axocoatl onboard` again \
                 later: {why}"
            ));
            return None;
        }
        tried.push(next.clone());
        model = next;
    }
}
