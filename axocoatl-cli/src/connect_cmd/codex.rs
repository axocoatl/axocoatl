//! `axocoatl connect codex`: sign in to ChatGPT with the official Codex CLI
//! and store the sign-in, without anyone seeing, copying or typing a token.
//!
//! By default nothing needs to be installed on this computer but Podman:
//! the pinned Codex runs `codex login` in a throwaway container of the
//! `codex` recipe image, with a private `CODEX_HOME` in memory
//! ([`super::codex_container`]). With `--codex PATH` a Codex CLI on this
//! computer runs it instead, as described below.
//!
//! Either way `codex -c cli_auth_credentials_store="file" login` runs
//! (`--device-auth` with `--device-auth`) in the same pseudo-terminal relay
//! as `connect claude-code` ([`super::relay`]): the user signs in in the
//! browser, or enters the code Codex shows. Its output reaches the user's
//! terminal through the same filters, which also hide OpenAI API keys and
//! JWTs ([`super::redact::FilterChain::codex`]); Codex 0.160.1 prints
//! neither, it writes the tokens to `auth.json`. Axocoatl reads them from
//! that file, never from the screen
//! (`axocoatl_daemon::external_agent::codex_chatgpt::ChatgptSignIn`), and
//! stores the access token, refresh token and account id as the secret
//! `codex-chatgpt` (same store and rules as `axocoatl secret set`: `0600`,
//! replaced atomically).
//!
//! With `--api-key` ([`Keep::ApiKeyAndSignIn`]) it also stores the OpenAI API
//! key OpenAI issues with the browser sign-in as the secret `codex-openai`,
//! and with `--api-key-only` ([`Keep::ApiKeyOnly`]) only that key. Codex
//! 0.160.1's browser sign-in (`codex-rs/login/src/server.rs`) exchanges the
//! id token it obtained at `auth.openai.com/oauth/token`
//! (`grant_type=urn:ietf:params:oauth:grant-type:token-exchange`,
//! `requested_token=openai-api-key`) and writes the key it gets as
//! `OPENAI_API_KEY` in `auth.json`; when the exchange fails it writes none
//! and the sign-in still succeeds. The device-code sign-in never exchanges,
//! so `--api-key` is refused with `--device-auth`. The key is read from the
//! same `auth.json`, in memory, checked for the form Codex writes
//! ([`is_api_key`]) and stored under the same lock as the sign-in; it is
//! never shown, logged or written anywhere else.
//!
//! With `--codex PATH`, the command makes a private, temporary `CODEX_HOME`
//! (`0700`) in the data root, and after the sign-in overwrites every file in
//! it with zeros and removes it, also on an error or a signal (a release
//! build aborts on a panic, and a killed or aborted run leaves the home for
//! the next run to erase, [`erase_leftovers`]). Codex runs with that
//! `CODEX_HOME`, in that directory (so no repository's
//! `.codex/config.toml` applies), with file storage forced (never the
//! system keyring) and without the variables that would change where it
//! signs in or what it uses instead (`CODEX_API_KEY`, `OPENAI_API_KEY`,
//! `CODEX_ACCESS_TOKEN`, the endpoint and client overrides, and
//! `CODEX_SQLITE_HOME`, so its state stays in the temporary home). The user's own
//! `~/.codex` is never read or written by Axocoatl, and Codex is told to
//! use the temporary home instead.

use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use axocoatl_daemon::external_agent::codex::CODEX_SECRET;
use axocoatl_daemon::external_agent::codex_chatgpt::{
    ChatgptSignIn, CODEX_CHATGPT_SECRET, MAX_AUTH_FILE_BYTES,
};
use axocoatl_daemon::secret_store;
use zeroize::{Zeroize, Zeroizing};

use super::{exit, failure, relay, Failure, Interrupt, Resized};

/// The temporary homes' names in the data root: this, then a random part.
pub(crate) const TEMP_HOME_PREFIX: &str = "connect-codex-";

