//! `axocoatl connect claude-code`: store a Claude Code OAuth token without
//! the user ever seeing, copying or typing it. Owner: workstream `agents`.
//!
//! The command runs `claude setup-token` in a pseudo-terminal the size of the
//! user's terminal, resized with it on `SIGWINCH` ([`relay`]), with the
//! user's terminal in raw mode (always restored) and keystrokes relayed to
//! the program until it exits. The program's output reaches the user's
//! terminal only through a streaming filter that runs it through a model of
//! the terminal ([`screen`]) and shows every `sk-ant-…` token it writes
//! masked, cell for cell, however the program's renderer places the token's
//! characters ([`redact`]); the filter also keeps the tokens it hid, in
//! memory only, and drops clipboard writes. Exactly one OAuth token
//! (`sk-ant-oat01-…`) must be found. Unless `--no-verify`, it is checked
//! with Anthropic ([`verify`]), then stored through the same secret store
//! as `axocoatl secret set` (same data root, `0600`).
//!
//! The token is held in zeroizing buffers and never reaches an argv, a
//! child's environment, a log, a panic, a `Debug` string, a temporary file
//! or an error message.
//!
//! `axocoatl connect codex` ([`codex`]) runs the official `codex login` the
//! same way, with a private `CODEX_HOME`, and stores the ChatGPT sign-in
//! Codex writes there: by default the pinned Codex in a throwaway container
//! ([`codex_container`]), whose sign-in page Axocoatl opens on this computer
//! ([`browser`]), or a Codex CLI on this computer with `--codex`. With
//! `--api-key` it also stores the OpenAI API key OpenAI issues with the
//! browser sign-in (`codex-openai`), and with `--api-key-only` only that key.
//! `claude setup-token` opens the browser itself (Claude Code 2.1.271's
//! bundle starts its OAuth flow without `skipBrowserOpen`), so nothing is
//! opened for it. `axocoatl connect codex --refresh` and `--status`
//! ([`codex_sign_in`]) refresh the stored ChatGPT sign-in through the
//! running daemon, and show what is stored, without a token.
//!
//! `axocoatl connect openrouter` ([`openrouter`]) signs in to OpenRouter in
//! the browser through its OAuth PKCE flow and stores the key OpenRouter
//! issues as the secret `openrouter`, which the configuration then names.

mod browser;
mod codex;
mod codex_container;
mod codex_sign_in;
pub(crate) mod openrouter;
mod openrouter_api;
mod openrouter_config;
mod openrouter_pkce;
mod redact;
mod relay;
mod screen;
mod verify;

#[cfg(test)]
mod ink_model;
#[cfg(test)]
mod openrouter_tests;
#[cfg(test)]
mod tests;

