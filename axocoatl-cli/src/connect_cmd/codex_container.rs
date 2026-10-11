//! `axocoatl connect codex` without a `codex` on this computer: the pinned
//! official Codex ([`CODEX_VERSION`], the one the `codex` recipe installs)
//! runs `codex login` in a throwaway container of the `codex` recipe image,
//! through the configured Podman connection.
//!
//! The image is the one `axocoatl recipe build codex` recorded in the data
//! root, by its exact image id; when there is none (or Podman no longer has
//! it), it is built with the same recipe machinery first. The container:
//!
//! - runs detached with `--rm` and `--timeout` [`MAX_LIFETIME_SECS`], so
//!   Podman stops and removes it even when Axocoatl is killed, and Axocoatl
//!   removes it (`podman rm --force`) on every path ([`Container`]);
//! - has a private `CODEX_HOME` on a `0700` tmpfs ([`CONTAINER_HOME`]), file
//!   credential storage forced, a read-only root file system, no
//!   capabilities, no new privileges, a non-root user, no log driver and no
//!   host mount of any kind;
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
//! pseudo-terminal relay and filters as the host program. The sign-in is
//! then read with `podman exec … head` straight into a zeroizing buffer in
//! this process: Axocoatl never writes `auth.json` to disk, and the only
//! other copy, Codex's own on the tmpfs, goes with the container.

use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axocoatl_daemon::external_agent::codex::CODEX_VERSION;
use axocoatl_daemon::external_agent::codex_chatgpt::MAX_AUTH_FILE_BYTES;
use axocoatl_daemon::external_agent::recipe_images;
use axocoatl_isolation::recipes;
use zeroize::Zeroizing;

use super::codex::{login_args, store_sign_in, Ended};
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
/// Podman stops (and so removes) a login container this long after it
/// starts, whatever happens to Axocoatl: longer than a sign-in takes (a
/// device code expires after 15 minutes).
pub(crate) const MAX_LIFETIME_SECS: u64 = 30 * 60;
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

/// A new container name.
pub(crate) fn container_name() -> String {
    format!("{NAME_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// The `podman run` arguments that start the login container `name` from
/// `image` (pure).
pub(crate) fn run_args(name: &str, image: &str, device_auth: bool) -> Vec<String> {
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
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    args.push(format!("--timeout={MAX_LIFETIME_SECS}"));
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

/// The `podman` to run, as an absolute path.
pub(crate) fn find_podman(
    path: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Result<PathBuf, String> {
    super::find_program(
        "podman",
        None,
        path,
        home,
        "Podman (`podman`) was not found on PATH or in ~/.local/bin. `axocoatl connect codex` \
         runs the pinned Codex in a container through Podman (https://podman.io); install it, \
         or pass a Codex CLI on this computer with --codex PATH",
    )
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

/// A started login container: removed (with `podman rm --force`) when
/// dropped.
pub(crate) struct Container {
    podman: PathBuf,
    name: String,
}

impl Container {
    /// Start the login container from `image`.
    pub(crate) fn start(podman: &Path, image: &str, device_auth: bool) -> Result<Self, Failure> {
        let name = container_name();
        // Removed from here on, also if `podman run` failed half way.
        let container = Self {
            podman: podman.to_path_buf(),
            name,
        };
        let output = Command::new(podman)
            .args(run_args(&container.name, image, device_auth))
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

    /// `auth.json` from the container's `CODEX_HOME`, read straight into a
    /// zeroizing buffer of at most [`MAX_AUTH_FILE_BYTES`] bytes. `None`
    /// when Codex wrote none.
    pub(crate) fn read_auth_file(&self) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        let mut child = Command::new(&self.podman)
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
        let _ = Command::new(&self.podman)
            .args(remove_args(&self.name))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Run `codex login` in a login container of `image` on the terminal
/// `input`/`output`, read the sign-in Codex stored in its `CODEX_HOME`,
/// store it, and remove the container (on every path).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect(
    podman: PathBuf,
    image: String,
    data_dir: PathBuf,
    device_auth: bool,
    input: OwnedFd,
    output: Box<dyn std::io::Write + Send>,
    interrupt: &Interrupt,
    resized: &Resized,
) -> Result<String, Failure> {
    let container = {
        let podman = podman.clone();
        tokio::task::spawn_blocking(move || Container::start(&podman, &image, device_auth))
            .await
            .map_err(|_| failure(exit::FAILURE, "starting the login container failed"))??
    };
    let flag = interrupt.flag.clone();
    let resized = resized.0.clone();
    let args = login_exec_args(container.name(), device_auth);
    let run = tokio::task::spawn_blocking(move || {
        let mut output = output;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        relay::run_program(
            &relay::Program {
                path: &podman,
                args: &args,
                cwd: None,
                set_env: &[],
                remove_env: &[],
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
        drop(container);
        return Err(failure(
            exit::INTERRUPTED,
            "interrupted: codex login was stopped, nothing was stored and the login container \
             was removed",
        ));
    }
    let ended = Ended::from_exit_code(run.exit_code);
    // Whatever the filters hid on screen is not used: the sign-in is read
    // from the file Codex wrote.
    drop(run);
    let bytes = tokio::task::spawn_blocking(move || {
        let read = container.read_auth_file();
        drop(container);
        read
    })
    .await
    .map_err(|_| failure(exit::FAILURE, "reading Codex's auth.json failed"))?
    .map_err(|error| failure(exit::FAILURE, error))?;
    store_sign_in(&data_dir, bytes, &ended)?;
    Ok(super::codex::connected_message(true))
}