/// Variables `codex login` must not inherit: other credentials Codex would
/// use, and overrides of where it signs in, refreshes or revokes.
pub(crate) const REMOVED_ENV: [&str; 11] = [
    "CODEX_API_KEY",
    "OPENAI_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
    "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
    "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
    "CODEX_AUTHAPI_BASE_URL",
    "CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL",
    "CODEX_AGENT_IDENTITY_JWKS_BASE_URL",
    "OPENAI_BASE_URL",
    "CODEX_SQLITE_HOME",
];

/// Where `codex login` runs.
pub(crate) enum Runner {
    /// A `codex` on this computer (`--codex PATH`), with a temporary
    /// `CODEX_HOME` in the data root.
    Host { codex: PathBuf },
    /// The pinned Codex in a throwaway container of the `codex` recipe
    /// image, through Podman ([`super::codex_container`]).
    Container(super::codex_container::Login),
}

/// What `connect codex` stores from the sign-in Codex makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// The ChatGPT sign-in, as [`CODEX_CHATGPT_SECRET`] (the default).
    SignIn,
    /// The OpenAI API key OpenAI issued with the browser sign-in, as
    /// [`CODEX_SECRET`], and the ChatGPT sign-in (`--api-key`).
    ApiKeyAndSignIn,
    /// Only the API key (`--api-key-only`).
    ApiKeyOnly,
}

impl Keep {
    /// From the command line's `--api-key` and `--api-key-only`.
    pub(crate) fn from_flags(api_key: bool, api_key_only: bool) -> Self {
        match (api_key, api_key_only) {
            (_, true) => Self::ApiKeyOnly,
            (true, false) => Self::ApiKeyAndSignIn,
            (false, false) => Self::SignIn,
        }
    }

    /// Whether the API key is stored.
    pub(crate) fn api_key(self) -> bool {
        self != Self::SignIn
    }

    /// Whether the ChatGPT sign-in is stored.
    pub(crate) fn sign_in(self) -> bool {
        self != Self::ApiKeyOnly
    }

    /// The secrets it may store.
    pub(crate) fn secrets(self) -> Vec<&'static str> {
        let mut secrets = Vec::new();
        if self.api_key() {
            secrets.push(CODEX_SECRET);
        }
        if self.sign_in() {
            secrets.push(CODEX_CHATGPT_SECRET);
        }
        secrets
    }

    /// The flag that asked for the API key, for messages.
    pub(crate) fn flag(self) -> &'static str {
        match self {
            Self::ApiKeyOnly => "--api-key-only",
            Self::ApiKeyAndSignIn | Self::SignIn => "--api-key",
        }
    }
}

/// What `connect codex` needs besides the terminal.
pub(crate) struct ConnectCodex {
    pub(crate) runner: Runner,
    pub(crate) data_dir: PathBuf,
    pub(crate) device_auth: bool,
    pub(crate) keep: Keep,
}

/// A private temporary `CODEX_HOME`: every file in it is overwritten with
/// zeros and the whole directory removed when it is dropped.
pub(crate) struct TempHome {
    path: PathBuf,
}

impl TempHome {
    /// A new `0700` directory in `data_dir`, as an absolute path: Codex
    /// runs in it and reads a relative `CODEX_HOME` from there, so a
    /// relative data root (`./data` beside the configuration file, or
    /// `AXOCOATL_DATA_DIR=data`) would point it at a directory that does
    /// not exist.
    pub(crate) fn create(data_dir: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::absolute(data_dir)?.join(format!(
            "{TEMP_HOME_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        erase_dir(&self.path);
    }
}

/// Overwrite every regular file under `path` with zeros (no link is
/// followed) and remove the directory.
pub(crate) fn erase_dir(path: &Path) {
    overwrite_files(path, 0);
    let _ = std::fs::remove_dir_all(path);
}

fn overwrite_files(path: &Path, depth: usize) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let child = entry.path();
        if kind.is_dir() {
            overwrite_files(&child, depth + 1);
        } else if kind.is_file() {
            let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc_nofollow())
                .open(&child)
            else {
                continue;
            };
            let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
                continue;
            };
            let zeros = [0u8; 8192];
            let mut left = length;
            while left > 0 {
                let chunk = left.min(zeros.len() as u64) as usize;
                if file.write_all(&zeros[..chunk]).is_err() {
                    break;
                }
                left -= chunk as u64;
            }
            let _ = file.sync_all();
        }
    }
}

