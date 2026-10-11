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

fn volumes() -> BTreeSet<String> {
    podman_text(&["volume", "ls", "--quiet"])
        .lines()
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
}

impl FakeImage {
    fn build(mode: &str, scratch: &Path) -> Self {
        let context = scratch.join(format!("image-{mode}"));
        std::fs::create_dir(&context).unwrap();
        let (access, refresh) = (jwt(), format!("rt_{}", uuid::Uuid::new_v4().simple()));
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
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
        command.cwd(root);
        command.env("PATH", path_dir);
        command.env("TMPDIR", root.join("tmp"));
        command.env("AXOCOATL_DATA_DIR", root.join("data"));
        command.env_remove("RUST_LOG");
        // Must not reach the container.
        command.env("OPENAI_API_KEY", "sk-proj-set-by-the-test-on-the-host");
        command.env("CODEX_API_KEY", "set-by-the-test-on-the-host");
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
    ] {
        assert!(leaked_positions(shown, token).is_empty(), "{text}");
    }
}

#[test]
#[ignore = "requires Podman (CONTAINER_CONNECTION) and a free 127.0.0.1:1455"]
fn connect_codex_signs_in_in_a_throwaway_container_without_a_host_codex() {
    assert!(
        std::env::var_os("CONTAINER_CONNECTION").is_some(),
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
    let path_dir = scratch_path.join("path");
    std::fs::create_dir(&path_dir).unwrap();
    let real_podman = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("podman"))
        .find(|candidate| candidate.is_file())
        .expect("podman on PATH");
    std::os::unix::fs::symlink(&real_podman, path_dir.join("podman")).unwrap();
    assert_eq!(
        std::fs::read_dir(&path_dir).unwrap().count(),
        1,
        "the PATH directory holds only podman"
    );
    let volumes_before = volumes();
    let ok = FakeImage::build("ok", &scratch_path);
    let none = FakeImage::build("none", &scratch_path);

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
                 root_writable=no OPENAI_API_KEY=unset CODEX_API_KEY=unset"
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
    drop(ok);
    drop(none);
    assert_eq!(volumes(), volumes_before);
}