use std::io::{IsTerminal, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axocoatl_config::egress_routes::is_valid_credential_name;
use axocoatl_daemon::external_agent::claude_code::CLAUDE_CODE_SECRET;
use axocoatl_daemon::secret_store;
use clap::Subcommand;
use tokio::sync::Notify;

use redact::{select_token, Selection};
pub(crate) use verify::{Verdict, Verifier};

/// Exit codes of `axocoatl connect claude-code`.
pub(crate) mod exit {
    /// The token was stored.
    pub(crate) const CONNECTED: i32 = 0;
    /// Anthropic rejected the token (`401`/`403`); nothing was stored.
    pub(crate) const REJECTED: i32 = 1;
    /// `claude setup-token` ended without printing exactly one token;
    /// nothing was stored.
    pub(crate) const NO_TOKEN: i32 = 2;
    /// Usage: no interactive terminal, a bad secret name, no `claude` CLI
    /// (for `connect codex`: no Podman, a `--codex` that is not an
    /// executable, or port 1455 on this computer in use), or a command line
    /// `connect` does not take (clap's own usage errors exit 2 elsewhere).
    pub(crate) const USAGE: i32 = 3;
    /// The token could not be checked (network error or an unexpected
    /// answer); nothing was stored.
    pub(crate) const UNVERIFIED: i32 = 4;
    /// The data directory, the secret store or the pseudo-terminal failed.
    pub(crate) const FAILURE: i32 = 5;
    // `connect codex` uses CONNECTED, NO_TOKEN (no ChatGPT sign-in was
    // stored by `codex login`, also when the sign-in was not completed
    // within the sign-in time, `--sign-in-timeout`), USAGE (also
    // `--api-key` with `--device-auth`, or a value on the command line),
    // FAILURE, INTERRUPTED and, with `--api-key` or `--api-key-only`,
    // NO_API_KEY. `connect codex --refresh` uses CONNECTED (refreshed),
    // REJECTED (OpenAI refused the refresh: sign in again), NO_TOKEN (no
    // sign-in stored), USAGE, UNVERIFIED (the refresh could not be
    // completed: the token endpoint could not be reached, did not answer in
    // time, or gave an answer that is neither new tokens nor a refusal),
    // FAILURE and DAEMON_NOT_RUNNING; `connect codex --status` uses
    // CONNECTED (a sign-in is stored), NO_TOKEN (none is) and FAILURE.
    /// A signal stopped the command; nothing was stored.
    pub(crate) const INTERRUPTED: i32 = 6;
    /// `connect codex --refresh`: no daemon answers at its address (start
    /// it with `axocoatl serve`); nothing was refreshed.
    pub(crate) const DAEMON_NOT_RUNNING: i32 = 7;
    /// `connect codex --api-key` or `--api-key-only`: the sign-in held no
    /// OpenAI API key (OpenAI issued none for the account), or one that is
    /// not in the form of a key, so none was stored as `codex-openai`; with
    /// `--api-key` the ChatGPT sign-in was stored as `codex-chatgpt`.
    pub(crate) const NO_API_KEY: i32 = 8;
}

#[derive(Debug, Subcommand)]
pub enum ConnectCommands {
    /// Run `claude setup-token`, capture the token it prints without showing
    /// it, check it with Anthropic, and store it as a secret
    ClaudeCode {
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
        /// Name of the secret to store the token as
        #[arg(long, value_name = "NAME", default_value = CLAUDE_CODE_SECRET)]
        secret: String,
        /// Store the token without checking it with Anthropic first
        #[arg(long)]
        no_verify: bool,
        /// Path to the claude CLI (default: the first `claude` on PATH, then
        /// ~/.local/bin/claude)
        #[arg(long, value_name = "PATH")]
        claude: Option<PathBuf>,
    },
    /// Run the official `codex login` (the pinned Codex, in a throwaway
    /// container through Podman) with a private CODEX_HOME, and store the
    /// ChatGPT sign-in it makes (never shown) as the secret codex-chatgpt,
    /// for Codex writers with `credential: codex-chatgpt`; with --api-key,
    /// also the OpenAI API key OpenAI issues with it, as codex-openai
    Codex {
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
        /// Sign in with a code shown here instead of a browser on this
        /// computer (`codex login --device-auth`; no port is published)
        #[arg(long)]
        device_auth: bool,
        /// Run this Codex CLI on this computer instead of the pinned Codex in
        /// a container (no Podman needed then)
        #[arg(long, value_name = "PATH")]
        codex: Option<PathBuf>,
        /// Do not open the sign-in page in this computer's browser (the
        /// pinned Codex in the container cannot open it, so Axocoatl does,
        /// with `open` or `xdg-open`); open the address Codex prints yourself
        #[arg(long)]
        no_browser: bool,
        /// Minutes the sign-in may take, 1 to 60 (default 30): Podman stops
        /// and removes the login container after this
        #[arg(
            long,
            value_name = "MINUTES",
            value_parser = clap::value_parser!(u64).range(1..=60),
            conflicts_with = "codex"
        )]
        sign_in_timeout: Option<u64>,
        /// Also store the OpenAI API key OpenAI issues with the browser
        /// sign-in, never shown, as the secret codex-openai (a Codex writer's
        /// default credential, billed at OpenAI API rates to an OpenAI
        /// Platform organization, not to your ChatGPT plan); the ChatGPT
        /// sign-in is stored as codex-chatgpt too. Not with --device-auth
        #[arg(long)]
        api_key: bool,
        /// Like --api-key, but store only the API key (codex-openai), not
        /// the ChatGPT sign-in
        #[arg(long)]
        api_key_only: bool,
        /// Ask the running daemon to refresh the stored ChatGPT sign-in now,
        /// the way it refreshes it before a request; prints when the new
        /// access token expires, never a token
        #[arg(
            long,
            conflicts_with_all = [
                "device_auth", "codex", "no_browser", "sign_in_timeout", "status", "api_key",
                "api_key_only"
            ]
        )]
        refresh: bool,
        /// Show whether a ChatGPT sign-in is stored, its account (last four
        /// characters), when its access token expires and when it was last
        /// refreshed or signed in, never a token
        #[arg(
            long,
            conflicts_with_all = [
                "device_auth", "codex", "no_browser", "sign_in_timeout", "api_key", "api_key_only"
            ]
        )]
        status: bool,
        /// With --refresh: the daemon's URL (default: AXOCOATL_URL, then
        /// the configured server address)
        #[arg(long, requires = "refresh", conflicts_with = "status")]
        url: Option<String>,
        /// A value given on the command line (`connect codex --api-key
        /// "$KEY"`): refused without being shown, since clap's own
        /// "unexpected argument" error would print it.
        #[arg(hide = true, value_name = "VALUE")]
        on_command_line: Vec<String>,
    },
    /// Sign in to OpenRouter in the browser (OAuth PKCE), store the key it
    /// issues (never shown) as the secret openrouter, check it, and point
    /// providers.openrouter at it with openrouter_billing: credits
    Openrouter {
        /// Path to config file (selects the data directory; providers.openrouter
        /// is written here)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
        /// Name of the secret to store the key as
        #[arg(long, value_name = "NAME", default_value = openrouter::OPENROUTER_SECRET)]
        secret: String,
        /// The writer's OpenRouter model (vendor/model), checked in
        /// OpenRouter's catalog before the sign-in
        #[arg(long, value_name = "MODEL", conflicts_with_all = ["status", "remove"])]
        writer: Option<String>,
        /// The reviewer's OpenRouter model (vendor/model, not the writer's),
        /// checked in OpenRouter's catalog before the sign-in
        #[arg(long, value_name = "MODEL", conflicts_with_all = ["status", "remove"])]
        reviewer: Option<String>,
        /// Do not open the sign-in page in this computer's browser; only print
        /// its address (the browser must still reach 127.0.0.1 on this
        /// computer)
        #[arg(long, conflicts_with_all = ["status", "remove"])]
        no_browser: bool,
        /// Minutes the browser sign-in may take, 1 to 60 (default 10)
        #[arg(
            long,
            value_name = "MINUTES",
            value_parser = clap::value_parser!(u64).range(1..=60),
            conflicts_with_all = ["status", "remove"]
        )]
        sign_in_timeout: Option<u64>,
        /// Answer yes to the billing question without a terminal: this
        /// OpenRouter account pays with credits and has no BYOK provider keys
        /// connected
        #[arg(long, conflicts_with_all = ["status", "remove"])]
        credits_only: bool,
        /// Store the key without changing the configuration (the lines to add
        /// are printed); with --remove, leave the configuration as it is
        #[arg(long, conflicts_with = "status")]
        no_config: bool,
        /// Show whether a key is stored, its label, credit limit, usage and
        /// free-tier status (from OpenRouter), and what the configuration
        /// says; never the key
        #[arg(long, conflicts_with = "remove")]
        status: bool,
        /// Remove the stored key and the configuration's reference to it (the
        /// key stays in your OpenRouter account until you delete it there)
        #[arg(long)]
        remove: bool,
        /// A value given on the command line (`connect openrouter "$KEY"`):
        /// refused without being shown.
        #[arg(hide = true, value_name = "VALUE")]
        on_command_line: Vec<String>,
    },
}

