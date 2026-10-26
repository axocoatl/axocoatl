//! `axocoatl connect codex` without a `codex` on this computer: the pinned
//! official Codex ([`CODEX_VERSION`], the one the `codex` recipe installs)
//! runs `codex login` in a throwaway container of the `codex` recipe image,
//! through the configured Podman connection.
//!
//! The image is the one `axocoatl recipe build codex` recorded in the data
//! root, by its exact image id; when there is none (or Podman no longer has
//! it), it is built with the same recipe machinery first. The container:
//!
//! - runs detached with `--rm` and `--timeout` set to the sign-in time
//!   (`--sign-in-timeout`, 1 to [`MAX_SIGN_IN_MINUTES`] minutes,
//!   [`DEFAULT_SIGN_IN_MINUTES`] by default), so Podman stops and removes it
//!   even when Axocoatl is killed, and Axocoatl removes it (`podman rm
//!   --force`) on every path ([`Container`]);
//! - has a private `CODEX_HOME` on a `0700` tmpfs ([`CONTAINER_HOME`]), file
//!   credential storage forced, a read-only root file system, no
//!   capabilities, no new privileges, a non-root user, no log driver, no
//!   host mount of any kind and no host variable (`--http-proxy=false`: not
//!   even Podman's proxy variables);
//! - is on Podman's default bridge network, not Axocoatl's egress routes:
//!   `codex login` reaches `auth.openai.com` directly, the way it would on
//!   the host. It holds no Axocoatl secret;
//! - for the browser sign-in, publishes one port, on this computer's
//!   loopback only: `127.0.0.1:1455` (the redirect address Codex prints,
//!   `http://localhost:1455/auth/callback`) to the container's
//!   [`FORWARD_PORT`], where a small Node forwarder passes each connection
//!   to `127.0.0.1:1455` in the container, where Codex listens. In the
//!   container's own network namespace nothing else holds 1455, so Codex
//!   always binds it and never falls back to another port. With
//!   `--device-auth` no port is published.
//!
//! `codex login` runs with `podman exec --interactive --tty` in the same
//! pseudo-terminal relay and filters as the host program. Codex cannot open
//! a browser on this computer from the container, so unless `--no-browser`
//! the relay opens the sign-in page it prints here, once
//! ([`super::browser`]). The sign-in is
//! then read with `podman exec … head` straight into a zeroizing buffer in
//! this process: Axocoatl never writes `auth.json` to disk, and the only
//! other copy, Codex's own on the tmpfs, goes with the container. With
//! `--api-key`, the OpenAI API key Codex wrote there is taken from that
//! buffer too ([`super::codex::store_sign_in`]). When the
//! sign-in cannot be read because the container has ended after its
//! lifetime, the user is told the sign-in was not completed in time
//! ([`timed_out`]), and the error `podman exec` printed about the exec
//! session it lost with the container ([`lost_exec_session`]), held back by
//! the relay as its last line, is not shown.
//!
//! The `podman` commands that start, read and remove the container
//! ([`podman_command`]) each run in a process group of their own and with
//! `SIGINT`, `SIGQUIT` and `SIGHUP` ignored (the `podman exec` of `codex
//! login` runs in the relay's own session), so a Ctrl-C, Ctrl-\ or terminal
//! hangup, which Axocoatl catches, cannot stop them, not even while one is
//! being started.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use axocoatl_daemon::external_agent::codex::CODEX_VERSION;
use axocoatl_daemon::external_agent::codex_chatgpt::MAX_AUTH_FILE_BYTES;
use axocoatl_daemon::external_agent::recipe_images;
use axocoatl_isolation::recipes;
use zeroize::Zeroizing;

use super::browser::{Opener, SignInPage};
use super::codex::{login_args, store_sign_in, Ended, Keep};
use super::{exit, failure, relay, Failure, Interrupt, Resized};

