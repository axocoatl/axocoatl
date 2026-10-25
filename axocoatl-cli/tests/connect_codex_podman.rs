//! `axocoatl connect codex` as the built binary runs it by default: the
//! sign-in in a throwaway login container through Podman, with no `codex`
//! on this computer. The image is a test image whose `codex` is the fake in
//! `fixtures/fake-codex-container.js` (it writes `auth.json` and prints the
//! sign-in address or a device code), recorded in each run's data root as
//! the `codex` recipe image, the way `axocoatl recipe build codex` records
//! the real one. Nothing here reaches a network or a real account.
//!
//! ```text
//! CONTAINER_CONNECTION=axocoatl-ci-pr74 \
//!   cargo test -p axocoatl-cli --test connect_codex_podman -- --ignored
//! ```
//!
//! It publishes `127.0.0.1:1455` on this computer, so it runs its cases one
//! after another in one test and needs that port free.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axocoatl_daemon::external_agent::codex_chatgpt::ChatgptSignIn;
use axocoatl_daemon::external_agent::recipe_images;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const BIN: &str = env!("CARGO_BIN_EXE_axocoatl");
const FAKE: &str = include_str!("fixtures/fake-codex-container.js");
const ROLE_LABEL: &str = "label=io.axocoatl.role=connect-codex";

fn jwt() -> String {
    let part = || uuid::Uuid::new_v4().simple().to_string();
    format!("eyJhbGciOiJSUzI1NiJ9.eyJ{}{}.{}", part(), part(), part())
}

fn podman(args: &[&str]) -> std::process::Output {
    Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("podman runs")
}

fn podman_text(args: &[&str]) -> String {
    let output = podman(args);
    assert!(output.status.success(), "podman {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The login containers that exist (running or not).
fn login_containers() -> Vec<String> {
    podman_text(&[
        "ps",
        "--all",
        "--filter",
        ROLE_LABEL,
        "--format",
        "{{.Names}}",
    ])
    .lines()
    .map(str::to_string)
    .collect()
}

/// The anonymous volumes (named by 64 hexadecimal digits): the only kind a
/// login container could leave, since it is given no named volume. Named
/// volumes other tests on the same Podman connection make and remove
/// meanwhile are not counted.
fn volumes() -> BTreeSet<String> {
    podman_text(&["volume", "ls", "--quiet"])
        .lines()
        .filter(|name| name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect()
}

/// Every 12-byte piece of `token` that `shown` contains, by position.
fn leaked_positions(shown: &[u8], token: &str) -> Vec<usize> {
    let token = token.as_bytes();
    (0..=token.len() - 12)
        .filter(|start| {
            let window = &token[*start..*start + 12];
            shown.windows(12).any(|candidate| candidate == window)
        })
        .collect()
}

/// Every regular file under `directory` (depth-limited, no link followed,
/// changed since `since` when given) whose bytes contain `needle`.
fn files_containing(
    directory: &Path,
    needle: &[u8],
    depth: usize,
    since: Option<SystemTime>,
    skip: &Path,
    hits: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if path == skip || (kind.is_dir() && path.canonicalize().is_ok_and(|real| real == skip)) {
            continue;
        }
        if kind.is_dir() && depth > 0 {
            files_containing(&path, needle, depth - 1, since, skip, hits);
        } else if kind.is_file() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let recent = since
                .is_none_or(|since| metadata.modified().is_ok_and(|modified| modified >= since));
            if recent && metadata.len() < 4 * 1024 * 1024 {
                if let Ok(bytes) = std::fs::read(&path) {
                    if bytes.windows(needle.len()).any(|window| window == needle) {
                        hits.push(path);
                    }
                }
            }
        }
    }
}

fn port_free() -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 1455)).is_ok()
}

/// Wait until this computer's 1455 is free again (Podman stops publishing
/// it shortly after the container is removed).
fn wait_port_free() {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !port_free() {
        assert!(Instant::now() < deadline, "127.0.0.1:1455 stayed taken");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// This computer's address on its network, when it has one.
fn lan_address() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback() && !address.is_unspecified()).then_some(address)
}

