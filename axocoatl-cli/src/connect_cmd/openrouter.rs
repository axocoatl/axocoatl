//! `axocoatl connect openrouter`: get an OpenRouter API key by signing in in
//! the browser, without anyone seeing, copying, pasting or typing it.
//!
//! It runs OpenRouter's OAuth PKCE flow ([`super::openrouter_pkce`]): a
//! loopback server on `127.0.0.1` and a port the system picks, OpenRouter's
//! sign-in page opened in the browser (its address also printed, and only
//! printed with `--no-browser`), the browser's return checked against this
//! sign-in's state, and the code exchanged with this sign-in's verifier for
//! a new key ([`super::openrouter_api`]). The key is checked with
//! `GET /key`, whose label, credit limit, usage and free-tier status are
//! shown (never the key), stored through the same secret store as
//! `axocoatl secret set` (`openrouter`, `0600`), and the configuration gains
//! `providers.openrouter_billing: credits` and `providers.openrouter` with
//! `api_key_secret: openrouter` ([`super::openrouter_config`]), so the key is
//! never written into it; the daemon reads it from the store when it
//! starts. Models named with `--writer` and `--reviewer` are checked in
//! OpenRouter's public catalog before the sign-in, so a misspelt model
//! never leaves a key behind.
//!
//! Before anything else it asks the question onboarding asks: native
//! Sessions run on OpenRouter credits, with no BYOK provider keys connected.
//! Everything stops, with nothing stored, if the sign-in is cancelled in the
//! browser, does not come back within the sign-in time, or Axocoatl gets a
//! signal. The key is held in zeroizing buffers and never reaches an argv,
//! a log, a URL, a `Debug` string, a file other than the secret, or a
//! message.
//!
//! Onboarding's OpenRouter step calls [`sign_in_for_onboarding`] and writes
//! [`provider_yaml`] under the `providers:` of a configuration it writes new
//! (an existing one is edited as `connect` edits it).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axocoatl_config::shown_secret_name;
use axocoatl_daemon::provider_secrets::{
    how_to_store_key, key_source, never_a_provider_key, KeySource,
};
use axocoatl_daemon::secret_store;

// Onboarding writes these lines in a configuration it writes new, and edits
// an existing one as `connect` does.
pub(crate) use super::openrouter_config::{plan_connect, provider_yaml};
pub(crate) use axocoatl_daemon::provider_secrets::OPENROUTER_SECRET;

use super::browser::Opener;
use super::openrouter_api::{catalog_model, dollars, model_id, Api, ApiError, KeyInfo, ModelRow};
use super::openrouter_config as config_file;
use super::openrouter_pkce::{auth_url, Callback, Pkce, Returned, AUTH_PAGE};
use super::{exit, failure, Failure, Interrupt};
use crate::onboard::ui::fit_prompt;
use crate::user_paths::UserPaths;

/// How long the sign-in may take by default. OpenRouter's codes expire 10
/// minutes after they are issued.
pub(crate) const DEFAULT_SIGN_IN_MINUTES: u64 = 10;

/// How OpenRouter's keys start; `--status` sends nothing else to OpenRouter.
const OPENROUTER_KEY_PREFIX: &str = "sk-or-";

/// Where OpenRouter's account pages are.
const KEYS_PAGE: &str = "https://openrouter.ai/settings/keys";
const CREDITS_PAGE: &str = "https://openrouter.ai/settings/credits";

/// The question onboarding asks before it configures OpenRouter.
pub(crate) const BYOK_QUESTION: &str = "Use OpenRouter credits, with no BYOK provider keys \
     connected? (Native Sessions do not support BYOK)";

/// The answer when it is no.
pub(crate) const BYOK_REFUSED: &str = "Native OpenRouter requires OpenRouter credit billing and \
     does not support BYOK provider keys; nothing was signed in or stored.";

/// OpenRouter's sign-in page and API, or a test's.
pub(crate) struct Endpoints {
    pub(crate) auth_page: String,
    pub(crate) api: Api,
}

impl Endpoints {
    /// <https://openrouter.ai/auth> and <https://openrouter.ai/api/v1>.
    pub(crate) fn openrouter() -> Result<Self, String> {
        Ok(Self {
            auth_page: AUTH_PAGE.to_string(),
            api: Api::openrouter()?,
        })
    }
}

/// How the sign-in page reaches the browser.
pub(crate) enum Browser {
    /// Open it with this computer's opener, and print its address.
    Open(Opener),
    /// Only print its address.
    PrintOnly,
    /// A test's browser: it gets the address.
    #[cfg(test)]
    Fake(Box<dyn Fn(String) + Send + Sync>),
}

impl Browser {
    /// This computer's opener, unless `no_browser` or there is none.
    pub(crate) fn for_this_computer(no_browser: bool) -> Self {
        if no_browser {
            return Self::PrintOnly;
        }
        match Opener::find(std::env::var_os("PATH").as_deref()) {
            Some(opener) => Self::Open(opener),
            None => Self::PrintOnly,
        }
    }
}

/// One sign-in.
pub(crate) struct SignIn<'a> {
    /// The data root the key is stored in.
    pub(crate) data_dir: PathBuf,
    /// The secret's name (default [`OPENROUTER_SECRET`]).
    pub(crate) secret: String,
    /// Models to check in the catalog first: (role, OpenRouter model id).
    pub(crate) models: Vec<(String, String)>,
    pub(crate) browser: Browser,
    /// How long the browser sign-in may take.
    pub(crate) sign_in_time: Duration,
    pub(crate) endpoints: Endpoints,
    /// The command a failure says to run again: `None` for `axocoatl
    /// connect openrouter` (and its `--sign-in-timeout` after a timeout),
    /// or onboarding's own command.
    pub(crate) retry: Option<String>,
    /// At a terminal: asked, when the sign-in time ends without a sign-in,
    /// whether to wait that long again (`true` waits). `None` without a
    /// terminal: the sign-in then stops.
    pub(crate) keep_waiting: Option<&'a (dyn Fn(Duration) -> bool + Sync)>,
    /// Told of each connection to the loopback server, in a line for a log
    /// (never its query: [`Callback::wait_reporting`]).
    pub(crate) report: Option<&'a (dyn Fn(&str) + Sync)>,
}