/// The recipe whose image runs `codex login`.
pub(crate) const RECIPE: &str = "codex";
/// The port Codex's login server listens on (in the container) and the
/// port its redirect address names (on this computer).
pub(crate) const CALLBACK_PORT: u16 = 1455;
/// The container port the forwarder listens on: not 1455, which Codex
/// binds on the container's loopback, and not a port Codex tries instead.
pub(crate) const FORWARD_PORT: u16 = 14550;
/// The container's private `CODEX_HOME` (a tmpfs).
pub(crate) const CONTAINER_HOME: &str = "/codex-home";
/// The label of every login container.
pub(crate) const ROLE_LABEL: &str = "io.axocoatl.role=connect-codex";
/// Login containers' names: this, then a random part.
pub(crate) const NAME_PREFIX: &str = "axocoatl-connect-codex-";
/// Without `--sign-in-timeout`, Podman stops (and so removes) a login
/// container this many minutes after it starts, whatever happens to
/// Axocoatl: longer than a sign-in takes (a device code expires after 15
/// minutes).
pub(crate) const DEFAULT_SIGN_IN_MINUTES: u64 = 30;
/// The longest `--sign-in-timeout`, in minutes.
pub(crate) const MAX_SIGN_IN_MINUTES: u64 = 60;
/// [`DEFAULT_SIGN_IN_MINUTES`] in seconds.
pub(crate) const DEFAULT_LIFETIME_SECS: u64 = DEFAULT_SIGN_IN_MINUTES * 60;
/// [`DEFAULT_LIFETIME_SECS`] as a duration.
pub(crate) const DEFAULT_LIFETIME: Duration = Duration::from_secs(DEFAULT_LIFETIME_SECS);

/// The login container's lifetime for `--sign-in-timeout minutes` (1 to
/// [`MAX_SIGN_IN_MINUTES`]; clap refuses any other value first), or the
/// default without it.
pub(crate) fn sign_in_lifetime(minutes: Option<u64>) -> Duration {
    minutes.map_or(DEFAULT_LIFETIME, |minutes| {
        Duration::from_secs(minutes.clamp(1, MAX_SIGN_IN_MINUTES) * 60)
    })
}
/// The user the container runs as (`node` in the recipe image's base).
pub(crate) const CONTAINER_USER: &str = "1000:1000";

/// The forwarder: each connection to [`FORWARD_PORT`] is piped to Codex's
/// login server on the container's loopback.
pub(crate) const FORWARDER: &str = "const net = require('net'); \
net.createServer((client) => { \
const server = net.connect(1455, '127.0.0.1'); \
client.pipe(server).pipe(client); \
client.on('error', () => server.destroy()); \
server.on('error', () => client.destroy()); \
}).listen(14550, '0.0.0.0');";