/// `O_NOFOLLOW | O_CLOEXEC`.
fn libc_nofollow() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32
}

/// How old a temporary home must be before another `connect codex` erases
/// it as left behind: longer than a sign-in takes (Codex's device code
/// expires after 15 minutes), so a run going on at the same time keeps its.
pub(crate) const LEFTOVER_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Erase what an earlier `connect codex` that was killed left in `data_dir`:
/// a temporary home it could not erase, older than [`LEFTOVER_AGE`].
pub(crate) fn erase_leftovers(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TEMP_HOME_PREFIX));
        let old = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= LEFTOVER_AGE);
        if metadata.is_dir() && ours && old {
            erase_dir(&entry.path());
        }
    }
}

/// `home/auth.json` as Codex wrote it: a regular file (no link followed) of
/// at most [`MAX_AUTH_FILE_BYTES`], in a zeroizing buffer. `None` when
/// there is none.
pub(crate) fn read_auth_file(home: &Path) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = home.join("auth.json");
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_nofollow())
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Codex's auth.json cannot be opened: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("Codex's auth.json cannot be read: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_AUTH_FILE_BYTES as u64 {
        return Err(format!(
            "Codex's auth.json is not a regular file of at most {MAX_AUTH_FILE_BYTES} bytes"
        ));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_AUTH_FILE_BYTES + 1));
    file.take(MAX_AUTH_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Codex's auth.json cannot be read: {error}"))?;
    Ok(Some(bytes))
}

/// The arguments `codex` runs with.
pub(crate) fn login_args(device_auth: bool) -> Vec<&'static str> {
    let mut args = vec!["-c", "cli_auth_credentials_store=\"file\"", "login"];
    if device_auth {
        args.push("--device-auth");
    }
    args
}

/// The line printed once what `keep` asks for is stored, with the next
/// step. `container`: Codex ran in a login container, not on this computer.
pub(crate) fn connected_message(keep: Keep, container: bool) -> String {
    let removed = if container {
        "the login container was removed"
    } else {
        "the temporary CODEX_HOME was erased"
    };
    let billed = "Requests a Codex writer makes with codex-openai are billed at OpenAI API rates \
                  to the key's OpenAI Platform organization, not to your ChatGPT plan.";
    match keep {
        Keep::SignIn => format!(
            "Connected Codex: stored your ChatGPT sign-in as secret {CODEX_CHATGPT_SECRET} (no \
             token was shown, and {removed})\n  Next: build the Session image \
             with `axocoatl recipe build codex`, then run a loadout whose writer has `runtime: \
             codex` and `credential: {CODEX_CHATGPT_SECRET}`, for example `axocoatl run \
             <loadout> --task \"…\" --model writer=openai:<model>`. The daemon refreshes the \
             access token on this computer; it never enters the Session container."
        ),
        Keep::ApiKeyAndSignIn => format!(
            "Connected Codex: stored the OpenAI API key OpenAI issued with your sign-in as \
             secret {CODEX_SECRET} and your ChatGPT sign-in as secret {CODEX_CHATGPT_SECRET} \
             (no key or token was shown, and {removed})\n  Next: build the Session image with \
             `axocoatl recipe build codex`, then run a loadout whose writer has `runtime: \
             codex`, for example `axocoatl run <loadout> --task \"…\" --model \
             writer=openai:<model>`: its credential is {CODEX_SECRET} unless it names \
             `credential: {CODEX_CHATGPT_SECRET}`. {billed}"
        ),
        Keep::ApiKeyOnly => format!(
            "Connected Codex: stored the OpenAI API key OpenAI issued with your sign-in as \
             secret {CODEX_SECRET} (no key or token was shown, your ChatGPT sign-in was not \
             kept, and {removed})\n  Next: build the Session image with `axocoatl recipe \
             build codex`, then run a loadout whose writer has `runtime: codex` \
             ({CODEX_SECRET} is its credential by default), for example `axocoatl run \
             <loadout> --task \"…\" --model writer=openai:<model>`. {billed}"
        ),
    }
}