/// A refusal or failure: its exit code and a message that never holds the
/// token.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) code: i32,
    pub(crate) message: String,
}

fn failure(code: i32, message: impl Into<String>) -> Failure {
    Failure {
        code,
        message: message.into(),
    }
}

/// A signal (`SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT`) asking the command to
/// stop. While it runs, the terminal is raw, so Ctrl-C reaches
/// `claude setup-token` as a keystroke instead; these come from elsewhere.
#[derive(Clone, Default)]
pub(crate) struct Interrupt {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Interrupt {
    /// Listen for the signals for as long as the returned tasks run.
    fn listen(&self) -> Vec<tokio::task::JoinHandle<()>> {
        use tokio::signal::unix::{signal, SignalKind};
        [
            SignalKind::interrupt(),
            SignalKind::terminate(),
            SignalKind::hangup(),
            SignalKind::quit(),
        ]
        .into_iter()
        .filter_map(|kind| signal(kind).ok())
        .map(|mut stream| {
            let interrupt = self.clone();
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    interrupt.raise();
                }
            })
        })
        .collect()
    }

    pub(crate) fn raise(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn raised(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    async fn wait(&self) {
        while !self.raised() {
            self.notify.notified().await;
        }
    }
}

/// `SIGWINCH`: the user's terminal changed size, so the program's must too.
#[derive(Clone, Default)]
pub(crate) struct Resized(Arc<AtomicBool>);

impl Resized {
    /// Listen for `SIGWINCH` for as long as the returned task runs.
    fn listen(&self) -> Option<tokio::task::JoinHandle<()>> {
        use tokio::signal::unix::{signal, SignalKind};
        let mut stream = signal(SignalKind::window_change()).ok()?;
        let resized = self.clone();
        Some(tokio::spawn(async move {
            while stream.recv().await.is_some() {
                resized.raise();
            }
        }))
    }

    pub(crate) fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// What `connect` needs besides the terminal.
pub(crate) struct Connect {
    pub(crate) claude: PathBuf,
    pub(crate) secret: String,
    pub(crate) data_dir: PathBuf,
    /// `None` with `--no-verify`.
    pub(crate) verifier: Option<Verifier>,
}

/// Is `path` a regular file someone may execute?
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The `claude` CLI: `explicit`, else the first `claude` on `path`, else
/// `home/.local/bin/claude`, as an absolute path.
pub(crate) fn find_claude(
    explicit: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    find_program(
        "claude",
        explicit,
        path,
        home,
        "the Claude Code CLI (`claude`) was not found on PATH or in ~/.local/bin. Install it \
         (https://docs.anthropic.com/en/docs/claude-code/setup) or pass its path with --claude",
    )
}

/// The Codex CLI given with `--codex PATH`, as an absolute path: an
/// executable file. Without `--codex`, no `codex` on this computer is used.
pub(crate) fn host_codex(explicit: &Path) -> Result<PathBuf, String> {
    find_program(
        "codex",
        Some(explicit),
        None,
        None,
        "the Codex CLI given with --codex was not found",
    )
}

/// `name`: `explicit`, else the first `name` on `path`, else
/// `home/.local/bin/<name>`, as an absolute path; `missing` when none is.
pub(crate) fn find_program(
    name: &str,
    explicit: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
    missing: &str,
) -> Result<PathBuf, String> {
    let absolute = |candidate: PathBuf| -> PathBuf {
        if candidate.is_absolute() {
            candidate
        } else {
            std::env::current_dir()
                .map(|directory| directory.join(&candidate))
                .unwrap_or(candidate)
        }
    };
    if let Some(explicit) = explicit {
        return if is_executable(explicit) {
            Ok(absolute(explicit.to_path_buf()))
        } else {
            // The path is not repeated: a key given as `--codex KEY` would
            // otherwise be printed.
            Err(format!(
                "the path given with --{name} is not an executable file (it is not repeated \
                 here, in case it is a key or token)"
            ))
        };
    }
    let on_path = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate));
    if let Some(found) = on_path {
        return Ok(found);
    }
    if let Some(home) = home {
        let local = home.join(".local/bin").join(name);
        if is_executable(&local) {
            return Ok(absolute(local));
        }
    }
    Err(missing.to_string())
}