/// One fake image: the fake `codex` with its `mode`, the `auth` it writes
/// and the `screen` lines it prints. Removed (with the login containers
/// made from it) when dropped.
struct FakeImage {
    id: String,
    access: String,
    refresh: String,
    shown_key: String,
    shown_jwt: String,
    /// The `OPENAI_API_KEY` its `auth.json` holds, if any.
    api_key: Option<String>,
}

impl FakeImage {
    fn build(mode: &str, scratch: &Path) -> Self {
        Self::build_with_key(mode, scratch, None)
    }

    /// [`FakeImage::build`] whose `auth.json` holds `api_key` as its
    /// `OPENAI_API_KEY` (`null` without one), as Codex writes it after the
    /// browser sign-in.
    fn build_with_key(mode: &str, scratch: &Path, api_key: Option<String>) -> Self {
        let context = scratch.join(format!("image-{mode}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&context).unwrap();
        let (access, refresh) = (jwt(), format!("rt_{}", uuid::Uuid::new_v4().simple()));
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": api_key,
            "tokens": {"id_token": jwt(), "access_token": access,
                "refresh_token": refresh, "account_id": "acct-podman"},
            "last_refresh": "2026-10-11T12:00:00Z",
        });
        let shown_key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
        let shown_jwt = jwt();
        std::fs::write(context.join("codex"), FAKE).unwrap();
        std::fs::write(context.join("mode"), mode).unwrap();
        std::fs::write(context.join("auth"), auth.to_string()).unwrap();
        std::fs::write(
            context.join("screen"),
            format!("{shown_key}\n{shown_jwt}\n"),
        )
        .unwrap();
        // The recipe images' own base (Node, pinned by digest).
        let from = axocoatl_isolation::recipes::RECIPE_BASE
            .lines()
            .find(|line| line.starts_with("FROM "))
            .unwrap();
        std::fs::write(
            context.join("Containerfile"),
            format!(
                "{from}\nCOPY codex mode auth screen /opt/fake/\nRUN chmod 0755 /opt/fake/codex \
                 && chmod 0644 /opt/fake/mode /opt/fake/auth /opt/fake/screen \
                 && ln -s /opt/fake/codex /usr/local/bin/codex\nLABEL io.axocoatl.test=connect-codex-podman\n"
            ),
        )
        .unwrap();
        let tag = format!(
            "localhost/axocoatl-test-fake-codex:{}",
            uuid::Uuid::new_v4().simple()
        );
        let context_arg = context.display().to_string();
        let built = podman(&[
            "build",
            "--pull=missing",
            "--quiet",
            "-t",
            &tag,
            &context_arg,
        ]);
        assert!(built.status.success(), "{built:?}");
        let id = podman_text(&["image", "inspect", "--format", "{{.Id}}", &tag]);
        // Only the id is used: the tag goes now.
        assert!(podman(&["untag", &id, &tag]).status.success());
        Self {
            id,
            access,
            refresh,
            shown_key,
            shown_jwt,
            api_key,
        }
    }
}

impl Drop for FakeImage {
    fn drop(&mut self) {
        let ancestor = format!("ancestor={}", self.id);
        let output = podman(&["ps", "--all", "--quiet", "--filter", &ancestor]);
        for container in String::from_utf8_lossy(&output.stdout).lines() {
            let _ = podman(&["rm", "--force", "--time=0", container]);
        }
        let _ = podman(&["rmi", "--force", &self.id]);
    }
}

/// One `axocoatl connect codex` run in a pseudo-terminal, with a private
/// data root and temporary directory, and a PATH holding nothing but
/// `podman`.
struct Run {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<Vec<u8>>>,
    reading: Option<std::thread::JoinHandle<()>>,
}

impl Run {
    fn start(root: &Path, path_dir: &Path, image: &FakeImage, device_auth: bool) -> Self {
        Self::start_with(root, path_dir, image, device_auth, &[])
    }