/// What the intro says Axocoatl does with the sign-in Codex stores.
pub(crate) fn what_is_kept(keep: Keep) -> String {
    match keep {
        Keep::SignIn => format!("never shows it, stores it as secret {CODEX_CHATGPT_SECRET}"),
        Keep::ApiKeyAndSignIn => format!(
            "never shows it, stores the OpenAI API key OpenAI issues with it as secret \
             {CODEX_SECRET} and the sign-in as secret {CODEX_CHATGPT_SECRET}"
        ),
        Keep::ApiKeyOnly => format!(
            "never shows it, stores only the OpenAI API key OpenAI issues with it, as secret \
             {CODEX_SECRET},"
        ),
    }
}

/// The message when `--api-key` (or `--api-key-only`, `flag`) comes with
/// `--device-auth`.
pub(crate) fn api_key_needs_browser(flag: &str) -> String {
    format!(
        "{flag} cannot be used with --device-auth: OpenAI issues the API key only with the \
         browser sign-in (Codex exchanges that sign-in for a key; the device-code sign-in gets \
         none), so nothing was run. Run `axocoatl connect codex {flag}` without --device-auth, \
         on a computer with a browser."
    )
}

/// The message when Codex's `auth.json` holds no usable API key: none
/// (`malformed` false: OpenAI issued none), or a value that is not one.
pub(crate) fn no_api_key(keep: Keep, malformed: bool) -> String {
    let why = if malformed {
        format!(
            "Codex's auth.json holds an OPENAI_API_KEY that does not have the form of an \
             OpenAI API key (sk-…), so nothing was stored as {CODEX_SECRET}."
        )
    } else {
        format!(
            "OpenAI issued no API key for this account, so nothing was stored as \
             {CODEX_SECRET}. API-key mode needs an OpenAI Platform organization \
             (https://platform.openai.com)."
        )
    };
    let chatgpt = if keep.sign_in() {
        format!(
            "Your ChatGPT sign-in was stored as {CODEX_CHATGPT_SECRET}: ChatGPT mode works with \
             it (a Codex writer with `credential: {CODEX_CHATGPT_SECRET}`)."
        )
    } else {
        format!(
            "Nothing else was stored. ChatGPT mode works with {CODEX_CHATGPT_SECRET}: run \
             `axocoatl connect codex` to store your ChatGPT sign-in."
        )
    };
    format!("{why} {chatgpt}")
}

/// The shortest and longest API key kept.
const MIN_API_KEY_BYTES: usize = 20;
pub(crate) const MAX_API_KEY_BYTES: usize = 1024;