/// The line printed once the token is stored, with the next step.
fn connected_message(secret: &str) -> String {
    let mut message = format!(
        "Connected Claude Code: stored secret {secret} (the token was never shown)\n  Next: \
         build the Session image with `axocoatl recipe build claude-code`, then run a loadout \
         whose writer has `runtime: claude-code`, for example `axocoatl run <loadout> --task \
         \"…\" --model writer=anthropic:<model>`."
    );
    if secret == CLAUDE_CODE_SECRET {
        message.push_str(&format!(
            " Claude Code's route to api.anthropic.com sends {secret}."
        ));
    } else {
        message.push_str(&format!(
            " Claude Code's own route sends {CLAUDE_CODE_SECRET}; name {secret} as a route's \
             `credential` to use this one."
        ));
    }
    message
}

/// Run `claude setup-token` on the terminal `input`/`output`, capture its
/// token, check it and store it. Returns the confirmation to print.
/// `resized` is raised when the terminal changes size.
pub(crate) async fn connect(
    request: Connect,
    input: OwnedFd,
    output: Box<dyn Write + Send>,
    interrupt: &Interrupt,
    resized: &Resized,
) -> Result<String, Failure> {
    let Connect {
        claude,
        secret,
        data_dir,
        verifier,
    } = request;
    let flag = interrupt.flag.clone();
    let resized = resized.0.clone();
    let (run, mut output) = tokio::task::spawn_blocking(move || {
        let mut output = output;
        let run = relay::run_setup_token(&claude, input.as_fd(), &mut *output, &flag, &resized);
        (run, output)
    })
    .await
    .map_err(|_| failure(exit::FAILURE, "running claude setup-token failed"))?;
    let run = run.map_err(|error| failure(exit::FAILURE, error))?;
    if run.interrupted {
        return Err(failure(
            exit::INTERRUPTED,
            "interrupted: claude setup-token was stopped and nothing was stored",
        ));
    }
    let ended = match run.exit_code {
        Some(0) => "finished".to_string(),
        Some(code) => format!("exited with status {code}"),
        None => "ended".to_string(),
    };
    let again = "Run `axocoatl connect claude-code` again and finish the sign-in it opens.";
    let token = match select_token(&run.found) {
        Selection::One(token) => token,
        Selection::None { other } => {
            let hidden = if other > 0 {
                format!(" It printed {other} other credential-like value(s), hidden on screen.")
            } else {
                String::new()
            };
            return Err(failure(
                exit::NO_TOKEN,
                format!(
                    "claude setup-token {ended} without printing a Claude Code OAuth token \
                     (sk-ant-oat01-…); nothing was stored.{hidden} {again}"
                ),
            ));
        }
        Selection::Several(count) => {
            return Err(failure(
                exit::NO_TOKEN,
                format!(
                    "claude setup-token printed {count} different OAuth tokens (all hidden on \
                     screen), so Axocoatl cannot tell which one to store; nothing was stored. \
                     {again}"
                ),
            ));
        }
    };
    // Every other copy of the token (the relay's captures) is zeroized now,
    // not after the network check. The keystroke relay stopped when the
    // program exited, so what the user types from here on stays in the
    // terminal for the shell.
    drop(run);
    if let Some(verifier) = &verifier {
        let _ = writeln!(
            output,
            "Checking the token with {} (it is not shown)…",
            verifier.host()
        );
        let _ = output.flush();
        let verdict = tokio::select! {
            verdict = verifier.check(&token) => verdict,
            () = interrupt.wait() => {
                return Err(failure(
                    exit::INTERRUPTED,
                    "interrupted while checking the token; nothing was stored",
                ));
            }
        };
        match verdict {
            Verdict::Valid => {}
            Verdict::Rejected(status) => {
                return Err(failure(
                    exit::REJECTED,
                    format!(
                        "Anthropic rejected the token (HTTP {status}); nothing was stored. \
                         {again}"
                    ),
                ));
            }
            Verdict::Unverified(reason) => {
                return Err(failure(
                    exit::UNVERIFIED,
                    format!(
                        "could not check the token with Anthropic: {reason}. Nothing was \
                         stored. Check the network and run `axocoatl connect claude-code` \
                         again, or add --no-verify to store the token without checking it."
                    ),
                ));
            }
        }
    }
    secret_store::set_secret(&data_dir, &secret, token.as_bytes())
        .map_err(|error| failure(exit::FAILURE, format!("storing the token: {error}")))?;
    drop(token);
    Ok(connected_message(&secret))
}