/// What a sign-in stored, without the key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Connected {
    /// The secret the key is stored as.
    pub(crate) secret: String,
    /// What OpenRouter says about the key, when it could be read.
    pub(crate) key: Option<KeyInfo>,
    /// Why it could not be read.
    pub(crate) key_unread: Option<String>,
    /// The models checked: (role, catalog row).
    pub(crate) models: Vec<(String, ModelRow)>,
}

/// `minutes` as words.
fn minutes(time: Duration) -> String {
    let minutes = time.as_secs().div_ceil(60).max(1);
    format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
}

/// The label OpenRouter gives the new key, to find it among the account's
/// keys.
fn key_label() -> String {
    format!("Axocoatl {}", chrono::Local::now().format("%Y-%m-%d"))
}

/// The models' rows in OpenRouter's catalog, or every reason one cannot be
/// used. Nothing is signed in for this: the catalog is public.
pub(crate) async fn check_models(
    api: &Api,
    models: &[(String, String)],
) -> Result<Vec<(String, ModelRow)>, Failure> {
    if models.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = api.models().await.map_err(|error| {
        failure(
            exit::UNVERIFIED,
            format!(
                "could not read OpenRouter's model catalog to check the models: {}. Nothing was \
                 signed in or stored; check the network and run again.",
                error.describe()
            ),
        )
    })?;
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    for (role, id) in models {
        match catalog_model(&catalog, id) {
            Ok(row) => rows.push((role.clone(), row)),
            Err(reason) => problems.push(format!("{role}: {reason}")),
        }
    }
    if problems.is_empty() {
        Ok(rows)
    } else {
        Err(failure(
            exit::USAGE,
            format!(
                "{}. Nothing was signed in or stored; choose models from \
                 https://openrouter.ai/models and run again.",
                problems.join("; ")
            ),
        ))
    }
}

/// Wait for `future`, unless `interrupt` is raised first.
async fn unless_interrupted<T>(
    interrupt: &Interrupt,
    what: &str,
    future: impl std::future::Future<Output = T>,
) -> Result<T, Failure> {
    tokio::select! {
        value = future => Ok(value),
        () = interrupt.wait() => Err(failure(
            exit::INTERRUPTED,
            format!("interrupted {what}; nothing was stored"),
        )),
    }
}

/// Sign in, get the key, check it and store it. Progress goes to `out`; the
/// key never does. The configuration is not touched.
pub(crate) async fn sign_in(
    request: SignIn<'_>,
    out: &mut (dyn Write + Send),
    interrupt: &Interrupt,
) -> Result<Connected, Failure> {
    let SignIn {
        data_dir,
        secret,
        models,
        browser,
        sign_in_time,
        endpoints,
        retry,
        keep_waiting,
        report,
    } = request;
    let report = report.unwrap_or(&|_| {});
    let command = retry.as_deref().unwrap_or("axocoatl connect openrouter");
    let again = format!("Run `{command}` again to retry.");
    let more_time = match &retry {
        None => "Give it more time with --sign-in-timeout MINUTES.",
        Some(_) => {
            "For more time, sign in first with `axocoatl connect openrouter --sign-in-timeout \
             MINUTES`, then run it."
        }
    };
    // Every refusal the store would make after the sign-in comes now,
    // before a key is made that would then be lost.
    secret_store::check_can_store(&data_dir, &secret).map_err(|error| {
        failure(
            exit::FAILURE,
            format!("{error}; nothing was signed in, so no key was made"),
        )
    })?;
    let rows = unless_interrupted(
        interrupt,
        "while checking the models",
        check_models(&endpoints.api, &models),
    )
    .await??;
    for (role, row) in &rows {
        let _ = writeln!(out, "✓ {role} {}", row.describe());
    }
    let pkce = Pkce::new().map_err(|error| failure(exit::FAILURE, error))?;
    let callback = Callback::bind().await.map_err(|error| {
        failure(
            exit::FAILURE,
            format!("could not listen on 127.0.0.1 for the browser's return: {error}"),
        )
    })?;
    let address = auth_url(&endpoints.auth_page, &callback.url(), &pkce, &key_label());
    let _ = writeln!(
        out,
        "Sign in to OpenRouter in your browser and allow Axocoatl to create a key. The browser \
         then returns to {} on this computer, and Axocoatl stores the key OpenRouter issues as \
         the secret {secret} without showing it.",
        callback.url()
    );
    if !matches!(browser, Browser::PrintOnly) {
        let _ = writeln!(
            out,
            "If no browser opens, open this address in a browser on this computer:\n  {address}"
        );
        let _ = out.flush();
    }
    match &browser {
        Browser::Open(opener) => {
            let opened = opener.open(&address);
            let opened = tokio::task::spawn_blocking(move || {
                opened.recv_timeout(Duration::from_secs(6)).unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            if opened {
                let _ = writeln!(out, "{}", super::browser::OPENED);
            }
        }
        Browser::PrintOnly => {
            let _ = writeln!(
                out,
                "Open this address in a browser on this computer (on another computer, forward \
                 the port first: ssh -L {port}:127.0.0.1:{port} …):\n  {address}",
                port = callback.port()
            );
        }
        #[cfg(test)]
        Browser::Fake(browse) => browse(address.clone()),
    }
    let _ = writeln!(
        out,
        "Waiting up to {} for the sign-in (Ctrl-C stops it)…",
        minutes(sign_in_time)
    );
    let _ = out.flush();
    // At a terminal, the end of the sign-in time asks whether to wait
    // again; the loopback server listens all along, and one wait serves
    // every round, so a return answered just as the time ended is not lost.
    let mut waited = sign_in_time;
    let returned = {
        let wait = callback.wait_reporting(&pkce, report);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                returned = &mut wait => break returned,
                () = tokio::time::sleep(sign_in_time) => {
                    if keep_waiting.is_some_and(|keep_waiting| keep_waiting(sign_in_time)) {
                        let _ = writeln!(
                            out,
                            "Waiting {} more for the sign-in (Ctrl-C stops it)…",
                            minutes(sign_in_time)
                        );
                        let _ = out.flush();
                        waited += sign_in_time;
                        continue;
                    }
                    let ignored = match callback.ignored() {
                        0 => String::new(),
                        1 => " One return to this computer carried a code without this sign-in's \
                              state, so it was not used."
                            .to_string(),
                        count => format!(
                            " {count} returns to this computer carried a code without this \
                             sign-in's state, so they were not used."
                        ),
                    };
                    // Nothing at all came back: the browser never reached
                    // this computer's address.
                    let nothing = if callback.connections() == 0 {
                        format!(" No browser came back to {} in that time.", callback.url())
                    } else {
                        String::new()
                    };
                    return Err(failure(
                        exit::NO_TOKEN,
                        format!(
                            "The OpenRouter sign-in was not completed within {}; nothing was \
                             stored.{nothing}{ignored} {again} {more_time}",
                            minutes(waited)
                        ),
                    ));
                }
                () = interrupt.wait() => {
                    return Err(failure(
                        exit::INTERRUPTED,
                        "interrupted: the OpenRouter sign-in was stopped and nothing was stored",
                    ));
                }
            }
        }
    };
    drop(callback);
    let code = match returned {
        Returned::Code(code) => code,
        Returned::Cancelled(reason) => {
            let said = reason
                .map(|reason| format!(" (OpenRouter said: {reason})"))
                .unwrap_or_default();
            return Err(failure(
                exit::NO_TOKEN,
                format!(
                    "The OpenRouter sign-in was cancelled in the browser{said}, so no key was \
                     made and nothing was stored. {again}"
                ),
            ));
        }
    };
    let _ = writeln!(
        out,
        "The browser returned to Axocoatl. Getting the key from OpenRouter (it is not shown)…"
    );
    let _ = out.flush();
    let api = &endpoints.api;
    let exchanged = unless_interrupted(
        interrupt,
        "while getting the key",
        api.exchange(&code, pkce.verifier()),
    )
    .await?;
    drop(code);
    drop(pkce);
    let key = exchanged.map_err(|error| match error {
        ApiError::Refused { .. } => failure(
            exit::REJECTED,
            format!(
                "OpenRouter did not issue a key for this sign-in ({}); nothing was stored. \
                 {again}",
                error.describe()
            ),
        ),
        _ => failure(
            exit::UNVERIFIED,
            format!(
                "could not get the key from OpenRouter: {}. Nothing was stored. Check the \
                 network and run `{command}` again.",
                error.describe()
            ),
        ),
    })?;
    let checked =
        unless_interrupted(interrupt, "while checking the key", api.key_info(&key)).await?;
    let (info, unread) = match checked {
        Ok(info) => (Some(info), None),
        Err(ApiError::Refused {
            status: status @ (401 | 403),
            ..
        }) => {
            return Err(failure(
                exit::REJECTED,
                format!(
                    "OpenRouter refused the key it had just issued (HTTP {status}), so it was \
                     not stored. {again}"
                ),
            ));
        }
        // The key came from OpenRouter's own exchange a moment ago: it is
        // stored, and what could not be read about it is said.
        Err(error) => (None, Some(error.describe())),
    };
    secret_store::set_secret(&data_dir, &secret, key.expose().as_bytes())
        .map_err(|error| failure(exit::FAILURE, format!("storing the key: {error}")))?;
    drop(key);
    Ok(Connected {
        secret,
        key: info,
        key_unread: unread,
        models: rows,
    })
}

