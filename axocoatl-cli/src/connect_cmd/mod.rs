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
//! ([`codex_container`]), or a Codex CLI on this computer with `--codex`.

mod codex;
mod codex_container;
mod redact;
mod relay;
mod screen;
mod verify;

#[cfg(test)]
mod ink_model;
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
    /// executable, or port 1455 on this computer in use).
    pub(crate) const USAGE: i32 = 3;
    /// The token could not be checked (network error or an unexpected
    /// answer); nothing was stored.
    pub(crate) const UNVERIFIED: i32 = 4;
    /// The data directory, the secret store or the pseudo-terminal failed.
    pub(crate) const FAILURE: i32 = 5;
    // `connect codex` uses CONNECTED, NO_TOKEN (no ChatGPT sign-in was
    // stored by `codex login`), USAGE, FAILURE and INTERRUPTED.
    /// A signal stopped the command; nothing was stored.
    pub(crate) const INTERRUPTED: i32 = 6;
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
    /// for Codex writers with `credential: codex-chatgpt`
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
            Err(format!(
                "--{name} {} is not an executable file",
                explicit.display()
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

/// Returns the process exit code.
pub async fn cmd_connect(command: ConnectCommands) -> i32 {
    let (config, secret, no_verify, claude) = match command {
        ConnectCommands::ClaudeCode {
            config,
            secret,
            no_verify,
            claude,
        } => (config, secret, no_verify, claude),
        ConnectCommands::Codex {
            config,
            device_auth,
            codex: path,
        } => return cmd_connect_codex(config, device_auth, path).await,
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
async fn cmd_connect_codex(config: PathBuf, device_auth: bool, explicit: Option<PathBuf>) -> i32 {
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
    let secret = axocoatl_daemon::external_agent::codex_chatgpt::CODEX_CHATGPT_SECRET;
    if let Err(error) = secret_store::check_can_store(&data_dir, secret) {
        eprintln!("✗ {error}; `codex login` was not run, so no sign-in was made");
        return exit::FAILURE;
    }
    let how = if device_auth {
        "Enter the code it shows at the address it gives, on any device."
    } else {
        "Sign in in the browser it opens, or open the address it prints in a browser on this \
         computer."
    };
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
                 the sign-in Codex stores there, never shows it, stores it as secret {secret} \
                 and then erases the temporary CODEX_HOME. Your own ~/.codex is not touched.\n",
                path.display(),
                if device_auth { " --device-auth" } else { "" },
            );
            (codex::Runner::Host { codex: path }, intro)
        }
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let podman = match codex_container::find_podman(
                std::env::var_os("PATH").as_deref(),
                home.as_deref(),
            ) {
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
            let intro = format!(
                "Running `codex login{}` (Codex {}) in a throwaway container of the codex recipe \
                 image ({}), with a private CODEX_HOME in its memory. {how}{returns} Axocoatl \
                 reads the sign-in Codex stores there, never shows it, stores it as secret \
                 {secret} and then removes the container. Your own ~/.codex is not touched.\n",
                if device_auth { " --device-auth" } else { "" },
                axocoatl_daemon::external_agent::codex::CODEX_VERSION,
                &image[..12.min(image.len())],
            );
            (codex::Runner::Container { podman, image }, intro)
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
    let interrupt = Interrupt::default();
    let mut listeners = interrupt.listen();
    let resized = Resized::default();
    listeners.extend(resized.listen());
    let result = codex::connect(
        codex::ConnectCodex {
            runner,
            data_dir,
            device_auth,
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