/// Exit status of the read script when there is no `auth.json`.
const READ_NONE: i32 = 3;
/// Exit status of the read script when `auth.json` is not a regular file.
const READ_NOT_FILE: i32 = 4;
/// How long a failed read waits for Podman to finish stopping and removing
/// a login container whose lifetime has passed ([`Container::read_failure`]):
/// for about three seconds after its `--timeout`, Podman 6.1 still lists it
/// (`container exists`) while `container inspect` no longer finds it. Short
/// in this crate's unit tests, whose fake `podman` never settles in one case.
const SETTLE: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });
/// How often [`Container::read_failure`] asks Podman again while it waits.
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// A new container name.
pub(crate) fn container_name() -> String {
    format!("{NAME_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// The `podman run` arguments that start the login container `name` from
/// `image`, stopped by Podman after `lifetime` (at least a second: Podman
/// reads `--timeout=0` as none) (pure).
pub(crate) fn run_args(
    name: &str,
    image: &str,
    device_auth: bool,
    lifetime: Duration,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--detach",
        "--rm",
        "--name",
        name,
        "--label",
        ROLE_LABEL,
        "--pull=never",
        "--network=bridge",
        "--read-only",
        "--cap-drop=all",
        "--security-opt=no-new-privileges",
        "--log-driver=none",
        "--pids-limit=256",
        // Podman would otherwise copy its own `http_proxy`, `https_proxy`,
        // `ftp_proxy` and `no_proxy` (either case) into the container.
        "--http-proxy=false",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    args.push(format!("--timeout={}", lifetime.as_secs().max(1)));
    args.push(format!("--user={CONTAINER_USER}"));
    args.push(format!(
        "--mount=type=tmpfs,destination={CONTAINER_HOME},tmpfs-mode=0700,tmpfs-size=16m,U=true"
    ));
    args.push("--tmpfs=/tmp:rw,nosuid,nodev,size=64m,mode=1777".into());
    args.push(format!("--env=CODEX_HOME={CONTAINER_HOME}"));
    args.push(format!("--env=HOME={CONTAINER_HOME}"));
    args.push(format!("--workdir={CONTAINER_HOME}"));
    if device_auth {
        args.extend(["--entrypoint=sleep".into(), image.into(), "infinity".into()]);
    } else {
        args.push(format!(
            "--publish=127.0.0.1:{CALLBACK_PORT}:{FORWARD_PORT}/tcp"
        ));
        args.extend([
            "--entrypoint=node".into(),
            image.into(),
            "-e".into(),
            FORWARDER.into(),
        ]);
    }
    args
}

/// The `podman exec` arguments that run `codex login` in `name` on the
/// relayed terminal (pure).
pub(crate) fn login_exec_args(name: &str, device_auth: bool) -> Vec<String> {
    let mut args: Vec<String> = ["exec", "--interactive", "--tty", name, "codex"]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    args.extend(login_args(device_auth).into_iter().map(str::to_string));
    args
}

/// The `podman exec` arguments that print `auth.json` from `name`: at most
/// one byte more than [`MAX_AUTH_FILE_BYTES`], only a regular file (no
/// link), exit status [`READ_NONE`] without one (pure).
pub(crate) fn read_args(name: &str) -> Vec<String> {
    let script = format!(
        "[ -e \"$1\" ] || [ -L \"$1\" ] || exit {READ_NONE}; \
         [ -f \"$1\" ] && [ ! -L \"$1\" ] || exit {READ_NOT_FILE}; \
         exec head -c {} -- \"$1\"",
        MAX_AUTH_FILE_BYTES + 1
    );
    vec![
        "exec".into(),
        name.into(),
        "sh".into(),
        "-c".into(),
        script,
        "sh".into(),
        format!("{CONTAINER_HOME}/auth.json"),
    ]
}

/// The `podman rm` arguments that remove `name` and its anonymous volumes
/// at once, if it still exists (pure).
pub(crate) fn remove_args(name: &str) -> Vec<String> {
    ["rm", "--force", "--time=0", "--volumes", "--ignore", name]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect()
}

/// The `podman` arguments that tell whether `name` still exists: exit
/// status 0 when it does, 1 when it does not (pure).
pub(crate) fn exists_args(name: &str) -> Vec<String> {
    ["container", "exists", name]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect()
}

/// The `podman` arguments that print whether `name` is running: `true` or
/// `false` (pure).
pub(crate) fn running_args(name: &str) -> Vec<String> {
    [
        "container",
        "inspect",
        "--format",
        "{{.State.Running}}",
        name,
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect()
}

/// `lifetime` in words: whole minutes, else seconds.
fn in_words(lifetime: Duration) -> String {
    let seconds = lifetime.as_secs();
    let (count, unit) = if seconds >= 60 && seconds.is_multiple_of(60) {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

/// The intro's line on how long the sign-in may take.
pub(crate) fn time_to_sign_in(lifetime: Duration) -> String {
    format!("You have {} to finish signing in.", in_words(lifetime))
}

/// The message when the login container ended after `lifetime` (Podman's
/// `--timeout`) before Codex's sign-in was read.
pub(crate) fn timed_out(lifetime: Duration) -> String {
    format!(
        "The Codex sign-in was not completed within {}; nothing was stored. Run `axocoatl \
         connect codex` again.",
        in_words(lifetime)
    )
}

/// Whether `line` is what `podman exec` prints when the container its
/// session ran in has gone under it (Podman stops and removes the login
/// container at its `--timeout`): one line of plain text, `Error:
/// retrieving exec session ID from container ID: … no such container` (or
/// `… no such exec session`), or `Error: no exec session with ID ID found`,
/// with or without Podman's `: no such exec session` after it, or
/// `Error: container has already been removed` (pure).
pub(crate) fn lost_exec_session(line: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let text = text.trim_end_matches(['\r', '\n']);
    if text.chars().any(char::is_control) {
        return false;
    }
    let Some(said) = text.strip_prefix("Error: ") else {
        return false;
    };
    let gone = said.ends_with(": no such container") || said.ends_with(": no such exec session");
    said == "container has already been removed"
        || (said.starts_with("retrieving exec session ") && gone)
        || (said.starts_with("no exec session with ID ") && (gone || said.ends_with(" found")))
}

/// The message when the login container ended before its lifetime and
/// before Codex's sign-in was read.
pub(crate) fn ended_early() -> String {
    "The login container stopped before Codex's sign-in could be read (it was removed, or \
     Podman stopped it); nothing was stored. Run `axocoatl connect codex` again."
        .to_string()
}

/// The message when this computer's `127.0.0.1:1455` (or `[::1]:1455`) is
/// taken.
pub(crate) fn port_in_use() -> String {
    format!(
        "port {CALLBACK_PORT} on this computer is in use (another sign-in, or another \
         program), so the browser sign-in cannot return to Codex; nothing was run. Stop what \
         uses it, or sign in with a code instead: `axocoatl connect codex --device-auth`."
    )
}

/// Whether the browser sign-in's port is free on this computer's loopback:
/// a listener on `127.0.0.1` or `[::1]` would take the browser's redirect.
pub(crate) fn check_callback_port(port: u16) -> Result<(), String> {
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        return Err(port_in_use());
    }
    match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Err(port_in_use()),
        // No IPv6 loopback here: nothing can listen there either.
        _ => Ok(()),
    }
}

/// The `podman` to run: the first on `path`, as an absolute path. Only
/// `PATH` is searched, because building the recipe image runs the `podman`
/// on `PATH` too.
pub(crate) fn find_podman(path: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    super::find_program(
        "podman",
        None,
        path,
        None,
        "Podman (`podman`) was not found on PATH. `axocoatl connect codex` runs the pinned \
         Codex in a container through Podman (https://podman.io); install it (or add it to \
         PATH), or pass a Codex CLI on this computer with --codex PATH",
    )
}

/// A `podman` command in its own process group, started with `SIGINT`,
/// `SIGQUIT` and `SIGHUP` ignored. Once the relay gives the terminal back,
/// a Ctrl-C, Ctrl-\ or the hangup when the terminal closes goes to
/// Axocoatl's process group, where Axocoatl catches it and goes on to
/// remove the container; in that group it would kill these commands, above
/// all the `podman rm` that removes the container and the sign-in in it.
/// None of them uses the terminal.
///
/// Its own group alone is not enough: a new process is in Axocoatl's group
/// until it has moved to its own, and with `posix_spawn` (which `std` uses
/// for a plain command) a signal sent to the group in that moment is
/// delivered only after `exec` has reset Axocoatl's handler to the default
/// action, which kills the command before it runs. With a `pre_exec`
/// closure `std` forks instead: until the child has moved to its own group
/// it still has Axocoatl's handlers, which only note the signal, and then
/// it ignores these signals (dropping one pending) before `exec`, which
/// keeps them ignored.
pub(crate) fn podman_command(podman: &Path) -> Command {
    let mut command = Command::new(podman);
    command.process_group(0);
    // SAFETY: between `fork` and `exec` the closure only calls `signal`,
    // which is async-signal-safe, and allocates nothing.
    unsafe {
        command.pre_exec(|| {
            for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP] {
                if libc::signal(signal, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
}

/// The image id of the `codex` recipe image recorded in `data_dir` that
/// Podman still has; built and recorded first when there is none. Podman's
/// build output reaches the terminal.
pub(crate) async fn recipe_image(data_dir: &Path) -> Result<String, Failure> {
    let names = vec![RECIPE.to_string()];
    let image =
        recipes::image_name(&names).map_err(|error| failure(exit::FAILURE, error.to_string()))?;
    let recorded = recipe_images::recorded_images_at(data_dir)
        .map_err(|error| failure(exit::FAILURE, error.to_string()))?
        .into_iter()
        .find(|record| record.image == image);
    if let Some(record) = recorded {
        match axocoatl_daemon::browser_install::image_present(&record.image_id).await {
            Ok(true) => return Ok(record.image_id),
            Ok(false) => {}
            Err(error) => return Err(failure(exit::FAILURE, error)),
        }
    }
    eprintln!(
        "Building {image}, the codex recipe image (Codex {CODEX_VERSION}), with Podman first. \
         This downloads the pinned base image and the pinned Codex package once.\n"
    );
    let record = axocoatl_daemon::browser_install::build_recipe_image(&names)
        .await
        .map_err(|error| failure(exit::FAILURE, format!("building {image}: {error}")))?;
    recipe_images::record_image_at(data_dir, record.clone()).map_err(|error| {
        failure(
            exit::FAILURE,
            format!("{image} was built but could not be recorded: {error}"),
        )
    })?;
    eprintln!();
    Ok(record.image_id)
}

/// What `connect codex` runs `codex login` with in a login container.
pub(crate) struct Login {
    pub(crate) podman: PathBuf,
    /// The `codex` recipe image's id.
    pub(crate) image: String,
    /// How long Podman lets the container live ([`sign_in_lifetime`]).
    pub(crate) lifetime: Duration,
    /// What opens the sign-in page on this computer (`None` with
    /// `--no-browser`, or when there is no opener).
    pub(crate) opener: Option<Opener>,
}

impl Login {
    pub(crate) fn new(
        podman: PathBuf,
        image: String,
        lifetime: Duration,
        opener: Option<Opener>,
    ) -> Self {
        Self {
            podman,
            image,
            lifetime,
            opener,
        }
    }
}

/// Why Codex's sign-in could not be read from a login container.
pub(crate) struct ReadFailure {
    pub(crate) failure: Failure,
    /// Whether the container had ended after its lifetime ([`timed_out`]).
    pub(crate) timed_out: bool,
}

/// Whether a login container is still there to read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Running,
    /// Stopped, or removed (with `--rm`, Podman removes it once it stops).
    Ended,
    /// Podman could not say.
    Unknown,
}

/// A started login container: removed (with `podman rm --force`) when
/// dropped.
pub(crate) struct Container {
    podman: PathBuf,
    name: String,
    /// When `podman run` was started, before the container was: Podman
    /// stops it `lifetime` after this at the earliest.
    started: Instant,
    started_at: SystemTime,
    lifetime: Duration,
}

impl Container {
    /// Start the login container from `image`, for at most `lifetime`.
    pub(crate) fn start(
        podman: &Path,
        image: &str,
        device_auth: bool,
        lifetime: Duration,
    ) -> Result<Self, Failure> {
        let name = container_name();
        // Removed from here on, also if `podman run` failed half way.
        let container = Self {
            podman: podman.to_path_buf(),
            name,
            started: Instant::now(),
            started_at: SystemTime::now(),
            lifetime,
        };
        let output = podman_command(podman)
            .args(run_args(&container.name, image, device_auth, lifetime))
            .stdin(Stdio::null())
            .output()
            .map_err(|error| failure(exit::FAILURE, format!("running podman: {error}")))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let lowered = said.to_ascii_lowercase();
            if !device_auth
                && (lowered.contains("address already in use")
                    || lowered.contains("port is already"))
            {
                return Err(failure(exit::USAGE, port_in_use()));
            }
            return Err(failure(
                exit::FAILURE,
                format!("could not start the login container: {said}"),
            ));
        }
        Ok(container)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Whether the container's lifetime has passed, so Podman has stopped
    /// it: by the monotonic clock, or by the wall clock, which also counts
    /// time this computer slept.
    pub(crate) fn outlived(&self) -> bool {
        self.started.elapsed() >= self.lifetime
            || self
                .started_at
                .elapsed()
                .is_ok_and(|elapsed| elapsed >= self.lifetime)
    }

    /// Whether the container is still running, has ended, or Podman cannot
    /// say.
    pub(crate) fn state(&self) -> State {
        let exists = podman_command(&self.podman)
            .args(exists_args(&self.name))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match exists.ok().and_then(|status| status.code()) {
            Some(0) => {}
            Some(1) => return State::Ended,
            _ => return State::Unknown,
        }
        let running = podman_command(&self.podman)
            .args(running_args(&self.name))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match running {
            Ok(output) if output.status.success() => {
                match String::from_utf8_lossy(&output.stdout).trim() {
                    "true" => State::Running,
                    "false" => State::Ended,
                    _ => State::Unknown,
                }
            }
            _ => State::Unknown,
        }
    }

    /// Why `auth.json` could not be read (`message`): the lifetime ran out
    /// ([`timed_out`], exit code [`exit::NO_TOKEN`]), the container ended
    /// before that ([`ended_early`]), or `message` itself. Once the lifetime
    /// has passed, a container Podman still shows running, or cannot
    /// describe, is given up to [`SETTLE`] to end first.
    pub(crate) fn read_failure(&self, message: String) -> ReadFailure {
        let mut state = self.state();
        if self.outlived() {
            let deadline = Instant::now() + SETTLE;
            while state != State::Ended && Instant::now() < deadline {
                std::thread::sleep(SETTLE_POLL);
                state = self.state();
            }
        }
        let (failure, timed_out) = match state {
            State::Ended if self.outlived() => {
                (failure(exit::NO_TOKEN, timed_out(self.lifetime)), true)
            }
            State::Ended => (failure(exit::FAILURE, ended_early()), false),
            State::Running | State::Unknown => (failure(exit::FAILURE, message), false),
        };
        ReadFailure { failure, timed_out }
    }

    /// `auth.json` from the container's `CODEX_HOME`, read straight into a
    /// zeroizing buffer of at most [`MAX_AUTH_FILE_BYTES`] bytes. `None`
    /// when Codex wrote none.
    pub(crate) fn read_auth_file(&self) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        let mut child = podman_command(&self.podman)
            .args(read_args(&self.name))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("reading Codex's auth.json: running podman: {error}"))?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_AUTH_FILE_BYTES + 1));
        let read = child
            .stdout
            .take()
            .map(|stdout| {
                stdout
                    .take(MAX_AUTH_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .transpose();
        let status = child
            .wait()
            .map_err(|error| format!("reading Codex's auth.json: {error}"))?;
        read.map_err(|error| format!("Codex's auth.json cannot be read: {error}"))?;
        match status.code() {
            Some(0) if bytes.len() <= MAX_AUTH_FILE_BYTES => Ok(Some(bytes)),
            Some(0) | Some(READ_NOT_FILE) => Err(format!(
                "Codex's auth.json is not a regular file of at most {MAX_AUTH_FILE_BYTES} bytes"
            )),
            Some(READ_NONE) => Ok(None),
            _ => Err(format!(
                "Codex's auth.json could not be read from the login container ({status})"
            )),
        }
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = podman_command(&self.podman)
            .args(remove_args(&self.name))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Run `codex login` in a login container (`login`) on the terminal
/// `input`/`output`, opening the sign-in page it prints on this computer
/// once (when `login` has an opener), read the sign-in Codex stored in its
/// `CODEX_HOME`, store what `keep` asks for, and remove the container (on
/// every path).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect(
    login: Login,
    data_dir: PathBuf,
    device_auth: bool,
    keep: Keep,
    input: OwnedFd,
    output: Box<dyn std::io::Write + Send>,
    interrupt: &Interrupt,
    resized: &Resized,
) -> Result<String, Failure> {
    let Login {
        podman,
        image,
        lifetime,
        opener,
    } = login;
    let container = {
        let podman = podman.clone();
        tokio::task::spawn_blocking(move || {
            Container::start(&podman, &image, device_auth, lifetime)
        })
        .await
        .map_err(|_| failure(exit::FAILURE, "starting the login container failed"))??
    };
    let flag = interrupt.flag.clone();
    let resized = resized.0.clone();
    let args = login_exec_args(container.name(), device_auth);
    let (run, mut output) = tokio::task::spawn_blocking(move || {
        let mut output = output;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = relay::run_program(
            &relay::Program {
                path: &podman,
                args: &args,
                cwd: None,
                set_env: &[],
                remove_env: &[],
                filters: super::redact::FilterChain::codex,
                open_sign_in: opener
                    .as_ref()
                    .map(|opener| (SignInPage::for_sign_in(device_auth), opener)),
                // Podman's own error is the last line `podman exec` prints.
                hold_error_line: true,
            },
            input.as_fd(),
            &mut *output,
            &flag,
            &resized,
        );
        (run, output)
    })
    .await
    .map_err(|_| failure(exit::FAILURE, "running codex login failed"))?;
    let mut run = run.map_err(|error| failure(exit::FAILURE, error))?;
    if run.interrupted {
        drop(container);
        return Err(failure(
            exit::INTERRUPTED,
            "interrupted: codex login was stopped, nothing was stored and the login container \
             was removed",
        ));
    }
    let ended = Ended::from_exit_code(run.exit_code);
    let held_line = std::mem::take(&mut run.held_line);
    // Whatever the filters hid on screen is not used: the sign-in is read
    // from the file Codex wrote.
    drop(run);
    let read = tokio::task::spawn_blocking(move || {
        let read = container
            .read_auth_file()
            .map_err(|message| container.read_failure(message));
        drop(container);
        read
    })
    .await
    .map_err(|_| failure(exit::FAILURE, "reading Codex's auth.json failed"))?;
    // `podman exec`'s own error about the session the container's lifetime
    // ended is not shown: the message says what happened. Any other last
    // line is.
    let timed_out = matches!(
        &read,
        Err(ReadFailure {
            timed_out: true,
            ..
        })
    );
    if !(timed_out && lost_exec_session(&held_line)) {
        let _ = output.write_all(&held_line);
        let _ = output.flush();
    }
    drop(output);
    let bytes = read.map_err(|read| read.failure)?;
    store_sign_in(&data_dir, bytes, &ended, keep, true)
}