/// What onboarding's sign-in asks and tells: whether to keep waiting at
/// the end of the sign-in time (at a terminal), and each connection to the
/// loopback server, for its log.
#[derive(Clone, Copy, Default)]
pub(crate) struct Hooks<'a> {
    pub(crate) keep_waiting: Option<&'a (dyn Fn(Duration) -> bool + Sync)>,
    pub(crate) report: Option<&'a (dyn Fn(&str) + Sync)>,
}

/// Onboarding's OpenRouter step (Step 3D): check `writer` and `reviewer`
/// (OpenRouter model ids, `vendor/model` or `openrouter:vendor/model`;
/// either may be absent) in OpenRouter's catalog, sign in in the browser
/// (opened with this computer's opener unless `no_browser`, the address
/// printed either way), and store the key as the secret
/// [`OPENROUTER_SECRET`] in `data_dir` (created if needed). Progress goes
/// to `out`, never the key; a failure says to run `retry` (onboarding's
/// command) again. It does not ask the BYOK question (onboarding asks it
/// first) and writes no configuration: the configuration onboarding writes
/// names the secret in `api_key_secret`, with `openrouter_billing: credits`
/// ([`provider_yaml`]`(&connected.secret)`); show [`connected_lines`].
/// Every [`Failure`] says that nothing was stored. `hooks` asks, at a
/// terminal, whether to keep waiting when the sign-in time ends, and takes
/// each connection to the loopback server for onboarding's log.
///
/// It installs no signal handling, so the process keeps its own: Ctrl-C,
/// `SIGTERM` or a hang-up during the sign-in ends onboarding as it would at
/// any other step, with nothing stored (the key is stored last, in one
/// atomic write). A caller that handles signals itself passes its own
/// [`Interrupt`] to [`onboarding_sign_in`] and raises it instead; the
/// sign-in then stops with [`exit::INTERRUPTED`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sign_in_for_onboarding(
    data_dir: &Path,
    writer: Option<&str>,
    reviewer: Option<&str>,
    no_browser: bool,
    retry: &str,
    hooks: Hooks<'_>,
    out: &mut (dyn Write + Send),
) -> Result<Connected, Failure> {
    let endpoints = Endpoints::openrouter().map_err(|error| failure(exit::FAILURE, error))?;
    // Never raised: signals keep the process's own handling.
    let interrupt = Interrupt::default();
    onboarding_sign_in(
        data_dir,
        writer,
        reviewer,
        Browser::for_this_computer(no_browser),
        endpoints,
        retry,
        hooks,
        out,
        &interrupt,
    )
    .await
}