/// Check, before the sign-in, that the store will take the token: every
/// refusal `set_secret` would make after it (a link or a directory at
/// `secrets/<name>`, a full store, a data root that does not open) comes
/// now, before `claude setup-token` makes a token that would then be lost.
pub(crate) fn check_store(data_dir: &Path, secret: &str) -> Result<(), Failure> {
    secret_store::check_can_store(data_dir, secret).map_err(|error| {
        failure(
            exit::FAILURE,
            format!("{error}; `claude setup-token` was not run, so no token was made"),
        )
    })
}

/// The message when standard input or output is not a terminal.
fn needs_terminal() -> String {
    "axocoatl connect claude-code needs an interactive terminal: it runs `claude setup-token`, \
     which signs you in through the browser. Run it in a terminal. Without one, store a token \
     you already have with `axocoatl secret set claude-code-oauth --from-env VAR` or by piping \
     it into `axocoatl secret set claude-code-oauth`."
        .to_string()
}

/// The refusal of a value on `connect codex`'s command line, which never
/// repeats it.
pub(crate) const VALUE_ON_COMMAND_LINE: &str =
    "axocoatl connect codex takes no key, token or other value on its command line, so \
     nothing was run or stored (the value is not repeated here). You never type or paste a \
     key: `axocoatl connect codex --api-key` signs you in in the browser and stores the OpenAI \
     API key OpenAI issues with that sign-in, without showing it.";

