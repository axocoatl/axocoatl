//! `axocoatl connect claude-code`: store a Claude Code OAuth token without
//! the user ever seeing, copying or typing it. Owner: workstream `agents`.
//!
//! The command runs `claude setup-token` in a pseudo-terminal 1000 columns
//! wide (so the token is printed on one line), with the user's terminal in
//! raw mode (always restored) and keystrokes relayed to it. The program's
//! output reaches the user's terminal only through a streaming filter that
//! replaces every `sk-ant-…` token with a mask ([`redact`]); the filter also
//! keeps the tokens it hid, in memory only. Exactly one OAuth token
//! (`sk-ant-oat01-…`) must be found. Unless `--no-verify`, it is checked
//! with Anthropic ([`verify`]), then stored through the same secret store
//! as `axocoatl secret set` (same data root, `0600`).
//!
//! The token is held in zeroizing buffers and never reaches an argv, a
//! child's environment, a log, a panic, a `Debug` string, a temporary file
//! or an error message.

mod redact;
mod relay;
mod verify;

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
    /// Usage: no interactive terminal, a bad secret name, no `claude` CLI.
    pub(crate) const USAGE: i32 = 3;
    /// The token could not be checked (network error or an unexpected
    /// answer); nothing was stored.
    pub(crate) const UNVERIFIED: i32 = 4;
    /// The data directory, the secret store or the pseudo-terminal failed.
    pub(crate) const FAILURE: i32 = 5;
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

/// What `connect` needs besides the terminal.
pub(crate) struct Connect {
    pub(crate) claude: PathBuf,
    pub(crate) secret: String,
    pub(crate) data_dir: PathBuf,
    /// `None` with `--no-verify`.
    pub(crate) verifier: Option<Verifier>,
    /// The program's terminal width ([`relay::WIDE_COLUMNS`]).
    pub(crate) cols: u16,
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
                "--claude {} is not an executable file",
                explicit.display()
            ))
        };
    }
    let on_path = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("claude"))
        .find(|candidate| is_executable(candidate));
    if let Some(found) = on_path {
        return Ok(found);
    }
    if let Some(home) = home {
        let local = home.join(".local/bin/claude");
        if is_executable(&local) {
            return Ok(absolute(local));
        }
    }
    Err(
        "the Claude Code CLI (`claude`) was not found on PATH or in ~/.local/bin. Install it \
         (https://docs.anthropic.com/en/docs/claude-code/setup) or pass its path with --claude"
            .into(),
    )
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
pub(crate) async fn connect(
    request: Connect,
    input: OwnedFd,
    output: Box<dyn Write + Send>,
    interrupt: &Interrupt,
) -> Result<String, Failure> {
    let Connect {
        claude,
        secret,
        data_dir,
        verifier,
        cols,
    } = request;
    let flag = interrupt.flag.clone();
    let (run, mut output) = tokio::task::spawn_blocking(move || {
        let mut output = output;
        let run = relay::run_setup_token(&claude, input.as_fd(), &mut *output, cols, &flag);
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
    let ConnectCommands::ClaudeCode {
        config,
        secret,
        no_verify,
        claude,
    } = command;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("✗ {}", needs_terminal());
        return exit::USAGE;
    }
    if !is_valid_credential_name(&secret) {
        eprintln!(
            "✗ {secret:?} is not a secret name: use 1-64 letters, digits, '_', '.' or '-', \
             starting with a letter or digit (for example {CLAUDE_CODE_SECRET})"
        );
        return exit::USAGE;
    }
    let data_dir = match crate::secret_cmd::data_dir(&config) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::FAILURE;
        }
    };
    // The store must open before the sign-in, not after it.
    if let Err(error) = secret_store::list_secrets(&data_dir) {
        eprintln!("✗ {error}");
        return exit::FAILURE;
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
    let listeners = interrupt.listen();
    let result = connect(
        Connect {
            claude,
            secret,
            data_dir,
            verifier,
            cols: relay::WIDE_COLUMNS,
        },
        input,
        Box::new(std::io::stdout()),
        &interrupt,
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