/// [`sign_in_for_onboarding`] with its browser, endpoints and
/// `interrupt`, which the caller raises to stop the sign-in.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn onboarding_sign_in(
    data_dir: &Path,
    writer: Option<&str>,
    reviewer: Option<&str>,
    browser: Browser,
    endpoints: Endpoints,
    retry: &str,
    hooks: Hooks<'_>,
    out: &mut (dyn Write + Send),
    interrupt: &Interrupt,
) -> Result<Connected, Failure> {
    let models = chosen_models(writer, reviewer)?;
    axocoatl_daemon::AxocoatlDaemon::initialize_data_root(data_dir).map_err(|error| {
        failure(
            exit::FAILURE,
            format!("opening the data directory: {error}"),
        )
    })?;
    sign_in(
        SignIn {
            data_dir: data_dir.to_path_buf(),
            secret: OPENROUTER_SECRET.to_string(),
            models,
            browser,
            sign_in_time: Duration::from_secs(DEFAULT_SIGN_IN_MINUTES * 60),
            endpoints,
            retry: Some(retry.to_string()),
            keep_waiting: hooks.keep_waiting,
            report: hooks.report,
        },
        out,
        interrupt,
    )
    .await
}

/// The (role, model id) pairs to check: each given model in OpenRouter's
/// form, never the same model for both.
pub(crate) fn chosen_models(
    writer: Option<&str>,
    reviewer: Option<&str>,
) -> Result<Vec<(String, String)>, Failure> {
    let mut models = Vec::new();
    for (role, flag, given) in [
        ("writer", "--writer", writer),
        ("reviewer", "--reviewer", reviewer),
    ] {
        if let Some(given) = given {
            let id = model_id(flag, given).map_err(|error| failure(exit::USAGE, error))?;
            models.push((role.to_string(), id));
        }
    }
    if let [(_, writer), (_, reviewer)] = models.as_slice() {
        if writer == reviewer {
            return Err(failure(
                exit::USAGE,
                format!(
                    "the writer and the reviewer are both {writer}: the reviewer must be a \
                     different model, so it does not share the writer's blind spots"
                ),
            ));
        }
    }
    Ok(models)
}

/// What OpenRouter says about the key, one line each.
pub(crate) fn key_lines(info: &KeyInfo) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(label) = &info.label {
        lines.push(format!("Key label: {label}"));
    }
    lines.push(match (info.limit, info.limit_remaining) {
        (None, _) => "Credit limit: none on this key (the account's credits apply)".to_string(),
        (Some(limit), remaining) => {
            let left = remaining
                .map(|remaining| format!(", ${} left", dollars(remaining)))
                .unwrap_or_default();
            let reset = info
                .limit_reset
                .as_deref()
                .map(|reset| format!(" (resets {reset})"))
                .unwrap_or_default();
            format!("Credit limit: ${} on this key{left}{reset}", dollars(limit))
        }
    });
    if let Some(usage) = info.usage {
        let today = info
            .usage_daily
            .map(|daily| format!(", ${} today (UTC)", dollars(daily)))
            .unwrap_or_default();
        lines.push(format!(
            "Used by this key: ${} in all{today}",
            dollars(usage)
        ));
    }
    match info.is_free_tier {
        Some(true) => {
            let free = match (info.free_requests_used, info.free_requests_limit) {
                (Some(used), Some(limit)) => format!(" ({used} of {limit} used today)"),
                _ => String::new(),
            };
            lines.push(format!(
                "Free tier: yes, this account has not bought credits. Requests to paid models \
                 fail until you add credits at {CREDITS_PAGE}; `:free` models have a daily \
                 request limit{free}."
            ));
        }
        Some(false) => lines.push("Free tier: no (the account has bought credits)".to_string()),
        None => {}
    }
    lines
}

/// The lines that say what was stored and what OpenRouter says about it.
pub(crate) fn connected_lines(connected: &Connected) -> Vec<String> {
    let mut lines = vec![format!(
        "Connected OpenRouter: stored the key as secret {} (it was never shown)",
        connected.secret
    )];
    match (&connected.key, &connected.key_unread) {
        (Some(info), _) => lines.extend(key_lines(info)),
        (None, Some(reason)) => lines.push(format!(
            "Could not read the key's limits from OpenRouter ({reason}); see them with \
             `axocoatl connect openrouter --status`"
        )),
        (None, None) => {}
    }
    for (role, row) in &connected.models {
        lines.push(format!("{role} {}", row.describe()));
    }
    lines
}

/// The writer to suggest with an OpenRouter reviewer, when no writer was
/// named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriterHint {
    /// The configuration's Lead model (`provider:model`).
    Configured(String),
    /// A loadout whose external writer has a stored sign-in
    /// (`fix-claude-code`, `fix-codex`).
    External(&'static str),
    /// Nothing says which.
    Unknown,
}

/// The writer to suggest with the reviewer `reviewer` (an OpenRouter model
/// id): the configuration's Lead model when it runs natively and is not
/// the reviewer, else a loadout whose writer's sign-in is stored in
/// `data_dir`.
pub(crate) fn writer_hint(config: Option<&str>, data_dir: &Path, reviewer: &str) -> WriterHint {
    if let Some(writer) = config.and_then(config_file::lead_writer) {
        if writer != format!("openrouter:{reviewer}") {
            return WriterHint::Configured(writer);
        }
    }
    let stored = |name: &str| secret_store::has_secret(data_dir, name).unwrap_or(false);
    use axocoatl_daemon::external_agent::{claude_code, codex};
    if stored(claude_code::CLAUDE_CODE_SECRET) {
        WriterHint::External("fix-claude-code")
    } else if stored(codex::CODEX_CHATGPT_SECRET) || stored(codex::CODEX_SECRET) {
        WriterHint::External("fix-codex")
    } else {
        WriterHint::Unknown
    }
}