/// Whether `key` has the form of the OpenAI API key Codex writes as
/// `OPENAI_API_KEY`: `sk-`, then only letters, digits, `-` and `_`, 20 to
/// [`MAX_API_KEY_BYTES`] bytes in all (pure).
pub(crate) fn is_api_key(key: &str) -> bool {
    key.starts_with("sk-")
        && (MIN_API_KEY_BYTES..=MAX_API_KEY_BYTES).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The API key in Codex's `auth.json`. No `Debug`: it holds the key.
pub(crate) enum ApiKey {
    /// The key OpenAI issued, in the form [`is_api_key`] accepts.
    Issued(Zeroizing<String>),
    /// None: `OPENAI_API_KEY` is absent, `null` or empty.
    None,
    /// A value that is not an API key.
    Malformed,
}

/// The `OPENAI_API_KEY` in the `auth.json` Codex wrote (pure).
pub(crate) fn api_key_from_auth_json(bytes: &[u8]) -> ApiKey {
    #[derive(serde::Deserialize)]
    struct AuthFile {
        #[serde(default, rename = "OPENAI_API_KEY")]
        openai_api_key: Option<String>,
    }
    let Ok(file) = serde_json::from_slice::<AuthFile>(bytes) else {
        return ApiKey::Malformed;
    };
    let key = Zeroizing::new(file.openai_api_key.unwrap_or_default());
    if key.is_empty() {
        ApiKey::None
    } else if is_api_key(&key) {
        ApiKey::Issued(key)
    } else {
        ApiKey::Malformed
    }
}

/// How `codex login` ended, for the messages.
pub(crate) struct Ended(String);

impl Ended {
    pub(crate) fn from_exit_code(code: Option<u32>) -> Self {
        Self(match code {
            Some(0) => "finished".to_string(),
            Some(code) => format!("exited with status {code}"),
            None => "ended".to_string(),
        })
    }
}

/// What to do after a failed sign-in.
const AGAIN: &str = "Run `axocoatl connect codex` again and finish the sign-in it opens (add \
                     --device-auth on a computer without a browser).";

/// Parse the `auth.json` Codex wrote (`None`: it wrote none), zeroize it,
/// and store what `keep` asks for, under one hold of the secret store's
/// lock: the API key as [`CODEX_SECRET`], the sign-in as
/// [`CODEX_CHATGPT_SECRET`]. Returns the confirmation to print
/// (`container`: Codex ran in a login container). Without an API key when
/// `keep` asks for one, the sign-in is still stored when `keep` asks for it,
/// and the result is [`exit::NO_API_KEY`] ([`no_api_key`]).
pub(crate) fn store_sign_in(
    data_dir: &Path,
    bytes: Option<Zeroizing<Vec<u8>>>,
    ended: &Ended,
    keep: Keep,
    container: bool,
) -> Result<String, Failure> {
    let Some(mut bytes) = bytes else {
        return Err(failure(
            exit::NO_TOKEN,
            format!(
                "codex login {} without storing a sign-in in its CODEX_HOME; nothing was \
                 stored. {AGAIN}",
                ended.0
            ),
        ));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let sign_in = ChatgptSignIn::from_codex_auth_json(&bytes, now);
    let api_key = if keep.api_key() {
        api_key_from_auth_json(&bytes)
    } else {
        ApiKey::None
    };
    bytes.zeroize();
    // The key OpenAI issues with a ChatGPT sign-in: none is taken from a
    // file that holds no such sign-in.
    let sign_in = sign_in.map_err(|reason| {
        failure(
            exit::NO_TOKEN,
            format!("{reason}; nothing was stored. {AGAIN}"),
        )
    })?;
    let encoded =
        if keep.sign_in() {
            Some(sign_in.encode().map_err(|reason| {
                failure(exit::FAILURE, format!("{reason}; nothing was stored"))
            })?)
        } else {
            None
        };
    drop(sign_in);
    let (key, malformed) = match api_key {
        ApiKey::Issued(key) => (Some(key), false),
        ApiKey::None => (None, false),
        ApiKey::Malformed => (None, true),
    };
    let lock = secret_store::lock_secrets(data_dir, true)
        .map_err(|error| failure(exit::FAILURE, format!("storing the sign-in: {error}")))?
        .ok_or_else(|| {
            failure(
                exit::FAILURE,
                "storing the sign-in: the secrets directory could not be created",
            )
        })?;
    if let Some(key) = &key {
        lock.set(CODEX_SECRET, key.as_bytes()).map_err(|error| {
            failure(
                exit::FAILURE,
                format!("storing the API key: {error}; nothing was stored"),
            )
        })?;
    }
    if let Some(encoded) = &encoded {
        lock.set(CODEX_CHATGPT_SECRET, encoded).map_err(|error| {
            let stored = if key.is_some() {
                format!("; the API key was stored as {CODEX_SECRET}")
            } else {
                String::new()
            };
            failure(
                exit::FAILURE,
                format!("storing the sign-in: {error}{stored}"),
            )
        })?;
    }
    drop(lock);
    drop(encoded);
    if keep.api_key() && key.is_none() {
        return Err(failure(exit::NO_API_KEY, no_api_key(keep, malformed)));
    }
    drop(key);
    Ok(connected_message(keep, container))
}

/// Run `codex login` with a private `CODEX_HOME` (on this computer or in a
/// login container) on the terminal `input`/`output`, read the sign-in
/// Codex stored there, store it and erase the `CODEX_HOME`. Returns the
/// confirmation to print.
pub(crate) async fn connect(
    request: ConnectCodex,
    input: OwnedFd,
    output: Box<dyn std::io::Write + Send>,
    interrupt: &Interrupt,
    resized: &Resized,
) -> Result<String, Failure> {
    let ConnectCodex {
        runner,
        data_dir,
        device_auth,
        keep,
    } = request;
    if device_auth && keep.api_key() {
        return Err(failure(exit::USAGE, api_key_needs_browser(keep.flag())));
    }
    match runner {
        Runner::Host { codex } => {
            connect_host(
                codex,
                data_dir,
                device_auth,
                keep,
                input,
                output,
                interrupt,
                resized,
            )
            .await
        }
        Runner::Container(login) => {
            super::codex_container::connect(
                login,
                data_dir,
                device_auth,
                keep,
                input,
                output,
                interrupt,
                resized,
            )
            .await
        }
    }
}

/// [`connect`] with a `codex` on this computer.
#[allow(clippy::too_many_arguments)]
async fn connect_host(
    codex: PathBuf,
    data_dir: PathBuf,
    device_auth: bool,
    keep: Keep,
    input: OwnedFd,
    output: Box<dyn std::io::Write + Send>,
    interrupt: &Interrupt,
    resized: &Resized,
) -> Result<String, Failure> {
    erase_leftovers(&data_dir);
    let home = TempHome::create(&data_dir).map_err(|error| {
        failure(
            exit::FAILURE,
            format!("could not make a private temporary CODEX_HOME in the data directory: {error}"),
        )
    })?;
    let flag = interrupt.flag.clone();
    let resized = resized.0.clone();
    let home_path = home.path().to_path_buf();
    let run = tokio::task::spawn_blocking(move || {
        let mut output = output;
        let args = login_args(device_auth);
        let set_env = [("CODEX_HOME", home_path.as_os_str())];
        relay::run_program(
            &relay::Program {
                path: &codex,
                args: &args,
                cwd: Some(&home_path),
                set_env: &set_env,
                remove_env: &REMOVED_ENV,
                filters: super::redact::FilterChain::codex,
                // Codex on this computer opens the browser sign-in itself.
                open_sign_in: None,
                hold_error_line: false,
            },
            input.as_fd(),
            &mut *output,
            &flag,
            &resized,
        )
    })
    .await
    .map_err(|_| failure(exit::FAILURE, "running codex login failed"))?
    .map_err(|error| failure(exit::FAILURE, error))?;
    if run.interrupted {
        return Err(failure(
            exit::INTERRUPTED,
            "interrupted: codex login was stopped, nothing was stored and the temporary \
             CODEX_HOME was erased",
        ));
    }
    let ended = Ended::from_exit_code(run.exit_code);
    // Whatever the filters hid on screen is not used: the sign-in is read
    // from the file Codex wrote.
    drop(run);
    let bytes = read_auth_file(home.path()).map_err(|error| failure(exit::FAILURE, error))?;
    let stored = store_sign_in(&data_dir, bytes, &ended, keep, false);
    drop(home);
    stored
}

/// What to do to sign in, for the intro. A Codex on this computer may open
/// the browser itself; Codex in the login container cannot open one here,
/// so Axocoatl opens the page it prints when `opens` (an opener was found
/// and `--no-browser` was not given).
pub(crate) fn sign_in_how(device_auth: bool, in_container: bool, opens: bool) -> &'static str {
    match (device_auth, in_container && opens) {
        (true, true) => {
            "Axocoatl opens the address it gives in your browser; enter the code it shows \
             there (or at that address on any device)."
        }
        (true, false) => "Enter the code it shows at the address it gives, on any device.",
        (false, true) => {
            "Axocoatl opens the address it prints in your browser; if no browser opens, open \
             that address in a browser on this computer."
        }
        (false, false) if in_container => {
            "Open the address it prints in a browser on this computer."
        }
        (false, false) => {
            "Sign in in the browser it opens, or open the address it prints in a browser on \
             this computer."
        }
    }
}

/// The message when standard input or output is not a terminal.
pub(crate) fn needs_terminal() -> String {
    "axocoatl connect codex needs an interactive terminal: it runs `codex login`, which signs \
     you in through the browser (or, with --device-auth, shows a code to enter on another \
     device). Run it in a terminal; `axocoatl connect codex --api-key` also stores the \
     OpenAI API key OpenAI issues with the browser sign-in. Only for automation without a \
     browser, `axocoatl secret set codex-openai --from-env VAR` stores a key that is already \
     in the environment variable VAR."
        .to_string()
}