    /// [`Run::start`] with `extra` arguments.
    fn start_with(
        root: &Path,
        path_dir: &Path,
        image: &FakeImage,
        device_auth: bool,
        extra: &[&str],
    ) -> Self {
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        std::fs::set_permissions(root.join("data"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        // The fake image, recorded as `axocoatl recipe build codex` records
        // the real one.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let record = recipe_images::record_for(&["codex".to_string()], &image.id, now).unwrap();
        recipe_images::record_image_at(&root.join("data"), record).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(BIN);
        command.args(["connect", "codex", "-c"]);
        command.arg(root.join("axocoatl.yaml"));
        if device_auth {
            command.arg("--device-auth");
        }
        command.args(extra);
        command.cwd(root);
        command.env("PATH", path_dir);
        command.env("TMPDIR", root.join("tmp"));
        command.env("AXOCOATL_DATA_DIR", root.join("data"));
        command.env_remove("RUST_LOG");
        // Must not reach the container.
        command.env("OPENAI_API_KEY", "sk-proj-set-by-the-test-on-the-host");
        command.env("CODEX_API_KEY", "set-by-the-test-on-the-host");
        // Nor must a proxy variable (Podman copies its own unless told not
        // to). Unreachable, so a proxy that is used fails the run.
        command.env("https_proxy", "http://127.0.0.1:9");
        command.env("HTTPS_PROXY", "http://127.0.0.1:9");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(Vec::new()));
        let reading = {
            let screen = screen.clone();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = reader.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    screen.lock().unwrap().extend_from_slice(&buffer[..read]);
                }
            })
        };
        Self {
            child,
            master: pair.master,
            writer,
            screen,
            reading: Some(reading),
        }
    }

    fn shown(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !self.shown().contains(text) {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "exited before {text:?}:\n{}",
                self.shown()
            );
            assert!(Instant::now() < deadline, "no {text:?}:\n{}", self.shown());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn type_text(&mut self, text: &[u8]) {
        self.writer.write_all(text).unwrap();
        self.writer.flush().unwrap();
    }

    fn signal(&self, signal: i32) {
        let pid = self.child.process_id().unwrap();
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Type `text` every `interval` until it exits (writes after the
    /// terminal closed are ignored).
    fn keep_typing_until_exit(&mut self, text: &[u8], interval: Duration) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "no exit:\n{}", self.shown());
            let _ = self.writer.write_all(text);
            let _ = self.writer.flush();
            std::thread::sleep(interval);
        }
    }

    /// Its exit code, and everything it showed.
    fn finish(mut self) -> (u32, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "no exit:\n{}", self.shown());
            std::thread::sleep(Duration::from_millis(50));
        };
        drop(self.writer);
        drop(self.master);
        if let Some(reading) = self.reading.take() {
            let _ = reading.join();
        }
        let shown = self.screen.lock().unwrap().clone();
        (status.exit_code(), shown)
    }

    fn secret(root: &Path) -> PathBuf {
        root.join("data/secrets/codex-chatgpt")
    }
}