/// What to run next with the models checked: a sentence, and a command
/// that runs as written when there is one.
pub(crate) fn next_lines(models: &[(String, ModelRow)], hint: &WriterHint) -> Vec<String> {
    let find = |wanted: &str| {
        models
            .iter()
            .find(|(role, _)| role == wanted)
            .map(|(_, row)| row.id.as_str())
    };
    match (find("writer"), find("reviewer")) {
        (Some(writer), Some(reviewer)) => vec![
            "Next, in your repository:".to_string(),
            format!(
                "  axocoatl run fix --task \"Describe the bug\" --model \
                 writer=openrouter:{writer} --model reviewer=openrouter:{reviewer}"
            ),
        ],
        (None, Some(reviewer)) => match hint {
            WriterHint::Configured(writer) => vec![
                format!(
                    "Next, in your repository, with your configuration's Lead model ({writer}) \
                     as the writer and OpenRouter as the reviewer:"
                ),
                format!(
                    "  axocoatl run fix --task \"Describe the bug\" --model writer={writer} \
                     --model reviewer=openrouter:{reviewer}"
                ),
            ],
            WriterHint::External(loadout) => vec![
                "Next, in your repository, with OpenRouter as the reviewer:".to_string(),
                format!(
                    "  axocoatl run {loadout} --task \"Describe the bug\" --model \
                     reviewer=openrouter:{reviewer}"
                ),
            ],
            WriterHint::Unknown => vec![
                "Next, in your repository, choose a writer on another model (ollama:MODEL, or \
                 openrouter:VENDOR/MODEL for another OpenRouter model):"
                    .to_string(),
                format!(
                    "  axocoatl run fix --task \"Describe the bug\" --model \
                     writer=PROVIDER:MODEL --model reviewer=openrouter:{reviewer}"
                ),
                "(`axocoatl run fix-claude-code` and `fix-codex` take the same --model \
                 reviewer=… after `axocoatl connect claude-code` or `axocoatl connect codex`)"
                    .to_string(),
            ],
        },
        (Some(writer), None) => vec![format!(
            "Next: run with --model writer=openrouter:{writer} and a reviewer on a different \
             model, --model reviewer=PROVIDER:MODEL (connect it with `axocoatl connect \
             openrouter --reviewer VENDOR/MODEL` for one on OpenRouter)"
        )],
        (None, None) => vec![
            "Next: name OpenRouter models as openrouter:VENDOR/MODEL in --model writer=… or \
             --model reviewer=… (models: https://openrouter.ai/models)"
                .to_string(),
        ],
    }
}

/// `doctor`'s line for a hosted provider's key (`name` is its display
/// name: OpenRouter, OpenAI, …) when it is a stored secret, or OpenRouter's
/// when it has none: `Ok` for a pass, `Err((line, hint))` for a warning.
/// `None` leaves the check to doctor's own.
pub(crate) fn doctor_key_line(
    name: &str,
    credentials: &axocoatl_config::ProviderCredentials,
    data_dir: &Path,
) -> Option<Result<String, (String, String)>> {
    let provider = name.to_ascii_lowercase();
    match key_source(&provider, credentials, data_dir) {
        KeySource::Secret {
            name: secret,
            stored: true,
        } => Some(Ok(format!(
            "{name} API key present (secret {}, never shown)",
            shown_secret_name(&secret)
        ))),
        KeySource::Secret {
            name: secret,
            stored: false,
        } => Some(Err((
            format!(
                "{name} API key secret {} is not stored",
                shown_secret_name(&secret)
            ),
            capitalized(&how_to_store_key(&provider, &secret)),
        ))),
        KeySource::Missing if provider == "openrouter" => Some(Err((
            "OpenRouter API key not set".to_string(),
            capitalized(&how_to_store_key("openrouter", OPENROUTER_SECRET)),
        ))),
        KeySource::Missing | KeySource::Config => None,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The refusal of a value on `connect openrouter`'s command line, which
/// never repeats it.
pub(crate) const VALUE_ON_COMMAND_LINE: &str =
    "axocoatl connect openrouter takes no key, token or other value on its command line, so \
     nothing was run or stored (the value is not repeated here). You never type or paste a key: \
     `axocoatl connect openrouter` signs you in to OpenRouter in the browser and stores the key \
     OpenRouter issues, without showing it.";

/// The flags of `axocoatl connect openrouter`, as `cmd_connect` parsed them.
pub(crate) struct Flags {
    pub(crate) config: PathBuf,
    pub(crate) secret: String,
    pub(crate) writer: Option<String>,
    pub(crate) reviewer: Option<String>,
    pub(crate) no_browser: bool,
    pub(crate) sign_in_timeout: Option<u64>,
    pub(crate) credits_only: bool,
    pub(crate) no_config: bool,
    pub(crate) status: bool,
    pub(crate) remove: bool,
}

/// What `connect` does to the configuration file.
pub(crate) enum ConfigPlan {
    /// Edit it (or leave it, when it already says this).
    Edit(config_file::Planned),
    /// There is none at the user's configuration path: write
    /// [`config_file::new_configuration`] there.
    Create(UserPaths),
}

/// The configuration file's planned change, checked before the sign-in.
/// A missing file is created only at the user's configuration path
/// (`creatable`), where `axocoatl onboard` writes it.
fn plan_config(
    path: &Path,
    secret: &str,
    creatable: Option<&UserPaths>,
) -> Result<ConfigPlan, Failure> {
    let manual = config_file::manual_lines(secret);
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(paths) = creatable.filter(|paths| paths.config_path == path) {
                return Ok(ConfigPlan::Create(paths.clone()));
            }
            return Err(failure(
                exit::USAGE,
                format!(
                    "there is no configuration file at {}. Pass an existing configuration with \
                     -c (without -c, Axocoatl uses the one at its usual place, and writes one \
                     there when there is none), or run again with --no-config to only store \
                     the key. Nothing was signed in or stored.",
                    path.display()
                ),
            ));
        }
        Err(error) => {
            return Err(failure(
                exit::FAILURE,
                format!(
                    "reading {}: {error}; nothing was signed in or stored",
                    path.display()
                ),
            ));
        }
    };
    config_file::check_writable(path)
        .and_then(|()| config_file::plan_connect(&text, secret))
        .map(ConfigPlan::Edit)
        .map_err(|reason| {
            failure(
                exit::USAGE,
                format!(
                    "{} cannot be updated by this command: {reason}. Nothing was signed in or \
                     stored. Make its providers: hold these entries yourself (they hold no key) \
                     and run again, or run again with --no-config to only store the key:\n{}",
                    path.display(),
                    manual.trim_end()
                ),
            )
        })
}

/// The user's configuration path, when `config` is it: a missing file there
/// is written by `connect`.
fn creatable(config: &Path) -> Option<UserPaths> {
    UserPaths::discover()
        .ok()
        .filter(|paths| paths.config_path == config)
}