/// The refusal of a value given to a `connect` flag that takes none
/// (`--api-key=VALUE`), in place of clap's own error, which would print it.
pub(crate) fn flag_takes_no_value(flag: &str) -> String {
    let mut message = format!(
        "{flag} takes no value, so nothing was run or stored (the value is not repeated here, \
         in case it is a key or token)."
    );
    if flag.starts_with("--api-key") {
        message.push_str(
            " You never type or paste a key: `axocoatl connect codex --api-key` signs you in in \
             the browser and stores the OpenAI API key OpenAI issues with that sign-in, without \
             showing it.",
        );
    }
    message
}

/// The refusal of an invalid value given to `flag` (`--sign-in-timeout
/// KEY`), in place of clap's own error, which would print it. `None` when
/// the flag is not one `connect` takes.
pub(crate) fn invalid_value(flag: Option<&str>) -> String {
    let mut message = format!(
        "the value given to {} is not one it takes, so nothing was run or stored (the value is \
         not repeated here, in case it is a key or token).",
        flag.unwrap_or("an option")
    );
    if flag == Some("--sign-in-timeout") {
        message.push_str(" --sign-in-timeout takes 1 to 60 minutes.");
    }
    message
}

/// The refusal of an argument `connect` does not take (`"--api-key KEY"`
/// as one argument, a misspelt flag), in place of clap's own error, which
/// would print it: only a flag `connect` takes that it starts with
/// (`starts_with`) and clap's suggestion (`suggested`, a flag `connect`
/// takes) are named.
pub(crate) fn unknown_argument(starts_with: Option<&str>, suggested: Option<&str>) -> String {
    let mut message = match starts_with {
        Some(flag) => format!(
            "an argument that starts with {flag} is not one axocoatl connect takes, so nothing \
             was run or stored (the rest of it is not repeated here, in case it is a key or \
             token)."
        ),
        None => "axocoatl connect was given an argument it does not take, so nothing was run \
                 or stored (it is not repeated here, in case it is a key or token)."
            .to_string(),
    };
    if let Some(suggested) = suggested {
        message.push_str(&format!(" Did you mean {suggested}?"));
    }
    if starts_with.is_some_and(|flag| flag.starts_with("--api-key")) {
        message.push_str(
            " You never type or paste a key: `axocoatl connect codex --api-key` signs you in in \
             the browser and stores the OpenAI API key OpenAI issues with that sign-in, without \
             showing it.",
        );
    }
    message
}

/// The refusal of a `connect` target that is not `claude-code`, `codex` or
/// `openrouter`, which never repeats it.
pub(crate) const UNKNOWN_TARGET: &str =
    "axocoatl connect connects claude-code, codex or openrouter, and what was given is none of \
     them, so nothing was run or stored (it is not repeated here, in case it is a key or token).";

/// Any other `connect` command line clap refuses whose error would repeat
/// a value given on it.
pub(crate) const UNREADABLE_COMMAND_LINE: &str =
    "axocoatl connect could not use its command line, so nothing was run or stored (it is not \
     repeated here, in case it holds a key or token). Run `axocoatl connect --help`.";