/// The one login container now running.
fn the_login_container() -> String {
    let names = login_containers();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

fn inspect(container: &str, format: &str) -> String {
    podman_text(&["inspect", "--format", format, container])
}

/// The sign-in's redirect, as the browser makes it on this computer.
fn callback() -> String {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, 1455)),
        Duration::from_secs(10),
    )
    .expect("127.0.0.1:1455 is published");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(
            b"GET /auth/callback?code=fake-code&state=Qsk-pLkJhGfDsAeyJ0x HTTP/1.1\r\n\
              Host: localhost:1455\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

/// The container is gone (and so is its tmpfs `CODEX_HOME`), no volume
/// was left, and the port is free again.
fn assert_cleaned_up(volumes_before: &BTreeSet<String>) {
    assert!(login_containers().is_empty(), "{:?}", login_containers());
    assert_eq!(&volumes(), volumes_before);
    wait_port_free();
}

/// The tokens are in the stored secret and in no other file under `root`
/// (data root, working directory, TMPDIR), nor in a file changed since
/// `since` in this test's temporary directory outside `scratch` (which
/// holds the fake image's build context).
fn assert_tokens_only_in_the_secret(
    root: &Path,
    scratch: &Path,
    image: &FakeImage,
    since: SystemTime,
) {
    let secret = Run::secret(root);
    for token in [&image.access, &image.refresh] {
        let mut hits = Vec::new();
        files_containing(
            root,
            token.as_bytes(),
            8,
            None,
            Path::new("/nonexistent"),
            &mut hits,
        );
        hits.retain(|hit| hit != &secret);
        assert!(hits.is_empty(), "{hits:?}");
        let mut elsewhere = Vec::new();
        files_containing(
            &std::env::temp_dir(),
            token.as_bytes(),
            4,
            Some(since),
            scratch,
            &mut elsewhere,
        );
        assert!(elsewhere.is_empty(), "{elsewhere:?}");
    }
}

fn assert_nothing_shown(shown: &[u8], image: &FakeImage) {
    let text = String::from_utf8_lossy(shown);
    for token in [
        &image.access,
        &image.refresh,
        &image.shown_key,
        &image.shown_jwt,
    ]
    .into_iter()
    .chain(&image.api_key)
    {
        assert!(leaked_positions(shown, token).is_empty(), "{text}");
    }
}

#[test]
#[ignore = "requires Podman (on a Mac, CONTAINER_CONNECTION) and a free 127.0.0.1:1455"]
fn connect_codex_signs_in_in_a_throwaway_container_without_a_host_codex() {
    // On a Mac, name the machine to test with; on Linux the default
    // connection is this computer's own Podman.
    assert!(
        cfg!(not(target_os = "macos")) || std::env::var_os("CONTAINER_CONNECTION").is_some(),
        "set CONTAINER_CONNECTION to the Podman connection to test with"
    );
    assert!(port_free(), "127.0.0.1:1455 must be free for this test");
    assert!(
        login_containers().is_empty(),
        "login containers exist before the test: {:?}",
        login_containers()
    );
    let since = SystemTime::now();
    let scratch = tempfile::tempdir().unwrap();
    let scratch_path = scratch.path().canonicalize().unwrap();
    std::fs::set_permissions(&scratch_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A PATH with `podman` and nothing else: no `codex` on this computer.
    // On Linux, Podman's own engine also looks up its rootless network
    // helpers on PATH to publish a port.
    let path_dir = scratch_path.join("path");
    std::fs::create_dir(&path_dir).unwrap();
    let on_path = |name: &str| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    };
    let mut tools = vec!["podman"];
    if cfg!(target_os = "linux") {
        tools.extend(["slirp4netns", "pasta"]);
    }
    for tool in &tools {
        match on_path(tool) {
            Some(real) => std::os::unix::fs::symlink(real, path_dir.join(tool)).unwrap(),
            None => assert_ne!(*tool, "podman", "podman on PATH"),
        }
    }
    let listed: Vec<String> = std::fs::read_dir(&path_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        listed.iter().all(|name| tools.contains(&name.as_str())),
        "the PATH directory holds only podman and its network helpers: {listed:?}"
    );
    let volumes_before = volumes();
    let ok = FakeImage::build("ok", &scratch_path);
    let none = FakeImage::build("none", &scratch_path);
    let key = format!(
        "sk-proj-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let with_key = FakeImage::build_with_key("ok", &scratch_path, Some(key));

    // 1. The browser sign-in: one port, published on this computer's
    //    loopback only, reaches Codex's login server in the container;
    //    nothing from this computer is mounted; the tokens are stored
    //    exactly and never shown.
    {
        let root = scratch_path.join("browser");
        let mut run = Run::start(&root, &path_dir, &ok, false);
        run.wait_for("navigate to this URL to authenticate");
        run.wait_for("originator=codex_cli_rs");
        let container = the_login_container();
        assert_eq!(
            inspect(&container, "{{json .HostConfig.PortBindings}}"),
            r#"{"14550/tcp":[{"HostIp":"127.0.0.1","HostPort":"1455"}]}"#
        );
        assert_eq!(inspect(&container, "{{json .Mounts}}"), "[]");
        assert_eq!(inspect(&container, "{{json .HostConfig.Binds}}"), "[]");
        assert_eq!(
            inspect(&container, "{{.HostConfig.ReadonlyRootfs}}"),
            "true"
        );
        assert_eq!(inspect(&container, "{{.Config.User}}"), "1000:1000");
        assert_eq!(inspect(&container, "{{.Image}}"), ok.id);
        let tmpfs = inspect(&container, "{{json .HostConfig.Tmpfs}}");
        assert!(
            tmpfs.contains("/codex-home") && tmpfs.contains("/tmp"),
            "{tmpfs}"
        );
        if let Some(address) = lan_address() {
            assert!(
                TcpStream::connect_timeout(&SocketAddr::new(address, 1455), Duration::from_secs(3))
                    .is_err(),
                "1455 is reachable at {address}"
            );
        }
        let response = callback();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("Signed in to fake Codex"), "{response}");
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("Connected Codex: stored your ChatGPT sign-in as secret codex-chatgpt"),
            "{text}"
        );
        assert!(text.contains("the login container was removed"), "{text}");
        let flat: String = text.split(['\r', '\n']).collect();
        assert!(
            flat.contains(
                "fake-seen device=no codex_home=/codex-home home_mode=700 uid=1000 \
                 root_writable=no OPENAI_API_KEY=unset CODEX_API_KEY=unset proxy=unset"
            ),
            "{text}"
        );
        assert!(
            flat.contains("fake-callback host=localhost:1455 code=fake-code"),
            "{text}"
        );
        assert!(
            flat.contains("state=Qsk-pLkJhGfDsAeyJ0x&originator=codex_cli_rs"),
            "{text}"
        );
        assert!(text.contains("Successfully logged in"), "{text}");
        assert!(
            text.contains("You have 30 minutes to finish signing in."),
            "{text}"
        );
        // Codex in the container cannot open a browser here: the intro
        // says to open the address.
        assert!(
            text.contains("Open the address it prints in a browser on this computer."),
            "{text}"
        );
        assert!(!text.contains("browser it opens"), "{text}");
        assert_nothing_shown(&shown, &ok);
        let stored = std::fs::read(Run::secret(&root)).unwrap();
        let sign_in = ChatgptSignIn::decode(&stored).unwrap();
        assert_eq!(sign_in.access_token(), ok.access);
        assert_eq!(sign_in.refresh_token(), ok.refresh);
        assert_eq!(sign_in.account_id(), Some("acct-podman"));
        assert_eq!(
            std::fs::metadata(Run::secret(&root))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_tokens_only_in_the_secret(&root, &scratch_path, &ok, since);
        assert_cleaned_up(&volumes_before);
    }

    // 2. The device sign-in: no port is published.
    {
        let root = scratch_path.join("device");
        let mut run = Run::start(&root, &path_dir, &ok, true);
        run.wait_for("WXYZ-1234");
        let container = the_login_container();
        assert_eq!(inspect(&container, "{{.Config.Timeout}}"), "1800");
        let bindings = inspect(&container, "{{json .HostConfig.PortBindings}}");
        assert!(bindings == "{}" || bindings == "null", "{bindings}");
        assert!(port_free(), "the device sign-in published 1455");
        run.type_text(b"\r");
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("fake-seen device=yes"), "{text}");
        assert_nothing_shown(&shown, &ok);
        let sign_in = ChatgptSignIn::decode(&std::fs::read(Run::secret(&root)).unwrap()).unwrap();
        assert_eq!(sign_in.access_token(), ok.access);
        assert_eq!(sign_in.refresh_token(), ok.refresh);
        assert_tokens_only_in_the_secret(&root, &scratch_path, &ok, since);
        assert_cleaned_up(&volumes_before);
    }

    // 3. Codex ends without a sign-in: nothing is stored, the container
    //    goes.
    {
        let root = scratch_path.join("none");
        let mut run = Run::start(&root, &path_dir, &none, false);
        run.wait_for("originator=codex_cli_rs");
        let response = callback();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 2, "{text}");
        assert!(text.contains("without storing a sign-in"), "{text}");
        assert!(!Run::secret(&root).exists());
        assert_nothing_shown(&shown, &none);
        assert_cleaned_up(&volumes_before);
    }

    // 4. Ctrl-C typed while Codex waits reaches Codex, which stops: nothing
    //    is stored, the container goes.
    {
        let root = scratch_path.join("ctrl-c");
        let mut run = Run::start(&root, &path_dir, &ok, false);
        run.wait_for("originator=codex_cli_rs");
        assert_eq!(login_containers().len(), 1);
        run.type_text(b"\x03");
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 2, "{text}");
        assert!(!Run::secret(&root).exists());
        assert_cleaned_up(&volumes_before);
    }

    // 5. A signal to Axocoatl while Codex waits: interrupted, nothing
    //    stored, the container goes.
    {
        let root = scratch_path.join("signal");
        let mut run = Run::start(&root, &path_dir, &ok, false);
        run.wait_for("originator=codex_cli_rs");
        assert_eq!(login_containers().len(), 1);
        run.signal(15);
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 6, "{text}");
        assert!(text.contains("the login container was removed"), "{text}");
        assert!(!Run::secret(&root).exists());
        assert_cleaned_up(&volumes_before);
    }

    // 6. 1455 taken on this computer: refused before any container starts,
    //    with the device sign-in as the way out.
    {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 1455)).unwrap();
        let root = scratch_path.join("taken");
        let run = Run::start(&root, &path_dir, &ok, false);
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 3, "{text}");
        assert!(text.contains("--device-auth"), "{text}");
        assert!(login_containers().is_empty());
        drop(taken);
    }

    // 7. Ctrl-C typed again and again once Codex has signed in and exited:
    //    the terminal is no longer raw, so each one is a SIGINT to
    //    Axocoatl's process group, which Axocoatl catches. The podman
    //    commands that read the sign-in and remove the container are in
    //    their own group: the container (and the tokens in it) still goes.
    //    Then the same ten times with Ctrl-C typed every millisecond, so
    //    that some arrive while one of those commands is being started,
    //    still in Axocoatl's group: they do not kill it either.
    for (round, interval) in std::iter::once(Duration::from_millis(15))
        .chain(std::iter::repeat_n(Duration::from_millis(1), 10))
        .enumerate()
    {
        let root = scratch_path.join(format!("ctrl-c-after-sign-in-{round}"));
        let mut run = Run::start(&root, &path_dir, &ok, true);
        run.wait_for("WXYZ-1234");
        run.type_text(b"\r");
        run.wait_for("Successfully logged in");
        run.keep_typing_until_exit(b"\x03", interval);
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert!(!text.contains("could not be read"), "round {round}: {text}");
        match code {
            // Read and stored before Axocoatl saw the signal.
            0 => {
                let sign_in =
                    ChatgptSignIn::decode(&std::fs::read(Run::secret(&root)).unwrap()).unwrap();
                assert_eq!(sign_in.access_token(), ok.access);
            }
            // Seen while the relay was still draining: nothing stored. (A
            // Ctrl-C discards what the terminal has not shown yet, so with
            // one every millisecond the message itself may be lost.)
            6 => {
                assert!(
                    round > 0 || text.contains("the login container was removed"),
                    "{text}"
                );
                assert!(!Run::secret(&root).exists());
            }
            _ => panic!("round {round}: exit {code}: {text}"),
        }
        assert_nothing_shown(&shown, &ok);
        assert_cleaned_up(&volumes_before);
    }

    // 8. Ctrl-C pressed again and again to abort the browser sign-in: the
    //    first stops Codex, the others reach Axocoatl. Nothing is stored,
    //    the container goes and 1455 is free for the next sign-in.
    {
        let root = scratch_path.join("ctrl-c-abort");
        let mut run = Run::start(&root, &path_dir, &ok, false);
        run.wait_for("originator=codex_cli_rs");
        assert_eq!(login_containers().len(), 1);
        run.keep_typing_until_exit(b"\x03", Duration::from_millis(15));
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert!(code == 2 || code == 6, "exit {code}: {text}");
        assert!(!Run::secret(&root).exists());
        assert_cleaned_up(&volumes_before);
        let root = scratch_path.join("after-abort");
        let mut run = Run::start(&root, &path_dir, &ok, false);
        run.wait_for("originator=codex_cli_rs");
        let response = callback();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 0, "{text}");
        assert_cleaned_up(&volumes_before);
    }
    // 9. `--sign-in-timeout 1`: the intro says one minute, Podman stops
    //    the container after 60 seconds, and then the sign-in was not
    //    completed in time (exit 2): nothing stored, the container gone.
    //    The error `podman exec` may print when the container goes under
    //    it (`Error: retrieving exec session … no such container`, when
    //    Podman has removed the container before it asks for the session's
    //    exit code) is not shown; Codex's output is, and one blank line
    //    separates its last line from the message.
    {
        let root = scratch_path.join("sign-in-timeout");
        let started = Instant::now();
        let mut run = Run::start_with(&root, &path_dir, &ok, true, &["--sign-in-timeout", "1"]);
        run.wait_for("You have 1 minute to finish signing in.");
        run.wait_for("WXYZ-1234");
        let container = the_login_container();
        assert_eq!(inspect(&container, "{{.Config.Timeout}}"), "60");
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 2, "{text}");
        assert!(started.elapsed() >= Duration::from_secs(60), "{text}");
        assert!(
            text.contains(
                "The Codex sign-in was not completed within 1 minute; nothing was stored. Run \
                 `axocoatl connect codex` again."
            ),
            "{text}"
        );
        assert!(text.contains("WXYZ-1234"), "{text}");
        // Codex's last line, its own blank line after it, and the message:
        // exactly one blank line, not another for the error not shown.
        assert!(
            text.contains(
                "   \x1b[1mWXYZ-1234\x1b[0m\r\n\r\n✗ The Codex sign-in was not completed \
                 within 1 minute"
            ),
            "{text:?}"
        );
        assert!(!text.contains("Error: "), "{text}");
        assert!(!text.contains("exec session"), "{text}");
        assert!(!text.contains("no such container"), "{text}");
        assert!(!Run::secret(&root).exists());
        assert_nothing_shown(&shown, &ok);
        assert_cleaned_up(&volumes_before);
    }

    // 10. `--api-key` and `--api-key-only`, with and without
    //     `--device-auth`: this version refuses them before any container
    //     starts, though the image would issue a key, and stores nothing.
    for (name, args) in [
        ("api-key", &["--api-key"][..]),
        ("api-key-only", &["--api-key-only"]),
        ("api-key-device", &["--api-key", "--device-auth"]),
    ] {
        let root = scratch_path.join(name);
        let device_auth = args.contains(&"--device-auth");
        let flags: Vec<&str> = args
            .iter()
            .copied()
            .filter(|arg| *arg != "--device-auth")
            .collect();
        let run = Run::start_with(&root, &path_dir, &with_key, device_auth, &flags);
        let (code, shown) = run.finish();
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 3, "{text}");
        let flat: String = text.split(['\r', '\n']).collect();
        assert!(
            flat.contains(&format!(
                "axocoatl connect codex {} is not available in Axocoatl",
                flags[0]
            )),
            "{text}"
        );
        assert!(
            flat.contains("Sign in with your ChatGPT plan instead"),
            "{text}"
        );
        assert!(login_containers().is_empty());
        assert!(!root.join("data/secrets/codex-openai").exists());
        assert!(!Run::secret(&root).exists());
        assert_nothing_shown(&shown, &with_key);
    }
    drop(ok);
    drop(none);
    drop(with_key);
    assert_eq!(volumes(), volumes_before);
}