/// `axocoatl connect openrouter`. Returns the process exit code.
pub(crate) async fn cmd_connect_openrouter(flags: Flags) -> i32 {
    if !axocoatl_config::egress_routes::is_valid_credential_name(&flags.secret) {
        eprintln!("✗ --secret: {}", secret_store::INVALID_NAME);
        return exit::USAGE;
    }
    if axocoatl_config::may_be_a_key(&flags.secret) {
        eprintln!(
            "✗ --secret takes the name of the secret to store the key as, and the value given \
             reads as a key, so it is not repeated and nothing was run, stored or removed. You \
             never type or paste a key: leave --secret out to store it as {OPENROUTER_SECRET}."
        );
        return exit::USAGE;
    }
    // Storing or removing a key there would replace or delete that secret,
    // and --status would send it to OpenRouter.
    if let Some(reason) = never_a_provider_key("openrouter", &flags.secret) {
        eprintln!(
            "✗ --secret {}: that secret never holds a provider key: {reason}. Nothing was run, \
             stored or removed.",
            flags.secret
        );
        return exit::USAGE;
    }
    if flags.status {
        return cmd_status(&flags.config, &flags.secret).await;
    }
    if flags.remove {
        return cmd_remove(&flags.config, &flags.secret, flags.no_config);
    }
    let models = match chosen_models(flags.writer.as_deref(), flags.reviewer.as_deref()) {
        Ok(models) => models,
        Err(Failure { code, message }) => {
            eprintln!("✗ {message}");
            return code;
        }
    };
    let creatable = creatable(&flags.config);
    if !flags.no_config {
        if let Err(Failure { code, message }) =
            plan_config(&flags.config, &flags.secret, creatable.as_ref())
        {
            eprintln!("✗ {message}");
            return code;
        }
    }
    let data_dir = match crate::secret_cmd::data_dir(&flags.config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    if !flags.credits_only {
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "✗ axocoatl connect openrouter asks whether this OpenRouter account pays with \
                 credits and has no BYOK provider keys connected, and needs a terminal for that. \
                 Run it in a terminal, or add --credits-only to answer yes. Nothing was signed \
                 in or stored."
            );
            return exit::USAGE;
        }
        let credits = dialoguer::Confirm::new()
            .with_prompt(fit_prompt(BYOK_QUESTION, 7))
            .default(true)
            .interact()
            .unwrap_or(false);
        if !credits {
            eprintln!("✗ {BYOK_REFUSED}");
            return exit::USAGE;
        }
    }
    let endpoints = match Endpoints::openrouter() {
        Ok(endpoints) => endpoints,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    let interrupt = Interrupt::default();
    let listeners = interrupt.listen();
    let browser = Browser::for_this_computer(flags.no_browser);
    // What reaches the sign-in's loopback server goes to a log, so a
    // sign-in that never completes can be diagnosed; at a terminal, the end
    // of the sign-in time asks whether to wait again.
    let log = SignInLog::new(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .map(|home| {
                crate::onboard::host::log_directory(
                    &home,
                    cfg!(target_os = "macos"),
                    std::env::var_os("XDG_STATE_HOME")
                        .map(PathBuf::from)
                        .as_deref(),
                )
            }),
    );
    let report = |line: &str| log.line(line);
    let ask = |time: Duration| keep_waiting_at_the_terminal(time);
    let hooks = Hooks {
        keep_waiting: std::io::stdin()
            .is_terminal()
            .then_some(&ask as &(dyn Fn(Duration) -> bool + Sync)),
        report: Some(&report),
    };
    let (code, mut text) = connect_and_configure(
        &flags,
        creatable.as_ref(),
        data_dir,
        models,
        browser,
        endpoints,
        hooks,
        &mut std::io::stderr(),
        &interrupt,
    )
    .await;
    for listener in listeners {
        listener.abort();
    }
    if code == exit::CONNECTED {
        println!("\n{text}");
    } else {
        if let Some(path) = log.path() {
            text.push_str(&log_hint(path));
        }
        eprintln!("{text}");
    }
    code
}

/// The log of `axocoatl connect openrouter`'s sign-in:
/// `connect-openrouter-TIME.log` in its directory, made with its first line
/// (a connection to the sign-in's loopback server), so a run that ends
/// before any connection leaves no log and names none.
pub(crate) struct SignInLog {
    directory: Option<PathBuf>,
    stamp: String,
    log: std::sync::OnceLock<Option<crate::onboard::host::Log>>,
}

impl SignInLog {
    /// A log in `directory` (`None`: no log is kept).
    pub(crate) fn new(directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            stamp: crate::onboard::host::stamp(),
            log: std::sync::OnceLock::new(),
        }
    }

    /// Log `text`, making the log first.
    pub(crate) fn line(&self, text: &str) {
        let Some(directory) = &self.directory else {
            return;
        };
        let log = self.log.get_or_init(|| {
            crate::onboard::host::Log::create_named(directory, "connect-openrouter", &self.stamp)
                .ok()
        });
        if let Some(log) = log {
            log.line(text);
        }
    }

    /// The log's path, once a line is in it.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.log.get()?.as_ref().map(|log| log.path())
    }
}

/// What a failed `axocoatl connect openrouter` adds when its log holds a
/// line.
pub(crate) fn log_hint(path: &Path) -> String {
    format!(
        "\n  Each connection to the sign-in's address on this computer is logged in {} (never a \
         code)",
        path.display()
    )
}

/// The question at the end of the sign-in time, at a terminal: Enter waits
/// that long again.
pub(crate) fn keep_waiting_question(time: Duration) -> String {
    format!(
        "No sign-in came back within {}. Keep waiting {} more? (No stops the sign-in; nothing \
         is stored.)",
        minutes(time),
        minutes(time)
    )
}

fn keep_waiting_at_the_terminal(time: Duration) -> bool {
    dialoguer::Confirm::new()
        .with_prompt(fit_prompt(&keep_waiting_question(time), 7))
        .default(true)
        .interact()
        .unwrap_or(false)
}

