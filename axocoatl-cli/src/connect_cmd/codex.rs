//! `axocoatl connect codex`: sign in to ChatGPT with the official Codex CLI
//! and store the sign-in, without anyone seeing, copying or typing a token.
//!
//! The command makes a private, temporary `CODEX_HOME` (`0700`) in the data
//! root and runs `codex -c cli_auth_credentials_store="file" login` there
//! (`--device-auth` with `--device-auth`), in the same pseudo-terminal relay
//! as `connect claude-code` ([`super::relay`]): the user signs in in the
//! browser, or enters the code Codex shows. Its output reaches the user's
//! terminal through the same filters, which also hide OpenAI API keys and
//! JWTs ([`super::redact::FilterChain::codex`]); Codex 0.160.1 prints
//! neither, it writes the tokens to `auth.json`. Axocoatl reads them from
//! that file, never from the screen
//! (`axocoatl_daemon::external_agent::codex_chatgpt::ChatgptSignIn`),
//! stores the access token, refresh token and account id as the secret
//! `codex-chatgpt` (same store and rules as `axocoatl secret set`: `0600`,
//! replaced atomically), and then overwrites every file in the temporary
//! home with zeros and removes it, also on an error or a signal (a release
//! build aborts on a panic, and a killed or aborted run leaves the home for
//! the next run to erase, [`erase_leftovers`]).
//!
//! Codex runs with that `CODEX_HOME`, in that directory (so no repository's
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

/// What `connect codex` needs besides the terminal.
pub(crate) struct ConnectCodex {
    pub(crate) codex: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) device_auth: bool,
}

/// A private temporary `CODEX_HOME`: every file in it is overwritten with
/// zeros and the whole directory removed when it is dropped.
pub(crate) struct TempHome {
    path: PathBuf,
}

impl TempHome {
    /// A new `0700` directory in `data_dir`.
    pub(crate) fn create(data_dir: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = data_dir.join(format!(
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

/// The line printed once the sign-in is stored, with the next step.
pub(crate) fn connected_message() -> String {
    format!(
        "Connected Codex: stored your ChatGPT sign-in as secret {CODEX_CHATGPT_SECRET} (no token \
         was shown, and the temporary CODEX_HOME was erased)\n  Next: build the Session image \
         with `axocoatl recipe build codex`, then run a loadout whose writer has `runtime: \
         codex` and `credential: {CODEX_CHATGPT_SECRET}`, for example `axocoatl run <loadout> \
         --task \"…\" --model writer=openai:<model>`. The daemon refreshes the access token on \
         this computer; it never enters the Session container."
    )
}

/// Run `codex login` with a private temporary `CODEX_HOME` on the terminal
/// `input`/`output`, read the sign-in Codex stored there, store it and
/// erase the temporary home. Returns the confirmation to print.
pub(crate) async fn connect(
    request: ConnectCodex,
    input: OwnedFd,
    output: Box<dyn std::io::Write + Send>,
    interrupt: &Interrupt,
    resized: &Resized,
) -> Result<String, Failure> {
    let ConnectCodex {
        codex,
        data_dir,
        device_auth,
    } = request;
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
    let ended = match run.exit_code {
        Some(0) => "finished".to_string(),
        Some(code) => format!("exited with status {code}"),
        None => "ended".to_string(),
    };
    // Whatever the filters hid on screen is not used: the sign-in is read
    // from the file Codex wrote.
    drop(run);
    let again = "Run `axocoatl connect codex` again and finish the sign-in it opens (add \
                 --device-auth on a computer without a browser).";
    let mut bytes = match read_auth_file(home.path()) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Err(failure(
                exit::NO_TOKEN,
                format!(
                    "codex login {ended} without storing a sign-in in the temporary CODEX_HOME; \
                     nothing was stored. {again}"
                ),
            ))
        }
        Err(error) => return Err(failure(exit::FAILURE, error)),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let sign_in = ChatgptSignIn::from_codex_auth_json(&bytes, now);
    bytes.zeroize();
    let sign_in = sign_in.map_err(|reason| {
        failure(
            exit::NO_TOKEN,
            format!("{reason}; nothing was stored. {again}"),
        )
    })?;
    let encoded = sign_in
        .encode()
        .map_err(|reason| failure(exit::FAILURE, format!("{reason}; nothing was stored")))?;
    drop(sign_in);
    secret_store::set_secret(&data_dir, CODEX_CHATGPT_SECRET, &encoded)
        .map_err(|error| failure(exit::FAILURE, format!("storing the sign-in: {error}")))?;
    drop(encoded);
    drop(home);
    Ok(connected_message())
}

/// The message when standard input or output is not a terminal.
pub(crate) fn needs_terminal() -> String {
    "axocoatl connect codex needs an interactive terminal: it runs `codex login`, which signs \
     you in through the browser (or, with --device-auth, shows a code to enter on another \
     device). Run it in a terminal. Without a ChatGPT plan, store an OpenAI API key instead \
     with `axocoatl secret set codex-openai --from-env OPENAI_API_KEY`."
        .to_string()
}