/// Returns the process exit code.
pub async fn cmd_connect(command: ConnectCommands) -> i32 {
    if let ConnectCommands::Codex {
        on_command_line, ..
    } = &command
    {
        if !on_command_line.is_empty() {
            eprintln!("✗ {VALUE_ON_COMMAND_LINE}");
            return exit::USAGE;
        }
    }
    let (config, secret, no_verify, claude) = match command {
        ConnectCommands::Openrouter {
            on_command_line, ..
        } if !on_command_line.is_empty() => {
            eprintln!("✗ {}", openrouter::VALUE_ON_COMMAND_LINE);
            return exit::USAGE;
        }
        ConnectCommands::Openrouter {
            config,
            secret,
            writer,
            reviewer,
            no_browser,
            sign_in_timeout,
            credits_only,
            no_config,
            status,
            remove,
            on_command_line: _,
        } => {
            return openrouter::cmd_connect_openrouter(openrouter::Flags {
                config,
                secret,
                writer,
                reviewer,
                no_browser,
                sign_in_timeout,
                credits_only,
                no_config,
                status,
                remove,
            })
            .await;
        }
        ConnectCommands::ClaudeCode {
            config,
            secret,
            no_verify,
            claude,
        } => (config, secret, no_verify, claude),
        ConnectCommands::Codex {
            config,
            refresh: true,
            url,
            ..
        } => return codex_sign_in::cmd_refresh(url.as_deref(), &config).await,
        ConnectCommands::Codex {
            config,
            status: true,
            ..
        } => return codex_sign_in::cmd_status(&config),
        ConnectCommands::Codex {
            config,
            device_auth,
            codex: path,
            no_browser,
            sign_in_timeout,
            api_key,
            api_key_only,
            ..
        } => {
            let keep = codex::Keep::from_flags(api_key, api_key_only);
            // Before anything else: no sign-in could give the key asked for.
            if device_auth && keep.api_key() {
                eprintln!("✗ {}", codex::api_key_needs_browser(keep.flag()));
                return exit::USAGE;
            }
            return cmd_connect_codex(config, device_auth, path, no_browser, sign_in_timeout, keep)
                .await;
        }
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("✗ {}", needs_terminal());
        return exit::USAGE;
    }
    // The message never repeats the argument: `--secret "$TOKEN"` would
    // otherwise print a token.
    if !is_valid_credential_name(&secret) {
        eprintln!("✗ --secret: {}", secret_store::INVALID_NAME);
        return exit::USAGE;
    }
    let data_dir = match crate::secret_cmd::data_dir(&config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    if let Err(Failure { code, message }) = check_store(&data_dir, &secret) {
        eprintln!("✗ {message}");
        return code;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let claude = match find_claude(
        claude.as_deref(),
        std::env::var_os("PATH").as_deref(),
        home.as_deref(),
    ) {
        Ok(claude) => claude,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::USAGE;
        }
    };
    let verifier = if no_verify {
        None
    } else {
        match Verifier::anthropic() {
            Ok(verifier) => Some(verifier),
            Err(error) => {
                eprintln!("✗ {error}");
                return exit::FAILURE;
            }
        }
    };
    let input = match relay::owned_stdin() {
        Ok(input) => input,
        Err(error) => {
            eprintln!("✗ could not read the terminal: {error}");
            return exit::FAILURE;
        }
    };
    eprintln!(
        "Running `{} setup-token`. Sign in when it asks. Axocoatl reads the token it prints, \
         hides it on screen and stores it as secret {secret}: you never copy or paste it.\n",
        claude.display()
    );
    let interrupt = Interrupt::default();
    let mut listeners = interrupt.listen();
    let resized = Resized::default();
    listeners.extend(resized.listen());
    let result = connect(
        Connect {
            claude,
            secret,
            data_dir,
            verifier,
        },
        input,
        Box::new(std::io::stdout()),
        &interrupt,
        &resized,
    )
    .await;
    for listener in listeners {
        listener.abort();
    }
    match result {
        Ok(message) => {
            println!("\n✓ {message}");
            exit::CONNECTED
        }
        Err(Failure { code, message }) => {
            eprintln!("\n✗ {message}");
            code
        }
    }
}

/// `axocoatl connect codex`. Returns the process exit code.
async fn cmd_connect_codex(
    config: PathBuf,
    device_auth: bool,
    explicit: Option<PathBuf>,
    no_browser: bool,
    sign_in_timeout: Option<u64>,
    keep: codex::Keep,
) -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("✗ {}", codex::needs_terminal());
        return exit::USAGE;
    }
    let data_dir = match crate::secret_cmd::data_dir(&config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    // Every secret this run may store, together: two that each fit alone
    // may not fit both, and a sign-in made then would be lost.
    if let Err(error) = secret_store::check_can_store_all(&data_dir, &keep.secrets()) {
        eprintln!("✗ {error}; `codex login` was not run, so no sign-in was made");
        return exit::FAILURE;
    }
    let kept = codex::what_is_kept(keep);
    // Only for the container: a Codex on this computer opens the browser
    // sign-in itself.
    let opener = if no_browser || explicit.is_some() {
        None
    } else {
        browser::Opener::find(std::env::var_os("PATH").as_deref())
    };
    let how = codex::sign_in_how(device_auth, explicit.is_none(), opener.is_some());
    let (runner, intro) = match explicit {
        Some(explicit) => {
            let path = match host_codex(&explicit) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!("✗ {error}");
                    return exit::USAGE;
                }
            };
            let intro = format!(
                "Running `{} login{}` with a private temporary CODEX_HOME. {how} Axocoatl reads \
                 the sign-in Codex stores there, {kept} and then erases the temporary \
                 CODEX_HOME. Your own ~/.codex is not touched.\n",
                path.display(),
                if device_auth { " --device-auth" } else { "" },
            );
            (codex::Runner::Host { codex: path }, intro)
        }
        None => {
            let podman = match codex_container::find_podman(std::env::var_os("PATH").as_deref()) {
                Ok(podman) => podman,
                Err(error) => {
                    eprintln!("✗ {error}");
                    return exit::USAGE;
                }
            };
            if !device_auth {
                if let Err(error) =
                    codex_container::check_callback_port(codex_container::CALLBACK_PORT)
                {
                    eprintln!("✗ {error}");
                    return exit::USAGE;
                }
            }
            let image = match codex_container::recipe_image(&data_dir).await {
                Ok(image) => image,
                Err(Failure { code, message }) => {
                    eprintln!("✗ {message}");
                    return code;
                }
            };
            let returns = if device_auth {
                String::new()
            } else {
                format!(
                    " The browser sign-in returns to 127.0.0.1:{}, published from the container \
                     on this computer's loopback only.",
                    codex_container::CALLBACK_PORT
                )
            };
            let login = codex_container::Login::new(
                podman,
                image,
                codex_container::sign_in_lifetime(sign_in_timeout),
                opener,
            );
            let intro = format!(
                "Running `codex login{}` (Codex {}) in a throwaway container of the codex recipe \
                 image ({}), with a private CODEX_HOME in its memory. {how}{returns} {} \
                 Axocoatl reads the sign-in Codex stores there, {kept} and then removes the \
                 container. Your own ~/.codex is not touched.\n",
                if device_auth { " --device-auth" } else { "" },
                axocoatl_daemon::external_agent::codex::CODEX_VERSION,
                &login.image[..12.min(login.image.len())],
                codex_container::time_to_sign_in(login.lifetime),
            );
            (codex::Runner::Container(login), intro)
        }
    };
    let input = match relay::owned_stdin() {
        Ok(input) => input,
        Err(error) => {
            eprintln!("✗ could not read the terminal: {error}");
            return exit::FAILURE;
        }
    };
    eprintln!("{intro}");
    // What is shown below the intro's blank line is followed, so that the
    // outcome is set off from it by exactly one blank line however Codex's
    // output ended.
    let lines = relay::Lines::new(relay::terminal_size(std::io::stdout().as_fd()), true);
    let interrupt = Interrupt::default();
    let mut listeners = interrupt.listen();
    let resized = Resized::default();
    listeners.extend(resized.listen());
    let result = codex::connect(
        codex::ConnectCodex {
            runner,
            data_dir,
            device_auth,
            keep,
        },
        input,
        Box::new(lines.follow(std::io::stdout())),
        &interrupt,
        &resized,
    )
    .await;
    for listener in listeners {
        listener.abort();
    }
    let before = lines.before_message();
    match result {
        Ok(message) => {
            println!("{before}✓ {message}");
            exit::CONNECTED
        }
        Err(Failure { code, message }) => {
            eprintln!("{before}✗ {message}");
            code
        }
    }
}