/// Sign in, store the key, and update the configuration (unless
/// `--no-config`; written new at `creatable` when it has no file there):
/// the exit code and what to print.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect_and_configure(
    flags: &Flags,
    creatable: Option<&UserPaths>,
    data_dir: PathBuf,
    models: Vec<(String, String)>,
    browser: Browser,
    endpoints: Endpoints,
    hooks: Hooks<'_>,
    out: &mut (dyn Write + Send),
    interrupt: &Interrupt,
) -> (i32, String) {
    let result = sign_in(
        SignIn {
            data_dir: data_dir.clone(),
            secret: flags.secret.clone(),
            models,
            browser,
            sign_in_time: Duration::from_secs(
                flags.sign_in_timeout.unwrap_or(DEFAULT_SIGN_IN_MINUTES) * 60,
            ),
            endpoints,
            retry: None,
            keep_waiting: hooks.keep_waiting,
            report: hooks.report,
        },
        out,
        interrupt,
    )
    .await;
    let connected = match result {
        Ok(connected) => connected,
        Err(Failure { code, message }) => return (code, format!("✗ {message}")),
    };
    let mut lines = connected_lines(&connected);
    let mut code = exit::CONNECTED;
    if flags.no_config {
        lines.push(format!(
            "The configuration was not changed (--no-config). To use the key, make its \
             providers: hold these entries (they hold no key):\n{}",
            config_file::manual_lines(&connected.secret).trim_end()
        ));
    } else {
        // Planned again: the file may have changed during the sign-in.
        let written = plan_config(&flags.config, &connected.secret, creatable).and_then(|plan| {
            match &plan {
                ConfigPlan::Edit(planned) if planned.unchanged => {}
                ConfigPlan::Edit(planned) => config_file::write(&flags.config, &planned.text)
                    .map_err(|error| failure(exit::FAILURE, error))?,
                ConfigPlan::Create(paths) => {
                    config_file::create(paths, &config_file::new_configuration(&connected.secret))
                        .map_err(|error| failure(exit::FAILURE, error))?
                }
            }
            Ok(plan)
        });
        match written {
            Ok(ConfigPlan::Edit(planned)) => {
                lines.push(if planned.unchanged {
                    format!(
                        "{} already reads the key from secret {} (openrouter_billing: credits)",
                        flags.config.display(),
                        connected.secret
                    )
                } else {
                    format!(
                        "Updated {}: providers.openrouter reads the key from secret {}, and \
                         openrouter_billing: credits",
                        flags.config.display(),
                        connected.secret
                    )
                });
                if planned.removed_key {
                    lines.push(format!(
                        "The key that was written in the file was removed from it; it still \
                         works in your OpenRouter account until you delete it at {KEYS_PAGE}"
                    ));
                }
            }
            Ok(ConfigPlan::Create(paths)) => lines.push(format!(
                "Wrote a new configuration, {} (0600): providers.openrouter reads the key from \
                 secret {}, with openrouter_billing: credits, and the server's defaults. It has \
                 no Agents: `axocoatl run` takes its models with --model, and `axocoatl \
                 onboard` adds a team for workbench Sessions",
                paths.config_path.display(),
                connected.secret
            )),
            Err(Failure { message, .. }) => {
                code = exit::FAILURE;
                lines.push(format!(
                    "The key is stored, but the configuration was not updated: {message}"
                ));
            }
        }
    }
    lines.push(
        "Axocoatl reads provider keys when it starts: start it with `axocoatl dev` (or \
         `axocoatl service start` for the always-on service), or, if it is running, restart it \
         (`axocoatl service stop` then `axocoatl service start`, or stop `axocoatl dev` and \
         start it again)"
            .to_string(),
    );
    let reviewer_only = connected.models.iter().all(|(role, _)| role == "reviewer");
    let hint = match connected.models.iter().find(|(role, _)| role == "reviewer") {
        Some((_, reviewer)) if reviewer_only => writer_hint(
            std::fs::read_to_string(&flags.config).ok().as_deref(),
            &data_dir,
            &reviewer.id,
        ),
        _ => WriterHint::Unknown,
    };
    lines.extend(next_lines(&connected.models, &hint));
    let mark = if code == exit::CONNECTED { '✓' } else { '!' };
    let text = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("{mark} {line}")
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    (code, text)
}

/// What the configuration says about OpenRouter's key, in words.
pub(crate) fn configured_key(path: &Path, secret: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("{}: no configuration file", path.display());
    };
    let Ok(document) = axocoatl_config::yaml::from_str::<axocoatl_config::yaml::Value>(&text)
    else {
        return format!("{}: not valid YAML", path.display());
    };
    use axocoatl_config::yaml::Value;
    let providers = document.get("providers");
    let openrouter = providers.and_then(|providers| providers.get("openrouter"));
    let billing = match providers.and_then(|providers| providers.get("openrouter_billing")) {
        Some(Value::String(billing)) => format!("openrouter_billing: {billing}"),
        _ => "no openrouter_billing".to_string(),
    };
    let key = match (
        openrouter.and_then(|openrouter| openrouter.get("api_key_secret")),
        openrouter.and_then(|openrouter| openrouter.get("api_key")),
    ) {
        (Some(Value::String(named)), _) if named == secret => {
            format!("providers.openrouter reads the key from secret {secret}")
        }
        (Some(Value::String(named)), _) if shown_secret_name(named) != named => {
            "providers.openrouter's api_key_secret holds something that is not a secret's name \
             (it is not shown, in case it is a key): write a name there, never the key, and \
             delete a key written there from the file and its backups"
                .to_string()
        }
        (Some(Value::String(named)), _) => format!(
            "providers.openrouter reads the key from secret {named}, not {secret} (check \
             that one with --secret {named})"
        ),
        (_, Some(Value::String(key))) if key.starts_with("${") => {
            "providers.openrouter takes the key from an environment variable; run `axocoatl \
             connect openrouter` to sign in instead"
                .to_string()
        }
        (_, Some(Value::String(key))) if !key.is_empty() => "providers.openrouter has a key \
             written in the file; run `axocoatl connect openrouter` to replace it with a \
             browser sign-in stored as a secret"
            .to_string(),
        _ if openrouter.is_some() => "providers.openrouter names no key".to_string(),
        _ => "no providers.openrouter; run `axocoatl connect openrouter`".to_string(),
    };
    format!("{}: {key}; {billing}", path.display())
}

/// `axocoatl connect openrouter --status`. Exit codes: 0 stored and
/// accepted, 1 OpenRouter refuses it, 2 none stored, 4 it could not be
/// checked, 5 failure.
async fn cmd_status(config: &Path, secret: &str) -> i32 {
    let data_dir = match crate::configure_data_dir(config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    let api = match Api::openrouter() {
        Ok(api) => api,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    let (code, lines) = status_lines(&data_dir, secret, &api).await;
    let configured = configured_key(config, secret);
    let text = std::iter::once(configured)
        .chain(lines)
        .collect::<Vec<_>>()
        .join("\n  ");
    if code == exit::CONNECTED {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    code
}

/// `--status`'s exit code and lines about the stored key.
pub(crate) async fn status_lines(data_dir: &Path, secret: &str, api: &Api) -> (i32, Vec<String>) {
    let stored = if data_dir.exists() {
        secret_store::lock_secrets(data_dir, false).and_then(|lock| match lock {
            Some(lock) => lock.read(secret),
            None => Ok(None),
        })
    } else {
        // No data root yet: nothing was ever stored.
        Ok(None)
    };
    let key = match stored {
        Ok(Some(value)) => match String::from_utf8(value.to_vec()) {
            // Only an OpenRouter key is sent to OpenRouter: a secret that
            // holds anything else stays here.
            Ok(text) if text.starts_with(OPENROUTER_KEY_PREFIX) => {
                super::openrouter_api::ApiKey::from_stored(text)
            }
            Ok(text) => {
                drop(zeroize::Zeroizing::new(text));
                return (
                    exit::FAILURE,
                    vec![format!(
                        "secret {secret} does not hold an OpenRouter key (they start with \
                         {OPENROUTER_KEY_PREFIX}), so it was not sent to OpenRouter; run \
                         `axocoatl connect openrouter --secret NAME` with another name"
                    )],
                );
            }
            Err(_) => {
                return (
                    exit::FAILURE,
                    vec![format!("secret {secret} is stored but is not text")],
                )
            }
        },
        Ok(None) => {
            return (
                exit::NO_TOKEN,
                vec![format!(
                    "No OpenRouter key is stored as secret {secret}; run `axocoatl connect \
                     openrouter`"
                )],
            )
        }
        Err(error) => {
            return (
                exit::FAILURE,
                vec![format!("reading secret {secret}: {error}")],
            )
        }
    };
    let mut lines = vec![format!(
        "OpenRouter key: stored as secret {secret} (never shown)"
    )];
    match api.key_info(&key).await {
        Ok(info) => {
            lines.extend(key_lines(&info));
            (exit::CONNECTED, lines)
        }
        Err(ApiError::Refused {
            status: status @ (401 | 403),
            ..
        }) => {
            lines.push(format!(
                "OpenRouter refuses this key (HTTP {status}): it was deleted or disabled. Run \
                 `axocoatl connect openrouter` to sign in again"
            ));
            (exit::REJECTED, lines)
        }
        Err(error) => {
            lines.push(format!(
                "Could not check it with OpenRouter: {}",
                error.describe()
            ));
            (exit::UNVERIFIED, lines)
        }
    }
}

/// `axocoatl connect openrouter --remove`: remove the stored key and the
/// configuration's reference to it. Exit codes: 0 removed, 2 nothing to
/// remove, 5 failure.
fn cmd_remove(config: &Path, secret: &str, no_config: bool) -> i32 {
    let data_dir = match crate::configure_data_dir(config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    let (code, text) = remove(&data_dir, config, secret, no_config);
    if code == exit::CONNECTED {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    code
}

/// `--remove` with its paths: the exit code and what to print.
pub(crate) fn remove(
    data_dir: &Path,
    config: &Path,
    secret: &str,
    no_config: bool,
) -> (i32, String) {
    let mut lines = Vec::new();
    let mut failed = false;
    // No data root yet: nothing was ever stored.
    let removed = if data_dir.exists() {
        secret_store::remove_secret(data_dir, secret)
    } else {
        Ok(false)
    };
    let removed_secret = match removed {
        Ok(removed) => removed,
        Err(error) => {
            failed = true;
            lines.push(format!("✗ removing secret {secret}: {error}"));
            false
        }
    };
    if removed_secret {
        lines.push(format!(
            "✓ Removed the stored OpenRouter key (secret {secret}). It still exists in your \
             OpenRouter account until you delete it at {KEYS_PAGE}"
        ));
    }
    let mut changed_config = false;
    if !no_config {
        match std::fs::read_to_string(config) {
            // Agents still on OpenRouter: the configuration keeps naming
            // the secret, so `doctor` says it is not stored and how to
            // connect again, rather than the provider vanishing.
            Ok(text)
                if !config_file::openrouter_agents(&text).is_empty()
                    && matches!(config_file::plan_remove(&text, secret), Ok(Some(_))) =>
            {
                let agents = config_file::openrouter_agents(&text).join(", ");
                lines.push(format!(
                    "! Kept providers.openrouter's api_key_secret: {secret} in {}: its Agents \
                     {agents} use provider openrouter, so Axocoatl will not start with this \
                     configuration until you run `axocoatl connect openrouter` again or move \
                     them to another provider (`axocoatl doctor` warns until then)",
                    config.display()
                ));
            }
            Ok(text) => match config_file::plan_remove(&text, secret) {
                Ok(Some(new)) => match config_file::write(config, &new) {
                    Ok(()) => {
                        changed_config = true;
                        lines.push(format!(
                            "✓ Removed providers.openrouter's api_key_secret: {secret} from {}",
                            config.display()
                        ));
                    }
                    Err(error) => {
                        failed = true;
                        lines.push(format!("✗ {error}"));
                    }
                },
                Ok(None) => {}
                Err(reason) => {
                    failed = true;
                    lines.push(format!("✗ {}: {reason}", config.display()));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                failed = true;
                lines.push(format!("✗ reading {}: {error}", config.display()));
            }
        }
    }
    if failed {
        return (exit::FAILURE, lines.join("\n"));
    }
    if !removed_secret && !changed_config {
        return (
            exit::NO_TOKEN,
            format!("No OpenRouter key is stored as secret {secret}; nothing was removed."),
        );
    }
    lines.push(
        "  A running Axocoatl keeps the key it read when it started until it restarts".to_string(),
    );
    (exit::CONNECTED, lines.join("\n"))
}
