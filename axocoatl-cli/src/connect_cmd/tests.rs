//! `axocoatl connect claude-code` end to end with a fake `claude` (the
//! script `tests/fixtures/fake-claude.sh`, which can replay what Claude
//! Code's renderer writes, from [`super::ink_model`]) on a pseudo-terminal
//! standing in for the user's terminal, and a local fake HTTPS endpoint
//! standing in for Anthropic (its base URL and root certificate are injected
//! here only).

use std::io::Write;
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rustix::termios::{self, LocalModes};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::ink_model::{self, Shape};
use super::redact::tests::leaked_positions;
use super::redact::MASK;
use super::*;

const FAKE_CLAUDE: &str = include_str!("../../tests/fixtures/fake-claude.sh");

/// A token of the real shape, unique to the test so a scan of shared
/// directories cannot match another test's.
fn fresh_token() -> String {
    let part = || uuid::Uuid::new_v4().simple().to_string();
    format!("sk-ant-oat01-{}{}_{}AA", part(), part(), &part()[..26])
}

/// A pseudo-terminal pair: the slave is "the user's terminal" handed to
/// `connect`, the master is where the test types.
struct UserTerminal {
    master: OwnedFd,
    slave: OwnedFd,
}

impl UserTerminal {
    fn set_size(&self, size: (u16, u16)) {
        set_size(&self.slave, size);
    }

    /// What is waiting to be read on the user's side (typed and not read by
    /// anyone), within `timeout`.
    fn pending_input(&self, timeout: Duration) -> Vec<u8> {
        use rustix::event::{poll, PollFd, PollFlags, Timespec};
        let mut input = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut fds = [PollFd::new(&self.slave, PollFlags::IN)];
            let wait = Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            if poll(&mut fds, Some(&wait)).unwrap_or(0) == 0 {
                continue;
            }
            let mut buffer = [0u8; 256];
            match rustix::io::read(&self.slave, &mut buffer) {
                Ok(read) if read > 0 => input.extend_from_slice(&buffer[..read]),
                _ => break,
            }
        }
        input
    }
}

fn user_terminal() -> UserTerminal {
    use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let name = ptsname(&master, Vec::new()).unwrap();
    let slave = rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    UserTerminal { master, slave }
}

/// The modes `RawMode` changes, to compare before and after. `PENDIN` is
/// left out: BSD kernels set it themselves whenever a terminal goes back to
/// canonical mode.
fn modes(fd: impl AsFd) -> (u64, u64, u64, u64) {
    let mut settings = termios::tcgetattr(fd).unwrap();
    settings.local_modes.remove(LocalModes::PENDIN);
    (
        settings.input_modes.bits() as u64,
        settings.output_modes.bits() as u64,
        settings.control_modes.bits() as u64,
        settings.local_modes.bits() as u64,
    )
}

fn is_raw(fd: impl AsFd) -> bool {
    let settings = termios::tcgetattr(fd).unwrap();
    !settings
        .local_modes
        .intersects(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG)
}

/// What `connect` shows the user.
#[derive(Clone, Default)]
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Screen {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
    fn contains(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.bytes()).contains(text)
    }
}

/// A private directory for one test: `bin/claude` (the fake, with its
/// `mode` and `token` files), `data/` (the data root) and `home/`.
struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    token: String,
}

impl Fixture {
    fn new(mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("claude"), FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(bin.join("mode"), mode).unwrap();
        let token = fresh_token();
        std::fs::write(bin.join("token"), &token).unwrap();
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        Self {
            directory,
            root,
            token,
        }
    }

    fn claude(&self) -> PathBuf {
        self.root.join("bin/claude")
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn request(&self, verifier: Option<Verifier>) -> Connect {
        Connect {
            claude: self.claude(),
            secret: CLAUDE_CODE_SECRET.into(),
            data_dir: self.data(),
            verifier,
        }
    }

    /// For mode `replay`: what Claude Code writes in a terminal of `size`.
    fn replay(&self, (cols, rows): (u16, u16), shape: Shape) {
        let (before, after) =
            ink_model::session(&self.token, usize::from(cols), usize::from(rows), shape);
        self.write_replay((cols, rows), &before, &after);
    }

    fn write_replay(&self, (cols, rows): (u16, u16), before: &[u8], after: &[u8]) {
        let bin = self.root.join("bin");
        std::fs::write(bin.join(format!("before-{cols}x{rows}")), before).unwrap();
        std::fs::write(bin.join(format!("after-{cols}x{rows}")), after).unwrap();
    }

    fn stored(&self) -> Option<Vec<u8>> {
        std::fs::read(secret_store::secret_path(&self.data(), CLAUDE_CODE_SECRET)).ok()
    }
}

/// Files under `directory` (to `depth` levels, skipping links and `skip`,
/// and, with `since`, files older than it) whose bytes contain `needle`.
fn files_containing(
    directory: &Path,
    needle: &[u8],
    depth: usize,
    since: Option<SystemTime>,
    skip: &[PathBuf],
    hits: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if skip.iter().any(|skipped| path.starts_with(skipped)) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if depth > 0 {
                files_containing(&path, needle, depth - 1, since, skip, hits);
            }
        } else if metadata.is_file() && metadata.len() <= 4 << 20 {
            let recent = since.is_none_or(|since| {
                metadata
                    .modified()
                    .map(|modified| modified >= since)
                    .unwrap_or(true)
            });
            if recent {
                if let Ok(bytes) = std::fs::read(&path) {
                    if bytes.windows(needle.len()).any(|window| window == needle) {
                        hits.push(path);
                    }
                }
            }
        }
    }
}

/// The token is in the stored secret and the fake's own `token` file, and
/// in no other file under the fixture or the temporary directory.
fn assert_written_nowhere_else(fixture: &Fixture, since: SystemTime) {
    let needle = fixture.token.as_bytes();
    let mut hits = Vec::new();
    files_containing(&fixture.root, needle, 8, None, &[], &mut hits);
    hits.sort();
    // The replay files the tests wrote for the fake hold it when it is on
    // one row.
    hits.retain(|path| {
        !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("after-"))
    });
    let mut expected = vec![fixture.root.join("bin/token")];
    if fixture.stored().is_some() {
        expected.push(secret_store::secret_path(
            &fixture.data(),
            CLAUDE_CODE_SECRET,
        ));
    }
    expected.sort();
    assert_eq!(hits, expected);
    let mut elsewhere = Vec::new();
    files_containing(
        &std::env::temp_dir(),
        needle,
        4,
        Some(since),
        &[fixture.root.clone(), fixture.directory.path().to_path_buf()],
        &mut elsewhere,
    );
    assert!(elsewhere.is_empty(), "{elsewhere:?}");
}

/// The text the fake shows when it waits for Enter.
fn prompt(fixture: &Fixture) -> &'static str {
    match std::fs::read_to_string(fixture.root.join("bin/mode")).as_deref() {
        Ok("replay") => ink_model::PROMPT,
        _ => "Press Enter to continue",
    }
}

/// Types Enter on the user's terminal once the fake asks for it (shows
/// `prompt`), after checking that the terminal is raw then; or raises
/// `interrupt` instead.
fn typist(
    terminal: &UserTerminal,
    screen: &Screen,
    prompt: &'static str,
    interrupt: Option<Interrupt>,
) -> std::thread::JoinHandle<bool> {
    let master = terminal.master.try_clone().unwrap();
    let slave = terminal.slave.try_clone().unwrap();
    let screen = screen.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !screen.contains(prompt) {
            assert!(Instant::now() < deadline, "the fake never asked for a key");
            std::thread::sleep(Duration::from_millis(20));
        }
        let raw = is_raw(&slave);
        match interrupt {
            Some(interrupt) => interrupt.raise(),
            None => {
                rustix::io::write(&master, b"\r").unwrap();
            }
        }
        raw
    })
}

/// `connect`'s result; if it has not finished within 60 seconds, `claude`
/// is stopped (so no test is left waiting on it) and the test fails.
async fn finish_within<F>(connecting: F, interrupt: &Interrupt) -> Result<String, Failure>
where
    F: std::future::Future<Output = Result<String, Failure>>,
{
    tokio::pin!(connecting);
    tokio::select! {
        result = &mut connecting => result,
        () = tokio::time::sleep(Duration::from_secs(60)) => {
            interrupt.raise();
            let _ = connecting.await;
            panic!("connect did not finish within 60 seconds");
        }
    }
}

/// The user's terminal size most tests use.
const SIZE: (u16, u16) = (100, 30);

/// Run `connect` on a fresh user terminal of `size`; returns its result,
/// what it showed, and whether the terminal was raw while the fake waited.
async fn run_connect(
    fixture: &Fixture,
    verifier: Option<Verifier>,
    size: (u16, u16),
    interrupt_at_prompt: bool,
) -> (Result<String, Failure>, Screen, bool) {
    let terminal = user_terminal();
    terminal.set_size(size);
    let before = modes(&terminal.slave);
    assert!(!is_raw(&terminal.slave));
    let screen = Screen::default();
    let interrupt = Interrupt::default();
    let typing = typist(
        &terminal,
        &screen,
        prompt(fixture),
        interrupt_at_prompt.then(|| interrupt.clone()),
    );
    let result = finish_within(
        connect(
            fixture.request(verifier),
            terminal.slave.try_clone().unwrap(),
            Box::new(screen.clone()),
            &interrupt,
            &Resized::default(),
        ),
        &interrupt,
    )
    .await;
    let raw_while_waiting = typing.join().unwrap();
    // Raw mode is restored exactly.
    assert_eq!(modes(&terminal.slave), before);
    (result, screen, raw_while_waiting)
}

fn assert_not_shown(screen: &Screen, token: &str) {
    let shown = screen.bytes();
    let leaks = leaked_positions(&shown, token);
    assert!(leaks.is_empty(), "token bytes at {leaks:?} were shown");
}

/// One request the fake Anthropic answered (whether its bearer token was
/// the expected one, never the token itself).
#[derive(Debug, Clone)]
struct Seen {
    request_line: String,
    bearer_matches: bool,
    version: Option<String>,
    beta: Option<String>,
}

struct FakeAnthropic {
    address: SocketAddr,
    root: reqwest::Certificate,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeAnthropic {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeAnthropic {
    /// An HTTPS server on loopback with a certificate for `localhost` from
    /// a fresh authority. It answers `status`, except that `200` becomes
    /// `401` unless the request carries `Bearer <token>` and the OAuth
    /// headers. Its body holds a marker that must never be shown.
    async fn start(token: &str, status: u16) -> Self {
        Self::start_slow(token, status, Duration::ZERO).await
    }

    /// [`Self::start`], answering each request after `delay`.
    async fn start_slow(token: &str, status: u16, delay: Duration) -> Self {
        let authority = axocoatl_daemon::egress_broker::SessionCa::new("connect-test").unwrap();
        let (certificate, key) = authority.leaf("localhost").unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let expected = format!("Bearer {token}");
        let task = {
            let seen = seen.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let Ok(stream) = acceptor.accept(stream).await else {
                        continue;
                    };
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut request_line = String::new();
                    if stream.read_line(&mut request_line).await.is_err() {
                        continue;
                    }
                    let mut seen_request = Seen {
                        request_line: request_line.trim_end().to_string(),
                        bearer_matches: false,
                        version: None,
                        beta: None,
                    };
                    loop {
                        let mut line = String::new();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let line = line.trim_end();
                        if line.is_empty() {
                            break;
                        }
                        let Some((name, value)) = line.split_once(':') else {
                            continue;
                        };
                        let value = value.trim().to_string();
                        match name.to_ascii_lowercase().as_str() {
                            "authorization" => seen_request.bearer_matches = value == expected,
                            "anthropic-version" => seen_request.version = Some(value),
                            "anthropic-beta" => seen_request.beta = Some(value),
                            _ => {}
                        }
                    }
                    let answered = if status == 200
                        && !(seen_request.bearer_matches
                            && seen_request.beta.as_deref() == Some("oauth-2025-04-20")
                            && seen_request.version.as_deref() == Some("2023-06-01"))
                    {
                        401
                    } else {
                        status
                    };
                    seen.lock().unwrap().push(seen_request);
                    tokio::time::sleep(delay).await;
                    let body =
                        r#"{"data":[{"id":"fake-model"}],"marker":"BODY-MARKER-NEVER-SHOWN"}"#;
                    let response = format!(
                        "HTTP/1.1 {answered} Fake\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.get_mut().write_all(response.as_bytes()).await;
                    let _ = stream.get_mut().shutdown().await;
                }
            })
        };
        Self {
            address,
            root: reqwest::Certificate::from_der(authority.der().as_ref()).unwrap(),
            seen,
            task,
        }
    }

    fn verifier(&self) -> Verifier {
        Verifier::for_test(
            &format!("https://localhost:{}", self.address.port()),
            self.root.clone(),
            Some(("localhost", self.address)),
        )
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_colored_token_is_captured_checked_and_stored_and_never_shown() {
    let started = SystemTime::now();
    let fixture = Fixture::new("colored");
    let anthropic = FakeAnthropic::start(&fixture.token, 200).await;
    let (result, screen, raw) =
        run_connect(&fixture, Some(anthropic.verifier()), SIZE, false).await;
    let message = result.unwrap();
    assert!(raw, "the user's terminal was raw while claude ran");
    assert!(message.starts_with(
        "Connected Claude Code: stored secret claude-code-oauth (the token was never shown)"
    ));
    assert!(
        message.contains("axocoatl recipe build claude-code"),
        "{message}"
    );
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    let path = secret_store::secret_path(&fixture.data(), CLAUDE_CODE_SECRET);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_not_shown(&screen, &fixture.token);
    let text = String::from_utf8_lossy(&screen.bytes()).into_owned();
    assert_eq!(text.matches(MASK).count(), 1, "{text}");
    assert!(text.contains("Your OAuth token (valid for 1 year):"));
    assert!(text.contains("Checking the token with localhost"));
    assert!(!text.contains("BODY-MARKER"));
    assert!(!message.contains("BODY-MARKER"));
    let seen = anthropic.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].request_line, "GET /v1/models?limit=1 HTTP/1.1");
    assert!(seen[0].bearer_matches);
    assert_eq!(seen[0].version.as_deref(), Some("2023-06-01"));
    assert_eq!(seen[0].beta.as_deref(), Some("oauth-2025-04-20"));
    assert_written_nowhere_else(&fixture, started);
}

/// The token arrives in three writes, two inside its prefix, with pauses
/// that make the relay show held-back output; `--no-verify` stores it
/// without a request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_split_across_writes_with_pauses_is_captured_exactly() {
    let started = SystemTime::now();
    let fixture = Fixture::new("split");
    let (result, screen, raw) = run_connect(&fixture, None, SIZE, false).await;
    result.unwrap();
    assert!(raw);
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    assert_not_shown(&screen, &fixture.token);
    let text = String::from_utf8_lossy(&screen.bytes()).into_owned();
    assert!(!text.contains("sk-ant-"), "{text}");
    assert!(!text.contains("Checking the token"));
    assert_written_nowhere_else(&fixture, started);
}

/// `claude` runs in a terminal as wide as the user's: in a wide one the
/// fake prints the token on one line, in a narrow one it breaks it at the
/// last column, and the token is still hidden and captured whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_wrapped_by_a_narrow_terminal_is_hidden_and_captured() {
    for cols in [250, 40, 57] {
        let fixture = Fixture::new("wrapped");
        let (result, screen, _) = run_connect(&fixture, None, (cols, 30), false).await;
        result.unwrap_or_else(|failure| panic!("{cols} columns: {}", failure.message));
        assert_eq!(
            fixture.stored().unwrap(),
            fixture.token.as_bytes(),
            "{cols} columns"
        );
        assert_not_shown(&screen, &fixture.token);
        assert!(screen.contains("Store this token securely."));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_token_printed_twice_is_stored_once() {
    let fixture = Fixture::new("twice");
    let (result, screen, _) = run_connect(&fixture, None, SIZE, false).await;
    result.unwrap();
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    assert_not_shown(&screen, &fixture.token);
    assert_eq!(
        String::from_utf8_lossy(&screen.bytes())
            .matches(MASK)
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_token_or_two_tokens_store_nothing() {
    let started = SystemTime::now();
    let fixture = Fixture::new("none");
    let (result, screen, _) = run_connect(&fixture, None, SIZE, false).await;
    let failure = result.unwrap_err();
    assert_eq!(failure.code, exit::NO_TOKEN);
    assert!(
        failure
            .message
            .starts_with("claude setup-token exited with status 1 without printing"),
        "{}",
        failure.message
    );
    assert!(screen.contains("Error: the sign-in was cancelled"));
    assert!(fixture.stored().is_none());
    assert_written_nowhere_else(&fixture, started);

    let fixture = Fixture::new("two");
    let (result, screen, _) = run_connect(&fixture, None, SIZE, false).await;
    let failure = result.unwrap_err();
    assert_eq!(failure.code, exit::NO_TOKEN);
    assert!(
        failure.message.contains("printed 2 different OAuth tokens"),
        "{}",
        failure.message
    );
    assert!(fixture.stored().is_none());
    assert_not_shown(&screen, &fixture.token);
    assert!(leaked_positions(failure.message.as_bytes(), &fixture.token).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_token_is_not_stored() {
    for status in [401, 403] {
        let fixture = Fixture::new("colored");
        let anthropic = FakeAnthropic::start(&fixture.token, status).await;
        let (result, screen, _) =
            run_connect(&fixture, Some(anthropic.verifier()), SIZE, false).await;
        let failure = result.unwrap_err();
        assert_eq!(failure.code, exit::REJECTED);
        assert!(
            failure.message.starts_with(&format!(
                "Anthropic rejected the token (HTTP {status}); nothing was stored."
            )),
            "{}",
            failure.message
        );
        assert!(fixture.stored().is_none());
        assert_eq!(anthropic.seen().len(), 1);
        assert_not_shown(&screen, &fixture.token);
        assert!(!screen.contains("BODY-MARKER"));
    }
}

/// No answer: nothing is stored, and the message says how to retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_api_stores_nothing_and_says_how_to_retry() {
    let fixture = Fixture::new("colored");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let authority = axocoatl_daemon::egress_broker::SessionCa::new("connect-test").unwrap();
    let verifier = Verifier::for_test(
        &format!("https://localhost:{}", address.port()),
        reqwest::Certificate::from_der(authority.der().as_ref()).unwrap(),
        Some(("localhost", address)),
    );
    let (result, screen, _) = run_connect(&fixture, Some(verifier), SIZE, false).await;
    let failure = result.unwrap_err();
    assert_eq!(failure.code, exit::UNVERIFIED);
    assert!(
        failure
            .message
            .starts_with("could not check the token with Anthropic: the request to localhost"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("--no-verify"));
    assert!(fixture.stored().is_none());
    assert_not_shown(&screen, &fixture.token);
    assert!(leaked_positions(failure.message.as_bytes(), &fixture.token).is_empty());
}

/// A signal while `claude setup-token` waits: it is stopped, the terminal
/// is restored, and nothing is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_stops_claude_and_stores_nothing() {
    let fixture = Fixture::new("colored");
    let (result, _, raw) = run_connect(&fixture, None, SIZE, true).await;
    assert!(raw);
    let failure = result.unwrap_err();
    assert_eq!(failure.code, exit::INTERRUPTED);
    assert!(fixture.stored().is_none());
}

/// Wait until `screen` shows `text`.
fn wait_for(screen: &Screen, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !screen.contains(text) {
        assert!(Instant::now() < deadline, "never shown: {text}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn set_size(fd: impl AsFd, (cols, rows): (u16, u16)) {
    termios::tcsetwinsize(
        fd,
        termios::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
}

/// What Claude Code 2.1.271 writes for `claude setup-token`, replayed by the
/// fake in a terminal as large as the user's, from narrow to wide (and with
/// the tall banner from 30 rows): the token, which Ink wraps into rows
/// placed by cursor movement, is stored exactly and never shown, and the
/// screen around it is Claude Code's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_codes_rendering_is_stored_and_never_shown_at_any_width() {
    for size in [
        (40, 24),
        (57, 24),
        (80, 24),
        (107, 30),
        (108, 30),
        (109, 30),
        (120, 40),
        (200, 50),
        (300, 60),
    ] {
        let started = SystemTime::now();
        let fixture = Fixture::new("replay");
        fixture.replay(size, Shape::Claude);
        let (result, screen, raw) = run_connect(&fixture, None, size, false).await;
        result.unwrap_or_else(|failure| panic!("{size:?}: {}", failure.message));
        assert!(raw, "{size:?}");
        assert_eq!(
            fixture.stored().unwrap(),
            fixture.token.as_bytes(),
            "{size:?}"
        );
        assert_not_shown(&screen, &fixture.token);
        let display = super::redact::tests::rendered(&screen.bytes(), size.0, size.1).join("\n");
        assert!(display.contains(MASK), "{size:?}:\n{display}");
        assert!(display.contains("Your OAuth token (valid for 1 year):"));
        assert!(display.contains("Store this token securely."));
        assert_written_nowhere_else(&fixture, started);
    }
}

/// A terminal smaller than the smallest `claude` runs in: `claude` gets a
/// 40-by-24 terminal, so Claude Code draws the whole token, which is stored
/// and never shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_small_terminal_gives_claude_the_smallest_size_it_draws_the_token_in() {
    assert_eq!(relay::program_size((30, 12)), (40, 24));
    assert_eq!(relay::program_size((30, 50)), (40, 50));
    assert_eq!(relay::program_size((120, 40)), (120, 40));
    let fixture = Fixture::new("replay");
    fixture.replay(relay::MIN_SIZE, Shape::Claude);
    let (result, screen, _) = run_connect(&fixture, None, (30, 12), false).await;
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    assert_not_shown(&screen, &fixture.token);
}

/// The token's frame written in pieces with pauses (so the relay shows what
/// it held back), cut inside the prefix, inside the token's first row, in
/// the cursor movement to its next row and inside that row; then a write to
/// the clipboard, which is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_codes_rendering_cut_inside_the_token_is_stored_and_never_shown() {
    let size = (60, 24);
    let fixture = Fixture::new("replay");
    let (before, after) = ink_model::session(&fixture.token, 60, 24, Shape::Claude);
    let at = after
        .windows(7)
        .position(|window| window == b"sk-ant-")
        .unwrap();
    let cuts = [at + 2, at + 5, at + 40, at + 62, at + 75]
        .map(|cut| cut.to_string())
        .join(" ");
    std::fs::write(fixture.root.join("bin/cuts"), cuts).unwrap();
    // A write to the clipboard, as Claude Code makes when asked to copy
    // the sign-in URL.
    let mut after = after;
    after.extend_from_slice(b"\x1b]52;c;aHR0cHM6Ly9jbGF1ZGUuYWkvb2F1dGg=\x07");
    fixture.write_replay(size, &before, &after);
    let (result, screen, _) = run_connect(&fixture, None, size, false).await;
    result.unwrap();
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    assert_not_shown(&screen, &fixture.token);
    let text = String::from_utf8_lossy(&screen.bytes()).into_owned();
    assert!(!text.contains("sk-ant-"));
    assert!(!text.contains("]52;"));
}

/// The user's terminal is resized while Claude Code waits for the sign-in:
/// `claude`'s terminal is resized with it (it gets `SIGWINCH` and sees the
/// new size), Claude Code redraws for it, and the token, wrapped for the new
/// width, is stored exactly and never shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_while_signing_in_resizes_claudes_terminal() {
    let (old, new) = ((100u16, 30u16), (60u16, 30u16));
    let fixture = Fixture::new("replay");
    fixture.replay(old, Shape::Claude);
    let after = ink_model::resized_session(&fixture.token, (100, 30), (60, 30));
    std::fs::write(
        fixture.root.join(format!("bin/after-{}x{}", new.0, new.1)),
        &after,
    )
    .unwrap();
    let terminal = user_terminal();
    terminal.set_size(old);
    let screen = Screen::default();
    let interrupt = Interrupt::default();
    let resized = Resized::default();
    let typing = {
        let master = terminal.master.try_clone().unwrap();
        let slave = terminal.slave.try_clone().unwrap();
        let screen = screen.clone();
        let resized = resized.clone();
        std::thread::spawn(move || {
            wait_for(&screen, ink_model::PROMPT);
            // What the terminal does on a resize, and the SIGWINCH the
            // command then gets.
            set_size(&slave, new);
            resized.raise();
            wait_for(&screen, "[size 30 60]");
            rustix::io::write(&master, b"\r").unwrap();
        })
    };
    let result = finish_within(
        connect(
            fixture.request(None),
            terminal.slave.try_clone().unwrap(),
            Box::new(screen.clone()),
            &interrupt,
            &resized,
        ),
        &interrupt,
    )
    .await;
    typing.join().unwrap();
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(screen.contains("[SIGWINCH]"));
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    assert_not_shown(&screen, &fixture.token);
}

/// What the user types once `claude` has exited, while a process it left
/// keeps its output open and while the token is checked, is not read away:
/// it stays in the terminal for the shell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keystrokes_typed_after_claude_exits_stay_in_the_terminal() {
    let fixture = Fixture::new("linger");
    let anthropic =
        FakeAnthropic::start_slow(&fixture.token, 200, Duration::from_millis(1500)).await;
    let terminal = user_terminal();
    terminal.set_size(SIZE);
    let screen = Screen::default();
    let interrupt = Interrupt::default();
    let typing = typist(&terminal, &screen, "Press Enter to continue", None);
    let late = {
        let master = terminal.master.try_clone().unwrap();
        let screen = screen.clone();
        std::thread::spawn(move || {
            wait_for(&screen, "You will not be able to see it again.");
            std::thread::sleep(Duration::from_millis(300));
            rustix::io::write(&master, b"abc").unwrap();
            wait_for(&screen, "Checking the token");
            rustix::io::write(&master, b"def\r").unwrap();
        })
    };
    let result = finish_within(
        connect(
            fixture.request(Some(anthropic.verifier())),
            terminal.slave.try_clone().unwrap(),
            Box::new(screen.clone()),
            &interrupt,
            &Resized::default(),
        ),
        &interrupt,
    )
    .await;
    typing.join().unwrap();
    late.join().unwrap();
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert_eq!(fixture.stored().unwrap(), fixture.token.as_bytes());
    let pending = terminal.pending_input(Duration::from_secs(2));
    let text = String::from_utf8_lossy(&pending);
    assert!(text.contains("abc"), "{text:?}");
    assert!(text.contains("def"), "{text:?}");
}

/// The panic hook puts a raw terminal back even when no destructor runs
/// (the release profile aborts on panic).
#[test]
fn the_panic_hook_restores_a_raw_terminal() {
    let terminal = user_terminal();
    let before = modes(&terminal.slave);
    let raw = relay::RawMode::enter(terminal.slave.as_fd()).unwrap();
    assert!(is_raw(&terminal.slave));
    std::mem::forget(raw);
    let panicked = std::panic::catch_unwind(|| panic!("a panic while the terminal is raw"));
    assert!(panicked.is_err());
    assert_eq!(modes(&terminal.slave), before);

    let raw = relay::RawMode::enter(terminal.slave.as_fd()).unwrap();
    assert!(is_raw(&terminal.slave));
    drop(raw);
    assert_eq!(modes(&terminal.slave), before);
}

#[test]
fn claude_is_found_from_the_flag_then_path_then_local_bin() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let write = |path: &Path, mode: u32| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let on_path = root.join("path-a/claude");
    let not_executable = root.join("path-b/claude");
    let local = root.join("home/.local/bin/claude");
    write(&on_path, 0o755);
    write(&not_executable, 0o644);
    write(&local, 0o755);
    let path = std::env::join_paths([root.join("path-b"), root.join("path-a")]).unwrap();
    let home = root.join("home");

    assert_eq!(
        find_claude(None, Some(&path), Some(&home)).unwrap(),
        on_path
    );
    assert_eq!(
        find_claude(None, Some(std::ffi::OsStr::new("relative:")), Some(&home)).unwrap(),
        local
    );
    assert_eq!(find_claude(Some(&local), Some(&path), None).unwrap(), local);
    let error = find_claude(Some(&not_executable), Some(&path), Some(&home)).unwrap_err();
    assert!(error.contains("is not an executable file"), "{error}");
    assert!(
        !error.contains(&*not_executable.to_string_lossy()),
        "{error}"
    );
    // A key given where the path belongs is never repeated.
    let key = "sk-proj-0123456789abcdefTYPEDKEY";
    for error in [
        host_codex(Path::new(key)).unwrap_err(),
        find_claude(Some(Path::new(key)), Some(&path), Some(&home)).unwrap_err(),
    ] {
        assert!(error.contains("is not an executable file"), "{error}");
        assert!(!error.contains("TYPEDKEY"), "{error}");
    }
    let error = find_claude(None, None, Some(&root)).unwrap_err();
    assert!(error.contains("--claude"), "{error}");
}

#[test]
fn the_command_line_takes_no_token() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: ConnectCommands,
    }
    let parsed = Cli::try_parse_from(["x", "claude-code"]).unwrap();
    let ConnectCommands::ClaudeCode {
        secret,
        no_verify,
        claude,
        ..
    } = parsed.command
    else {
        panic!("claude-code")
    };
    assert_eq!(secret, "claude-code-oauth");
    assert!(!no_verify);
    assert!(claude.is_none());
    let parsed = Cli::try_parse_from([
        "x",
        "claude-code",
        "--secret",
        "work-claude",
        "--no-verify",
        "--claude",
        "/opt/claude",
        "-c",
        "/tmp/a.yaml",
    ])
    .unwrap();
    let ConnectCommands::ClaudeCode {
        secret,
        no_verify,
        claude,
        config,
    } = parsed.command
    else {
        panic!("claude-code")
    };
    assert_eq!(secret, "work-claude");
    assert!(no_verify);
    assert_eq!(claude.as_deref(), Some(Path::new("/opt/claude")));
    assert_eq!(config, Path::new("/tmp/a.yaml"));
    for argv in [
        &["x", "claude-code", "sk-ant-oat01-value"][..],
        &["x", "claude-code", "--token", "sk-ant-oat01-value"],
    ] {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
}

#[test]
fn the_confirmation_names_the_secret_and_the_next_step() {
    let message = connected_message("claude-code-oauth");
    assert!(message.starts_with(
        "Connected Claude Code: stored secret claude-code-oauth (the token was never shown)"
    ));
    assert!(message.contains("runtime: claude-code"));
    let message = connected_message("work-claude");
    assert!(message.contains("name work-claude as a route's `credential`"));
    assert!(needs_terminal().contains("--from-env"));
}

// ------------------------------------------------------------ connect codex

const FAKE_CODEX: &str = include_str!("../../tests/fixtures/fake-codex.sh");
/// What the fake `codex` shows when it waits for the sign-in.
const CODEX_PROMPT: &str = "Press Enter once you have signed in";

/// A JWT-shaped token unique to the test (the CLI never decodes one).
fn fake_jwt() -> String {
    let part = || uuid::Uuid::new_v4().simple().to_string();
    format!(
        "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJ{}{}.{}_{}",
        part(),
        part(),
        part(),
        part()
    )
}

/// A private directory for one `connect codex` test: `bin/codex` (the
/// fake, with its `mode`, the `auth` it writes and the `screen` lines it
/// prints), `data/` (the data root) and `home/`.
struct CodexFixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    access: String,
    refresh: String,
    /// What the fake prints on screen: an API key and a JWT.
    shown_key: String,
    shown_jwt: String,
    /// The `OPENAI_API_KEY` the fake writes to `auth.json`
    /// ([`CodexFixture::with_api_key`]).
    api_key: Option<String>,
    /// What `connect codex` is asked to store.
    keep: codex::Keep,
}

impl CodexFixture {
    fn new(mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("codex"), FAKE_CODEX).unwrap();
        std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(bin.join("mode"), mode).unwrap();
        let (access, refresh) = (fake_jwt(), format!("rt_{}", uuid::Uuid::new_v4().simple()));
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": fake_jwt(),
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct-fixture",
            },
            "last_refresh": "2026-10-11T12:00:00Z",
        });
        std::fs::write(
            bin.join("auth"),
            serde_json::to_string_pretty(&auth).unwrap(),
        )
        .unwrap();
        let shown_key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
        let shown_jwt = fake_jwt();
        std::fs::write(bin.join("screen"), format!("{shown_key}\n{shown_jwt}\n")).unwrap();
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        Self {
            directory,
            root,
            access,
            refresh,
            shown_key,
            shown_jwt,
            api_key: None,
            keep: codex::Keep::SignIn,
        }
    }

    /// The fake writes `key` (a JSON value) as `auth.json`'s
    /// `OPENAI_API_KEY`, and `connect codex` is asked to keep `keep`.
    fn with_api_key(mut self, key: serde_json::Value, keep: codex::Keep) -> Self {
        let path = self.root.join("bin/auth");
        let mut auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        auth["OPENAI_API_KEY"] = key.clone();
        std::fs::write(&path, serde_json::to_string_pretty(&auth).unwrap()).unwrap();
        self.api_key = key.as_str().map(str::to_string);
        self.keep = keep;
        self
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn request(&self, device_auth: bool) -> codex::ConnectCodex {
        codex::ConnectCodex {
            runner: codex::Runner::Host {
                codex: self.root.join("bin/codex"),
            },
            data_dir: self.data(),
            device_auth,
            keep: self.keep,
        }
    }

    fn stored(&self) -> Option<Vec<u8>> {
        std::fs::read(secret_store::secret_path(
            &self.data(),
            axocoatl_daemon::external_agent::codex_chatgpt::CODEX_CHATGPT_SECRET,
        ))
        .ok()
    }

    /// What the fake saw: `name=value` lines.
    fn seen(&self) -> String {
        std::fs::read_to_string(self.root.join("bin/seen")).unwrap_or_default()
    }

    /// The data root holds nothing but the secret store (no temporary
    /// CODEX_HOME is left).
    fn assert_no_home_left(&self) {
        let mut entries: Vec<String> = std::fs::read_dir(self.data())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(codex::TEMP_HOME_PREFIX))
            .collect();
        entries.sort();
        assert!(entries.is_empty(), "{entries:?}");
    }
}

/// Run `connect codex` on a fresh user terminal; its result and what it
/// showed.
async fn run_connect_codex(
    fixture: &CodexFixture,
    device_auth: bool,
    interrupt_at_prompt: bool,
) -> (Result<String, Failure>, Screen) {
    let terminal = user_terminal();
    terminal.set_size(SIZE);
    let before = modes(&terminal.slave);
    let screen = Screen::default();
    let interrupt = Interrupt::default();
    let typing = typist(
        &terminal,
        &screen,
        CODEX_PROMPT,
        interrupt_at_prompt.then(|| interrupt.clone()),
    );
    let result = finish_within(
        codex::connect(
            fixture.request(device_auth),
            terminal.slave.try_clone().unwrap(),
            Box::new(screen.clone()),
            &interrupt,
            &Resized::default(),
        ),
        &interrupt,
    )
    .await;
    assert!(
        typing.join().unwrap(),
        "the terminal was raw while codex ran"
    );
    assert_eq!(modes(&terminal.slave), before);
    (result, screen)
}

/// The tokens Codex wrote are stored exactly, as one sign-in; none of them,
/// nor the key and JWT the fake printed, is shown; the login URL is shown
/// whole; the temporary CODEX_HOME (where the fake also left a log and a
/// copy of auth.json) is gone; and no token is in any other file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_stores_the_sign_in_codex_wrote_and_erases_its_home() {
    use axocoatl_daemon::external_agent::codex_chatgpt::ChatgptSignIn;
    let since = SystemTime::now();
    let fixture = CodexFixture::new("ok");
    let (result, screen) = run_connect_codex(&fixture, false, false).await;
    let message = result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(
        message.starts_with("Connected Codex: stored your ChatGPT sign-in as secret codex-chatgpt")
    );
    assert!(message.contains("credential: codex-chatgpt"), "{message}");
    // Stored exactly, 0600, in the secret store.
    let stored = fixture.stored().expect("the sign-in is stored");
    let sign_in = ChatgptSignIn::decode(&stored).unwrap();
    assert_eq!(sign_in.access_token(), fixture.access);
    assert_eq!(sign_in.refresh_token(), fixture.refresh);
    assert_eq!(sign_in.account_id(), Some("acct-fixture"));
    let path = secret_store::secret_path(&fixture.data(), "codex-chatgpt");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Codex ran with a private temporary home in the data root, there.
    let seen = fixture.seen();
    let home = seen
        .lines()
        .find_map(|line| line.strip_prefix("codex_home="))
        .unwrap();
    assert!(
        home.starts_with(&format!(
            "{}/{}",
            fixture.data().display(),
            codex::TEMP_HOME_PREFIX
        )),
        "{seen}"
    );
    assert!(seen.contains(&format!("pwd={home}\n")), "{seen}");
    assert!(seen.contains("home_mode=700\n"), "{seen}");
    assert!(seen.contains("device=no\n"), "{seen}");
    assert!(!Path::new(home).exists(), "{home} was not removed");
    fixture.assert_no_home_left();
    // Nothing secret on screen; the URL and Codex's own lines are.
    for secret in [
        &fixture.access,
        &fixture.refresh,
        &fixture.shown_key,
        &fixture.shown_jwt,
    ] {
        assert_not_shown(&screen, secret);
    }
    let shown = String::from_utf8_lossy(&screen.bytes()).into_owned();
    assert!(shown.contains("Successfully logged in"), "{shown}");
    assert!(shown.contains(MASK), "{shown}");
    let url = "state=Qsk-pLkJhGfDsAeyJ0x&originator=codex_cli_rs";
    let flat: String = shown.split(['\r', '\n']).collect();
    assert!(flat.contains(url), "{shown}");
    // The tokens are in the fake's own file and the secret, nowhere else.
    for secret in [&fixture.access, &fixture.refresh] {
        let mut hits = Vec::new();
        files_containing(&fixture.root, secret.as_bytes(), 8, None, &[], &mut hits);
        hits.sort();
        let mut expected = vec![fixture.root.join("bin/auth"), path.clone()];
        expected.sort();
        assert_eq!(hits, expected);
        let mut elsewhere = Vec::new();
        files_containing(
            &std::env::temp_dir(),
            secret.as_bytes(),
            4,
            Some(since),
            &[fixture.root.clone(), fixture.directory.path().to_path_buf()],
            &mut elsewhere,
        );
        assert!(elsewhere.is_empty(), "{elsewhere:?}");
    }
}

/// `--device-auth` runs `codex login --device-auth`, whose code is shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_with_device_auth_shows_the_code() {
    let fixture = CodexFixture::new("ok");
    let (result, screen) = run_connect_codex(&fixture, true, false).await;
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(fixture.seen().contains("device=yes\n"));
    assert!(screen.contains("WXYZ-1234"));
    assert!(screen.contains("https://auth.openai.com/codex/device"));
    assert!(fixture.stored().is_some());
    fixture.assert_no_home_left();
}

/// No ChatGPT sign-in, an API key instead, or an auth.json that is a link:
/// nothing is stored, the temporary home is erased (never what a link in it
/// points to), and the message says why without a value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_without_a_chatgpt_sign_in_stores_nothing() {
    for (mode, code, said) in [
        ("none", exit::NO_TOKEN, "without storing a sign-in"),
        ("apikey", exit::NO_TOKEN, "not a ChatGPT sign-in"),
        ("linked", exit::FAILURE, "auth.json cannot be opened"),
    ] {
        let fixture = CodexFixture::new(mode);
        let auth = std::fs::read(fixture.root.join("bin/auth")).unwrap();
        let (result, screen) = run_connect_codex(&fixture, false, false).await;
        let failure = result
            .err()
            .unwrap_or_else(|| panic!("{mode} stored something"));
        assert_eq!(failure.code, code, "{mode}: {}", failure.message);
        assert!(
            failure.message.contains(said),
            "{mode}: {}",
            failure.message
        );
        assert!(failure.message.contains("nothing was stored") || code == exit::FAILURE);
        for secret in [&fixture.access, &fixture.refresh] {
            assert!(!failure.message.contains(secret.as_str()));
            assert_not_shown(&screen, secret);
        }
        assert!(!failure.message.contains("sk-proj-"));
        assert!(fixture.stored().is_none(), "{mode}");
        fixture.assert_no_home_left();
        assert_eq!(std::fs::read(fixture.root.join("bin/auth")).unwrap(), auth);
    }
}

/// An OpenAI API key shaped as Codex writes one, unique to the test.
fn fake_api_key() -> String {
    format!(
        "sk-proj-{}{}_{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The secret `name` as stored, if it is.
fn stored_secret(data: &Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(secret_store::secret_path(data, name)).ok()
}

/// `--api-key`: the key Codex wrote is stored exactly as `codex-openai`
/// (`0600`) beside the ChatGPT sign-in; neither is shown, and the key is in
/// no file but the fake's own and the secret (the temporary home, where the
/// fake also left a copy of auth.json, is gone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_with_api_key_stores_the_key_and_the_sign_in() {
    use axocoatl_daemon::external_agent::codex_chatgpt::ChatgptSignIn;
    let since = SystemTime::now();
    let key = fake_api_key();
    let fixture =
        CodexFixture::new("ok").with_api_key(serde_json::json!(key), codex::Keep::ApiKeyAndSignIn);
    let (result, screen) = run_connect_codex(&fixture, false, false).await;
    let message = result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(
        message.starts_with(
            "Connected Codex: stored the OpenAI API key OpenAI issued with your sign-in as \
             secret codex-openai and your ChatGPT sign-in as secret codex-chatgpt"
        ),
        "{message}"
    );
    assert!(message.contains("billed at OpenAI API rates"), "{message}");
    assert!(message.contains("not to your ChatGPT plan"), "{message}");
    assert!(!message.contains(&key));
    let path = secret_store::secret_path(&fixture.data(), "codex-openai");
    assert_eq!(std::fs::read(&path).unwrap(), key.as_bytes());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let sign_in = ChatgptSignIn::decode(&fixture.stored().unwrap()).unwrap();
    assert_eq!(sign_in.access_token(), fixture.access);
    assert_eq!(sign_in.refresh_token(), fixture.refresh);
    let stored_sign_in = String::from_utf8(fixture.stored().unwrap()).unwrap();
    assert!(!stored_sign_in.contains(&key));
    fixture.assert_no_home_left();
    for secret in [
        &key,
        &fixture.access,
        &fixture.refresh,
        &fixture.shown_key,
        &fixture.shown_jwt,
    ] {
        assert_not_shown(&screen, secret);
    }
    let mut hits = Vec::new();
    files_containing(&fixture.root, key.as_bytes(), 8, None, &[], &mut hits);
    hits.sort();
    let mut expected = vec![fixture.root.join("bin/auth"), path];
    expected.sort();
    assert_eq!(hits, expected);
    let mut elsewhere = Vec::new();
    files_containing(
        &std::env::temp_dir(),
        key.as_bytes(),
        4,
        Some(since),
        &[fixture.root.clone(), fixture.directory.path().to_path_buf()],
        &mut elsewhere,
    );
    assert!(elsewhere.is_empty(), "{elsewhere:?}");
    assert_eq!(fixture.api_key.as_deref(), Some(key.as_str()));
}

/// `--api-key-only`: the key is stored, the ChatGPT sign-in is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_with_api_key_only_stores_only_the_key() {
    let key = fake_api_key();
    let fixture =
        CodexFixture::new("ok").with_api_key(serde_json::json!(key), codex::Keep::ApiKeyOnly);
    let (result, screen) = run_connect_codex(&fixture, false, false).await;
    let message = result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(
        message.starts_with(
            "Connected Codex: stored the OpenAI API key OpenAI issued with your sign-in as \
             secret codex-openai (no key or token was shown, your ChatGPT sign-in was not kept"
        ),
        "{message}"
    );
    assert_eq!(
        stored_secret(&fixture.data(), "codex-openai").unwrap(),
        key.as_bytes()
    );
    assert!(fixture.stored().is_none());
    assert_not_shown(&screen, &key);
    fixture.assert_no_home_left();
}

/// `--api-key` when OpenAI issued no key (`OPENAI_API_KEY` null, absent or
/// empty, as Codex writes it when the exchange failed), or when the value
/// is not a key: nothing is stored as codex-openai, the message says so
/// plainly without a value, and the exit code is NO_API_KEY; the ChatGPT
/// sign-in is still stored with `--api-key`, not with `--api-key-only`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_with_api_key_when_none_was_issued() {
    let malformed = format!("sk-proj {}", uuid::Uuid::new_v4().simple());
    for (key, keep, issued_none) in [
        (serde_json::Value::Null, codex::Keep::ApiKeyAndSignIn, true),
        (serde_json::json!(""), codex::Keep::ApiKeyAndSignIn, true),
        (serde_json::Value::Null, codex::Keep::ApiKeyOnly, true),
        (
            serde_json::json!(malformed),
            codex::Keep::ApiKeyAndSignIn,
            false,
        ),
    ] {
        let fixture = CodexFixture::new("ok").with_api_key(key.clone(), keep);
        let (result, screen) = run_connect_codex(&fixture, false, false).await;
        let failure = result
            .err()
            .unwrap_or_else(|| panic!("{key} {keep:?}: no failure"));
        assert_eq!(failure.code, exit::NO_API_KEY, "{}", failure.message);
        assert_eq!(
            failure.message,
            codex::no_api_key(keep, !issued_none, false)
        );
        if issued_none {
            assert!(
                failure.message.starts_with(&format!(
                    "OpenAI issued no API key with this sign-in, so nothing was stored as \
                     codex-openai. API-key mode needs an OpenAI Platform organization \
                     (https://platform.openai.com); if your account has one, the request for \
                     the key may have failed, so run `axocoatl connect codex {}` again.",
                    keep.flag()
                )),
                "{}",
                failure.message
            );
        } else {
            assert!(failure.message.contains("does not have the form"));
            assert!(!failure.message.contains(&malformed));
            assert_not_shown(&screen, &malformed);
        }
        assert!(stored_secret(&fixture.data(), "codex-openai").is_none());
        if keep == codex::Keep::ApiKeyAndSignIn {
            assert!(fixture.stored().is_some(), "{key}");
            assert!(failure
                .message
                .contains("Your ChatGPT sign-in was stored as codex-chatgpt: ChatGPT mode works"));
        } else {
            assert!(fixture.stored().is_none());
            assert!(failure.message.contains("Nothing else was stored."));
            assert!(failure
                .message
                .contains("ChatGPT mode works with codex-chatgpt"));
        }
        fixture.assert_no_home_left();
    }

    // A key stored before is left as it was, and the message says so.
    let fixture =
        CodexFixture::new("ok").with_api_key(serde_json::Value::Null, codex::Keep::ApiKeyAndSignIn);
    let earlier = fake_api_key();
    secret_store::set_secret(&fixture.data(), "codex-openai", earlier.as_bytes()).unwrap();
    let (result, _) = run_connect_codex(&fixture, false, false).await;
    let failure = result.expect_err("no failure");
    assert_eq!(failure.code, exit::NO_API_KEY, "{}", failure.message);
    assert!(
        failure.message.contains(
            "nothing was stored as codex-openai (the codex-openai already stored was left as it \
             was, and a Codex writer still uses it by default)."
        ),
        "{}",
        failure.message
    );
    assert!(!failure.message.contains(&earlier));
    assert_eq!(
        stored_secret(&fixture.data(), "codex-openai").unwrap(),
        earlier.as_bytes()
    );
    fixture.assert_no_home_left();
}

/// `--api-key` with `--device-auth` is refused before Codex runs: the
/// device-code sign-in gets no key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_refuses_api_key_with_device_auth() {
    for keep in [codex::Keep::ApiKeyAndSignIn, codex::Keep::ApiKeyOnly] {
        let fixture = CodexFixture::new("ok").with_api_key(serde_json::json!(fake_api_key()), keep);
        let input: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let failure = codex::connect(
            fixture.request(true),
            input,
            Box::new(Vec::new()),
            &Interrupt::default(),
            &Resized::default(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(failure.code, exit::USAGE);
        assert_eq!(failure.message, codex::api_key_needs_browser(keep.flag()));
        assert!(failure.message.starts_with(&format!(
            "{} cannot be used with --device-auth: OpenAI issues the API key only with the \
             browser sign-in",
            keep.flag()
        )));
        assert_eq!(fixture.seen(), "", "codex ran");
        assert!(fixture.stored().is_none());
        assert!(stored_secret(&fixture.data(), "codex-openai").is_none());
    }
}

/// The form of the key Codex writes, and reading it from `auth.json`.
#[test]
fn the_api_key_is_read_from_auth_json_in_the_form_codex_writes() {
    let key = fake_api_key();
    assert!(codex::is_api_key(&key));
    assert!(codex::is_api_key("sk-abcdefghijklmnopqrstu"));
    for bad in [
        "sk-short",
        "pk-abcdefghijklmnopqrstuvwxyz",
        "sk-abc def ghi jkl mno pqr",
        "sk-abcdefghijklmnopqrstu\n",
        "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.sig",
        "sk-ant-oat01-abcdefghijklmnop.qrstuvwxyz",
    ] {
        assert!(!codex::is_api_key(bad), "{bad:?}");
    }
    assert!(!codex::is_api_key(&format!(
        "sk-{}",
        "a".repeat(codex::MAX_API_KEY_BYTES)
    )));
    let read = |json: &str| codex::api_key_from_auth_json(json.as_bytes());
    match read(&format!(
        "{{\"auth_mode\":\"chatgpt\",\"OPENAI_API_KEY\":\"{key}\"}}"
    )) {
        codex::ApiKey::Issued(read) => assert_eq!(read.as_str(), key),
        _ => panic!("not read"),
    }
    for none in [
        "{\"OPENAI_API_KEY\":null}",
        "{\"OPENAI_API_KEY\":\"\"}",
        "{\"auth_mode\":\"chatgpt\"}",
    ] {
        assert!(matches!(read(none), codex::ApiKey::None), "{none}");
    }
    for malformed in [
        "{\"OPENAI_API_KEY\":\"not a key\"}",
        "{\"OPENAI_API_KEY\":42}",
        "not json",
    ] {
        assert!(
            matches!(read(malformed), codex::ApiKey::Malformed),
            "{malformed}"
        );
    }
    assert_eq!(codex::Keep::from_flags(false, false), codex::Keep::SignIn);
    assert_eq!(
        codex::Keep::from_flags(true, false),
        codex::Keep::ApiKeyAndSignIn
    );
    assert_eq!(
        codex::Keep::from_flags(false, true),
        codex::Keep::ApiKeyOnly
    );
    assert_eq!(codex::Keep::from_flags(true, true), codex::Keep::ApiKeyOnly);
    assert_eq!(codex::Keep::SignIn.secrets(), ["codex-chatgpt"]);
    assert_eq!(
        codex::Keep::ApiKeyAndSignIn.secrets(),
        ["codex-openai", "codex-chatgpt"]
    );
    assert_eq!(codex::Keep::ApiKeyOnly.secrets(), ["codex-openai"]);
}

/// A signal while Codex waits: nothing is stored and the home is erased.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_interrupted_stores_nothing_and_erases_its_home() {
    let fixture = CodexFixture::new("ok");
    let (result, _) = run_connect_codex(&fixture, false, true).await;
    let failure = result.err().unwrap();
    assert_eq!(failure.code, exit::INTERRUPTED);
    assert!(failure.message.contains("erased"), "{}", failure.message);
    assert!(fixture.stored().is_none());
    fixture.assert_no_home_left();
}

/// A temporary home an earlier, killed `connect codex` left is erased
/// first; the erasure overwrites files before removing them and never
/// follows a link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_codex_erases_what_a_killed_run_left() {
    let fixture = CodexFixture::new("ok");
    let left = fixture
        .data()
        .join(format!("{}old", codex::TEMP_HOME_PREFIX));
    std::fs::create_dir_all(left.join("log")).unwrap();
    std::fs::write(left.join("auth.json"), "left-behind-secret").unwrap();
    let outside = fixture.root.join("outside");
    std::fs::write(&outside, "keep me").unwrap();
    std::os::unix::fs::symlink(&outside, left.join("log/link")).unwrap();
    // One a run going on now would have: too new to be erased.
    let current = fixture
        .data()
        .join(format!("{}now", codex::TEMP_HOME_PREFIX));
    std::fs::create_dir(&current).unwrap();
    let old = SystemTime::now() - codex::LEFTOVER_AGE - Duration::from_secs(60);
    std::fs::File::open(&left)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let (result, _) = run_connect_codex(&fixture, false, false).await;
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(!left.exists());
    assert!(current.exists());
    std::fs::remove_dir(&current).unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me");
    fixture.assert_no_home_left();

    // `erase_dir` on its own: the bytes are overwritten before removal.
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("home");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("auth.json"), "secret-bytes").unwrap();
    let file = std::fs::File::open(target.join("auth.json")).unwrap();
    codex::erase_dir(&target);
    assert!(!target.exists());
    // The open handle still reads the (now zeroed) contents.
    let mut contents = Vec::new();
    std::io::Read::read_to_end(&mut &file, &mut contents).unwrap();
    assert_eq!(contents, vec![0u8; "secret-bytes".len()]);
}

#[test]
fn connect_codex_runs_codex_login_with_file_storage_and_no_other_credentials() {
    assert_eq!(
        codex::login_args(false),
        ["-c", "cli_auth_credentials_store=\"file\"", "login"]
    );
    assert_eq!(
        codex::login_args(true),
        [
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "--device-auth"
        ]
    );
    for variable in [
        "CODEX_API_KEY",
        "OPENAI_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        "CODEX_SQLITE_HOME",
    ] {
        assert!(codex::REMOVED_ENV.contains(&variable), "{variable}");
    }
    assert!(codex::needs_terminal().contains("codex login"));
    assert!(codex::needs_terminal().contains("--device-auth"));
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: ConnectCommands,
    }
    let parsed = Cli::try_parse_from(["x", "codex"]).unwrap();
    let ConnectCommands::Codex {
        device_auth,
        codex,
        no_browser,
        ..
    } = parsed.command
    else {
        panic!("codex")
    };
    assert!(!device_auth);
    assert!(codex.is_none());
    assert!(!no_browser);
    let parsed = Cli::try_parse_from([
        "x",
        "codex",
        "--device-auth",
        "--codex",
        "/opt/codex",
        "-c",
        "/tmp/a.yaml",
    ])
    .unwrap();
    let ConnectCommands::Codex {
        device_auth,
        codex,
        config,
        no_browser,
        ..
    } = parsed.command
    else {
        panic!("codex")
    };
    assert!(!no_browser);
    assert!(device_auth);
    assert_eq!(codex.as_deref(), Some(Path::new("/opt/codex")));
    assert_eq!(config, Path::new("/tmp/a.yaml"));
    for argv in [
        &["x", "codex", "--secret", "other"][..],
        &["x", "codex", "--token", "value"],
        &["x", "codex", "--api-key", "--status"],
        &["x", "codex", "--api-key-only", "--refresh"],
    ] {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
    // A value on the command line (a key after `--api-key`) is taken, to be
    // refused by `cmd_connect` without being repeated, as clap would.
    for argv in [
        &["x", "codex", "eyJhbGciOi.value.sig"][..],
        &["x", "codex", "--api-key", "sk-proj-typed-by-hand"],
    ] {
        let parsed = Cli::try_parse_from(argv).unwrap();
        let ConnectCommands::Codex {
            on_command_line, ..
        } = parsed.command
        else {
            panic!("codex")
        };
        assert_eq!(on_command_line.len(), 1, "{argv:?}");
    }
    assert!(!VALUE_ON_COMMAND_LINE.contains("sk-"));
    let refused = flag_takes_no_value("--api-key");
    assert!(refused.contains("--api-key takes no value"), "{refused}");
    assert!(refused.contains("signs you in in the browser"), "{refused}");
    assert!(!flag_takes_no_value("--no-browser").contains("signs you in"));
    let parsed = Cli::try_parse_from(["x", "codex", "--api-key"]).unwrap();
    let ConnectCommands::Codex {
        api_key,
        api_key_only,
        ..
    } = parsed.command
    else {
        panic!("codex")
    };
    assert!(api_key && !api_key_only);
    let parsed = Cli::try_parse_from(["x", "codex", "--api-key-only"]).unwrap();
    let ConnectCommands::Codex {
        api_key,
        api_key_only,
        ..
    } = parsed.command
    else {
        panic!("codex")
    };
    assert!(!api_key && api_key_only);
    // `--codex PATH` must name an executable file; no `codex` on PATH or in
    // ~/.local/bin is used without it.
    let root = tempfile::tempdir().unwrap();
    let local = root.path().join("codex");
    let error = host_codex(&local).unwrap_err();
    assert!(error.contains("--codex"), "{error}");
    std::fs::write(&local, "#!/bin/sh\n").unwrap();
    assert!(host_codex(&local).is_err());
    std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(host_codex(&local).unwrap(), local);
}

/// The login container: the pinned image by id, never pulled; a private
/// `0700` tmpfs `CODEX_HOME` and nothing from this computer mounted; no
/// capabilities, a read-only root, a non-root user, no log; stopped and
/// removed by Podman after at most [`codex_container::DEFAULT_LIFETIME_SECS`];
/// for the browser sign-in exactly one port published, on this computer's
/// loopback only, and with `--device-auth` none.
#[test]
fn the_login_container_publishes_only_the_callback_on_loopback_and_mounts_nothing() {
    use codex_container::*;
    let image = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let name = container_name();
    assert!(name.starts_with(NAME_PREFIX));
    assert_ne!(name, container_name());
    let browser = run_args(&name, image, false, DEFAULT_LIFETIME);
    let device = run_args(&name, image, true, DEFAULT_LIFETIME);
    for args in [&browser, &device] {
        assert_eq!(
            &args[..5],
            ["run", "--detach", "--rm", "--name", name.as_str()]
        );
        for expected in [
            "--pull=never",
            "--network=bridge",
            "--read-only",
            "--cap-drop=all",
            "--security-opt=no-new-privileges",
            "--log-driver=none",
            "--user=1000:1000",
            "--timeout=1800",
            "--env=CODEX_HOME=/codex-home",
            "--env=HOME=/codex-home",
            "--workdir=/codex-home",
            "--mount=type=tmpfs,destination=/codex-home,tmpfs-mode=0700,tmpfs-size=16m,U=true",
            ROLE_LABEL,
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "{expected}: {args:?}"
            );
        }
        // No host path is mounted, no host variable passed through.
        for arg in args.iter() {
            assert!(
                !arg.starts_with("-v")
                    && !arg.starts_with("--volume")
                    && !arg.contains("type=bind")
                    && !arg.starts_with("--env-host")
                    && !arg.starts_with("--env-file"),
                "{arg}"
            );
        }
        let mounts: Vec<&String> = args
            .iter()
            .filter(|arg| arg.starts_with("--mount") || arg.starts_with("--tmpfs"))
            .collect();
        assert_eq!(mounts.len(), 2, "{mounts:?}");
        assert!(mounts.iter().all(|mount| mount.contains("tmpfs")));
        let image_at = args.iter().position(|arg| arg == image).unwrap();
        assert!(args[..image_at].iter().all(|arg| !arg.contains("OPENAI")));
    }
    let published: Vec<&String> = browser
        .iter()
        .filter(|arg| arg.starts_with("--publish") || arg.starts_with("-p"))
        .collect();
    assert_eq!(published, ["--publish=127.0.0.1:1455:14550/tcp"]);
    assert!(browser.ends_with(&[
        "--entrypoint=node".to_string(),
        image.to_string(),
        "-e".to_string(),
        FORWARDER.to_string()
    ]));
    assert!(FORWARDER.contains("net.connect(1455, '127.0.0.1')"));
    assert!(FORWARDER.contains(".listen(14550, '0.0.0.0')"));
    assert!(!device
        .iter()
        .any(|arg| arg.starts_with("--publish") || arg.starts_with("-p")));
    assert!(device.ends_with(&[
        "--entrypoint=sleep".to_string(),
        image.to_string(),
        "infinity".to_string()
    ]));
    assert_eq!(
        login_exec_args(&name, true),
        [
            "exec",
            "--interactive",
            "--tty",
            name.as_str(),
            "codex",
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "--device-auth"
        ]
    );
    let read = read_args(&name);
    assert_eq!(&read[..4], ["exec", name.as_str(), "sh", "-c"]);
    assert!(read[4].contains("head -c 65537"), "{}", read[4]);
    assert_eq!(
        read.last().map(String::as_str),
        Some("/codex-home/auth.json")
    );
    assert!(!read
        .iter()
        .any(|arg| arg == "--tty" || arg == "--interactive"));
    assert_eq!(
        remove_args(&name),
        [
            "rm",
            "--force",
            "--time=0",
            "--volumes",
            "--ignore",
            name.as_str()
        ]
    );
    let missing = find_podman(None).unwrap_err();
    assert!(missing.contains("--codex PATH"), "{missing}");
    // Podman's proxy variables do not reach the container either.
    for args in [&browser, &device] {
        let image_at = args.iter().position(|arg| arg == image).unwrap();
        let proxy: Vec<&String> = args[..image_at]
            .iter()
            .filter(|arg| arg.starts_with("--http-proxy"))
            .collect();
        assert_eq!(proxy, ["--http-proxy=false"]);
    }
}

/// Podman is looked for on PATH only: the recipe image is built with the
/// `podman` on PATH, so a `~/.local/bin/podman` that is not on it is not
/// taken (it would start the sign-in and then fail building the image).
#[test]
fn podman_is_looked_for_on_path_only() {
    let directory = tempfile::tempdir().unwrap();
    let local = directory.path().join(".local/bin");
    std::fs::create_dir_all(&local).unwrap();
    let podman = local.join("podman");
    std::fs::write(&podman, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    let elsewhere = std::ffi::OsString::from("/nonexistent-axocoatl-path");
    let missing = codex_container::find_podman(Some(&elsewhere)).unwrap_err();
    assert!(missing.contains("not found on PATH."), "{missing}");
    assert!(!missing.contains(".local"), "{missing}");
    let on_path = std::env::join_paths([&local]).unwrap();
    assert_eq!(
        codex_container::find_podman(Some(&on_path)).unwrap(),
        podman
    );
}

/// The intro says what to do: Codex in the login container cannot open a
/// browser on this computer, so Axocoatl opens the address it prints when
/// it has an opener (and no --no-browser), and otherwise the intro says to
/// open it; a Codex on this computer opens it itself. The container's
/// intro also says how long the sign-in may take.
#[test]
fn the_container_intro_does_not_promise_a_browser_codex_cannot_open() {
    let container = codex::sign_in_how(false, true, false);
    assert_eq!(
        container,
        "Open the address it prints in a browser on this computer."
    );
    assert!(!container.contains("it opens"));
    let opened = codex::sign_in_how(false, true, true);
    assert!(opened.starts_with("Axocoatl opens the address it prints in your browser"));
    assert!(opened.contains("open that address in a browser on this computer"));
    for opens in [false, true] {
        assert!(codex::sign_in_how(false, false, opens).contains("browser it opens"));
        assert!(codex::sign_in_how(true, false, opens).contains("Enter the code"));
    }
    assert!(codex::sign_in_how(true, true, false).starts_with("Enter the code"));
    let device = codex::sign_in_how(true, true, true);
    assert!(
        device.starts_with("Axocoatl opens the address it gives"),
        "{device}"
    );
    assert!(device.contains("enter the code it shows"), "{device}");
    assert_eq!(
        codex_container::time_to_sign_in(codex_container::DEFAULT_LIFETIME),
        "You have 30 minutes to finish signing in."
    );
}

/// The `podman` commands that start the login container, read the sign-in
/// from it and remove it each run in a process group of their own, so a
/// Ctrl-C, Ctrl-\ or hangup sent to Axocoatl's group once Codex has exited
/// (Axocoatl catches it) cannot kill them and leave the container, and the
/// sign-in in it, behind.
#[test]
fn the_login_containers_podman_commands_run_in_their_own_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("log");
    let podman = directory.path().join("podman");
    std::fs::write(
        &podman,
        format!(
            "#!/bin/sh\nprintf '%s %s %s\\n' \"$1\" \"$$\" \"$(ps -o pgid= -p $$ | tr -d ' ')\" \
             >> '{}'\n[ \"$1\" = exec ] && exit 3\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Wait out a "text file busy" while another test's fork still holds
    // the file open for writing.
    for _ in 0..100 {
        match std::process::Command::new(&podman).arg("probe").status() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => {
                other.unwrap();
                break;
            }
        }
    }
    let container = codex_container::Container::start(
        &podman,
        "image-id",
        true,
        codex_container::DEFAULT_LIFETIME,
    )
    .unwrap();
    assert!(container.read_auth_file().unwrap().is_none());
    drop(container);
    let ours = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let ours = String::from_utf8_lossy(&ours.stdout).trim().to_string();
    assert!(!ours.is_empty());
    let log = std::fs::read_to_string(&log).unwrap();
    let ran: Vec<Vec<&str>> = log
        .lines()
        .map(|line| line.split(' ').collect())
        .filter(|fields: &Vec<&str>| fields[0] != "probe")
        .collect();
    assert_eq!(
        ran.iter().map(|fields| fields[0]).collect::<Vec<_>>(),
        ["run", "exec", "rm"],
        "{log}"
    );
    for fields in &ran {
        assert_eq!(fields.len(), 3, "{log}");
        // Its own group (the group id is its process id), not Axocoatl's.
        assert_eq!(fields[1], fields[2], "{log}");
        assert_ne!(fields[2], ours, "{log}");
    }
}

/// Where [`podman_commands_started_while_ctrl_c_is_typed`] tells
/// [`start_podman_commands_while_ctrl_c_is_typed`] to write.
const CTRL_C_WHILE_STARTING: &str = "AXOCOATL_TEST_CTRL_C_WHILE_STARTING";
/// How many commands it starts.
const STARTED_WHILE_CTRL_C_IS_TYPED: usize = 300;

/// Nor can a Ctrl-C (or Ctrl-\ or hangup) that reaches Axocoatl's process
/// group while one of those `podman` commands is being started kill it: a
/// new process is in Axocoatl's group until it has moved to its own, and
/// the signal must not end it then either. This runs
/// [`start_podman_commands_while_ctrl_c_is_typed`] as the foreground
/// process group of a terminal of its own, with Axocoatl's signal handlers,
/// and types Ctrl-C on that terminal all the while it starts commands the
/// way the `podman` commands are started: none may end by a signal.
#[test]
fn podman_commands_started_while_ctrl_c_is_typed() {
    use std::io::Read;
    let directory = tempfile::tempdir().unwrap();
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "connect_cmd::tests::start_podman_commands_while_ctrl_c_is_typed",
        "--exact",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    command.env(CTRL_C_WHILE_STARTING, directory.path());
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while reader.read(&mut buffer).is_ok_and(|read| read > 0) {}
    });
    let mut writer = pair.master.take_writer().unwrap();
    let ready = directory.path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the commands did not finish");
        if ready.exists() {
            // Each Ctrl-C is a SIGINT to the terminal's foreground group.
            let _ = writer.write_all(b"\x03");
            let _ = writer.flush();
            std::thread::sleep(Duration::from_micros(200));
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let result = std::fs::read_to_string(directory.path().join("result")).unwrap_or_default();
    assert!(status.success(), "{status:?}: {result}");
    assert_eq!(
        result,
        format!(
            "{STARTED_WHILE_CTRL_C_IS_TYPED} started, ended by a signal: {{}}, Ctrl-C meanwhile: \
             true"
        )
    );
}

/// [`podman_commands_started_while_ctrl_c_is_typed`]'s side (it does
/// nothing run any other way): with Axocoatl's handlers for the signals it
/// catches, once the first Ctrl-C has arrived, start
/// [`STARTED_WHILE_CTRL_C_IS_TYPED`] commands as
/// [`codex_container::podman_command`] starts `podman`, and write how many
/// each signal ended and whether Ctrl-C kept arriving meanwhile.
#[test]
#[ignore = "run by podman_commands_started_while_ctrl_c_is_typed"]
fn start_podman_commands_while_ctrl_c_is_typed() {
    use std::os::unix::process::ExitStatusExt;
    let Some(directory) = std::env::var_os(CTRL_C_WHILE_STARTING).map(PathBuf::from) else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let interrupt = Interrupt::default();
    let _listeners = runtime.block_on(async { interrupt.listen() });
    std::fs::write(directory.join("ready"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !interrupt.raised() {
        assert!(Instant::now() < deadline, "no Ctrl-C arrived");
        std::thread::sleep(Duration::from_millis(1));
    }
    let program = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .map(Path::new)
        .find(|program| program.exists())
        .unwrap();
    // Set again by a Ctrl-C while they are started.
    interrupt
        .flag
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let mut signals = std::collections::BTreeMap::<i32, usize>::new();
    for _ in 0..STARTED_WHILE_CTRL_C_IS_TYPED {
        let status = codex_container::podman_command(program)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        if let Some(signal) = status.signal() {
            *signals.entry(signal).or_default() += 1;
        }
    }
    std::fs::write(
        directory.join("result"),
        format!(
            "{STARTED_WHILE_CTRL_C_IS_TYPED} started, ended by a signal: {signals:?}, Ctrl-C \
             meanwhile: {}",
            interrupt.raised()
        ),
    )
    .unwrap();
}

/// A taken callback port is refused before anything runs, with the device
/// sign-in as the way out.
#[test]
fn a_taken_callback_port_is_refused_with_the_device_sign_in_as_the_way_out() {
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let error = codex_container::check_callback_port(port).unwrap_err();
    assert!(error.contains("--device-auth"), "{error}");
    drop(taken);
    codex_container::check_callback_port(port).unwrap();
}

/// The `connect codex` filters hide an OpenAI key and a whole JWT at the
/// start of a word, as `connect claude-code`'s hides an Anthropic token, and
/// leave the same letters inside a word (a URL's parameters) alone.
#[test]
fn the_codex_filters_hide_keys_and_jwts_and_leave_words_alone() {
    let key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    let jwt = fake_jwt();
    let anthropic = fresh_token();
    let text = format!(
        "key: {key}\r\ntoken {jwt}.\r\n{anthropic}\r\nhttps://x.test/?a=Qsk-abc&b=xeyJdef ok\r\n"
    );
    let mut chain = redact::FilterChain::codex(100, 30);
    let mut shown = Vec::new();
    for chunk in text.as_bytes().chunks(7) {
        chain.push(chunk, &mut shown);
    }
    chain.finish(&mut shown);
    for secret in [&key, &jwt, &anthropic] {
        let leaks = leaked_positions(&shown, secret);
        assert!(leaks.is_empty(), "{secret}: {leaks:?}");
    }
    let shown = String::from_utf8_lossy(&shown).into_owned();
    assert!(shown.contains("a=Qsk-abc&b=xeyJdef ok"), "{shown}");
    assert!(shown.contains("key: [token hidden"), "{shown}");
    assert!(shown.contains("token [token hidden"), "{shown}");
    // The Claude Code chain is the one filter it always was.
    let mut claude = redact::FilterChain::claude(100, 30);
    let mut shown = Vec::new();
    claude.push(format!("{key} {anthropic}\r\n").as_bytes(), &mut shown);
    claude.finish(&mut shown);
    let shown = String::from_utf8_lossy(&shown).into_owned();
    assert!(shown.contains(&key), "{shown}");
    assert!(leaked_positions(shown.as_bytes(), &anthropic).is_empty());
}

// ------------------------- connect codex: the sign-in page and the lifetime

const FAKE_PODMAN: &str = include_str!("../../tests/fixtures/fake-podman.sh");
/// The browser sign-in's address the fake `podman` prints (after
/// look-alikes, and before another address of the same form).
const SIGN_IN_ADDRESS: &str = "https://auth.openai.com/oauth/authorize?response_type=code&\
client_id=app_fake&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&\
code_challenge=AbCdEfGhIjKlMnOp-qr_stUVwxyz0123456789ABCDE&code_challenge_method=S256&\
state=Qsk-pLkJhGfDsAeyJ0x&scope=openid+profile+email+offline_access&originator=codex_cli_rs";
const DEVICE_PAGE: &str = "https://auth.openai.com/codex/device";

/// Write an executable script, and run it with `probe` (which it answers
/// at once, without doing anything) until it is not "text file busy",
/// which it is while another test's fork still holds a file it was writing.
fn install_script(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..100 {
        match std::process::Command::new(path).arg("probe").status() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => {
                assert!(other.unwrap().success());
                return;
            }
        }
    }
    panic!("{} stayed busy", path.display());
}

/// A fake opener (`open` or `xdg-open`, as this computer's would be named)
/// alone in a directory: it appends how many arguments it got and each one
/// to `opened` beside it, and exits with `status`. Its directory, to use as
/// `PATH`, and the `opened` file.
fn fake_opener(root: &Path, status: u8) -> (PathBuf, PathBuf) {
    let directory = root.join("opener-bin");
    std::fs::create_dir_all(&directory).unwrap();
    let opened = root.join("opened");
    install_script(
        &directory.join(browser::Opener::name()),
        &format!(
            "#!/bin/sh\n[ \"$*\" = probe ] && exit 0\n{{ printf 'argc=%s\\n' \"$#\"; for a in \
             \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; }} >> '{}'\nexit {status}\n",
            opened.display()
        ),
    );
    (directory, opened)
}

/// What the fake opener was run with, one `argc=`/`arg=` line each.
fn opened(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap_or_default()
}

/// Wait up to five seconds for the opener's answer.
fn opener_answer(receiver: std::sync::mpsc::Receiver<bool>) -> bool {
    receiver.recv_timeout(Duration::from_secs(5)).unwrap()
}

/// Only the exact page is accepted: `https://auth.openai.com` (no other
/// host, look-alike, scheme, port, user part or case), the authorize path
/// with a form-encoded query, or exactly the device page.
#[test]
fn only_the_exact_sign_in_page_is_accepted() {
    use browser::SignInPage::{Authorize, Device};
    assert_eq!(browser::SignInPage::for_sign_in(false), Authorize);
    assert_eq!(browser::SignInPage::for_sign_in(true), Device);
    assert!(Authorize.accepts(SIGN_IN_ADDRESS));
    assert!(Authorize.accepts("https://auth.openai.com/oauth/authorize?state=a%2Fb&x=1+2*3~"));
    assert!(Device.accepts(DEVICE_PAGE));
    for refused in [
        "http://auth.openai.com/oauth/authorize?state=a",
        "HTTPS://auth.openai.com/oauth/authorize?state=a",
        "https://AUTH.OPENAI.COM/oauth/authorize?state=a",
        "https://auth.openai.com.evil.test/oauth/authorize?state=a",
        "https://auth.openai.com@evil.test/oauth/authorize?state=a",
        "https://auth.openai.com:443/oauth/authorize?state=a",
        "https://auth.openai.co/oauth/authorize?state=a",
        "https://evil.test/https://auth.openai.com/oauth/authorize?state=a",
        "https://xauth.openai.com/oauth/authorize?state=a",
        "https://auth.openai.com/oauth/authorize",
        "https://auth.openai.com/oauth/authorize?",
        "https://auth.openai.com/oauth/authorizex?state=a",
        "https://auth.openai.com/oauth/authorize/?state=a",
        "https://auth.openai.com/oauth/authorize/../../evil?state=a",
        "https://auth.openai.com//oauth/authorize?state=a",
        "https://auth.openai.com/oauth/authorize?state=a#fragment",
        "https://auth.openai.com/oauth/authorize?state=a%2",
        "https://auth.openai.com/oauth/authorize?state=a%zz",
        "https://auth.openai.com/oauth/authorize?state=a;b",
        "https://auth.openai.com/oauth/authorize?state=$(touch x)",
        "https://auth.openai.com/oauth/authorize?next=https://evil.test/",
        "https://auth.openai.com/codex/device",
        " https://auth.openai.com/oauth/authorize?state=a",
    ] {
        assert!(!Authorize.accepts(refused), "{refused}");
    }
    for refused in [
        "http://auth.openai.com/codex/device",
        "https://auth.openai.com.evil.test/codex/device",
        "https://auth.openai.com/codex/devices",
        "https://auth.openai.com/codex/device/",
        "https://auth.openai.com/codex/device?user_code=X",
        "https://auth.openai.com/codex/device#x",
        "https://auth.openai.com/codex/device.",
        "https://auth.openai.com/oauth/authorize?state=a",
    ] {
        assert!(!Device.accepts(refused), "{refused}");
    }
    let long = format!(
        "https://auth.openai.com/oauth/authorize?state={}",
        "a".repeat(5000)
    );
    assert!(!Authorize.accepts(&long));
}

/// The watcher finds the page as it is shown, however the output is cut
/// into writes: color changes do not split it, a hyperlink's hidden target
/// is not read, look-alikes and other words are passed over, and the first
/// address is returned once, never another one after it.
#[test]
fn the_watcher_finds_the_shown_address_once() {
    use browser::{LinkWatcher, SignInPage};
    let browser_screen = format!(
        "Starting local login server on http://localhost:1455.\r\n\
         http://auth.openai.com/oauth/authorize?state=plain-http\r\n\
         https://auth.openai.com.evil.test/oauth/authorize?state=look-alike\r\n\
         \x1b]8;;https://auth.openai.com/oauth/authorize?state=hidden-target\x1b\\https://evil.test/\x1b]8;;\x1b\\\r\n\
         see:https://auth.openai.com/oauth/authorize?state=glued\r\n\
         https://auth.openai.com/oauth/authorize?state=moved\x1b[3Cx\r\n\
         \x1b[1m{}\x1b[0m\r\n\r\nhttps://auth.openai.com/oauth/authorize?state=printed-again\r\n",
        SIGN_IN_ADDRESS.replacen("state=", "st\x1b[32mate=", 1)
    );
    for chunk_size in [1, 2, 7, 64, browser_screen.len()] {
        let mut watcher = LinkWatcher::new(SignInPage::Authorize);
        let found: Vec<String> = browser_screen
            .as_bytes()
            .chunks(chunk_size)
            .filter_map(|chunk| watcher.push(chunk))
            .collect();
        assert_eq!(found, [SIGN_IN_ADDRESS], "chunks of {chunk_size}");
    }
    // A cursor movement or an erase right after an address drops it (the
    // program may have cut it there), and neither part is taken; one
    // before an address leaves it whole.
    let mut watcher = LinkWatcher::new(SignInPage::Authorize);
    assert_eq!(
        watcher.push(b"https://auth.openai.com/oauth/authorize?state=a\x1b[2Cb\r\n"),
        None
    );
    assert_eq!(
        watcher.push(b"https://auth.openai.com/oauth/authorize?state=b\x1b[K\r\n"),
        None
    );
    assert_eq!(
        watcher.push(b"x\x1b[2C\x1b[2Khttps://auth.openai.com/oauth/authorize?state=c\r\n"),
        Some("https://auth.openai.com/oauth/authorize?state=c".to_string())
    );
    // The device page, in Codex's colors; the authorize address is not it.
    let device_screen = format!(
        "   https://auth.openai.com/codex/devices\r\n   {SIGN_IN_ADDRESS}\r\n\
         1. Open this link\r\n   \x1b[94m{DEVICE_PAGE}\x1b[0m\r\n   \x1b[94m{DEVICE_PAGE}\x1b[0m\r\n"
    );
    let mut watcher = LinkWatcher::new(SignInPage::Device);
    let found: Vec<String> = device_screen
        .as_bytes()
        .chunks(3)
        .filter_map(|chunk| watcher.push(chunk))
        .collect();
    assert_eq!(found, [DEVICE_PAGE]);
    // An address that never ends (no character after it) is not taken,
    // and one too long to be Codex's is not either.
    let mut watcher = LinkWatcher::new(SignInPage::Device);
    assert_eq!(watcher.push(DEVICE_PAGE.as_bytes()), None);
    let mut watcher = LinkWatcher::new(SignInPage::Authorize);
    let long = format!(
        "https://auth.openai.com/oauth/authorize?state={}\r\n",
        "a".repeat(5000)
    );
    assert_eq!(watcher.push(long.as_bytes()), None);
}

/// The opener is the first `open` (macOS) or `xdg-open` on PATH, run with
/// the address as its one argument and no shell (`&`, `$(…)`, `;` and
/// spaces reach it as they are, and nothing runs); an opener that fails
/// says so, and with none on PATH there is no opener.
#[test]
fn the_opener_gets_the_address_as_its_one_argument_without_a_shell() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let (bin, log) = fake_opener(root, 0);
    let opener = browser::Opener::find(Some(bin.as_os_str())).expect("the fake opener");
    let marker = root.join("ran-a-shell");
    let address = format!(
        "https://auth.openai.com/oauth/authorize?a=1&b=$(touch {})&c=2;d e",
        marker.display()
    );
    assert!(opener_answer(opener.open(&address)));
    assert_eq!(opened(&log), format!("argc=1\narg={address}\n"));
    assert!(!marker.exists());

    let failing = tempfile::tempdir().unwrap();
    let (bin, log) = fake_opener(failing.path(), 3);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    assert!(!opener_answer(opener.open(DEVICE_PAGE)));
    assert_eq!(opened(&log), format!("argc=1\narg={DEVICE_PAGE}\n"));

    let empty = tempfile::tempdir().unwrap();
    assert!(browser::Opener::find(Some(empty.path().as_os_str())).is_none());
    assert!(browser::Opener::find(None).is_none());
}

/// A private directory for one login-container test: `bin/podman` (the
/// fake, with its `mode`, the `auth` it prints and the `calls` it records)
/// and `data/` (the data root).
struct ContainerFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    access: String,
    /// What `connect codex` is asked to store.
    keep: codex::Keep,
}

impl ContainerFixture {
    fn new(mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("mode"), mode).unwrap();
        let access = fake_jwt();
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": fake_jwt(),
                "access_token": access,
                "refresh_token": format!("rt_{}", uuid::Uuid::new_v4().simple()),
                "account_id": "acct-fixture",
            },
            "last_refresh": "2026-10-11T12:00:00Z",
        });
        std::fs::write(bin.join("auth"), auth.to_string()).unwrap();
        install_script(&bin.join("podman"), FAKE_PODMAN);
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _directory: directory,
            root,
            access,
            keep: codex::Keep::SignIn,
        }
    }

    /// The fake prints an `auth.json` with `key` as its `OPENAI_API_KEY`,
    /// and `connect codex` is asked to keep `keep`.
    fn with_api_key(mut self, key: &str, keep: codex::Keep) -> Self {
        let path = self.root.join("bin/auth");
        let mut auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        auth["OPENAI_API_KEY"] = serde_json::Value::String(key.to_string());
        std::fs::write(&path, auth.to_string()).unwrap();
        self.keep = keep;
        self
    }

    fn login(&self, opener: Option<browser::Opener>, lifetime: Duration) -> codex_container::Login {
        codex_container::Login::new(
            self.root.join("bin/podman"),
            "0123456789abcdef".to_string(),
            lifetime,
            opener,
        )
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.root.join("bin/calls")).unwrap_or_default()
    }

    fn stored(&self) -> Option<Vec<u8>> {
        std::fs::read(secret_store::secret_path(
            &self.root.join("data"),
            axocoatl_daemon::external_agent::codex_chatgpt::CODEX_CHATGPT_SECRET,
        ))
        .ok()
    }
}

/// Run `connect codex` with the login container (the fake `podman`) on a
/// fresh user terminal, typing Enter once `ready` is shown; its result and
/// what it showed.
async fn run_container_login(
    fixture: &ContainerFixture,
    login: codex_container::Login,
    device_auth: bool,
    ready: &'static str,
) -> (Result<String, Failure>, Screen) {
    let terminal = user_terminal();
    terminal.set_size(SIZE);
    let before = modes(&terminal.slave);
    let screen = Screen::default();
    let interrupt = Interrupt::default();
    let typing = typist(&terminal, &screen, ready, None);
    let result = finish_within(
        codex_container::connect(
            login,
            fixture.root.join("data"),
            device_auth,
            fixture.keep,
            terminal.slave.try_clone().unwrap(),
            Box::new(screen.clone()),
            &interrupt,
            &Resized::default(),
        ),
        &interrupt,
    )
    .await;
    assert!(
        typing.join().unwrap(),
        "the terminal was raw while codex ran"
    );
    assert_eq!(modes(&terminal.slave), before);
    (result, screen)
}

/// What the user's terminal shows once `connect codex` has shown `screen`
/// (below the intro's blank line) and printed its outcome as
/// `cmd_connect_codex` prints it: after the line breaks [`relay::Lines`]
/// asks for, each `\n` written as the terminal, no longer raw, writes it.
fn with_outcome(screen: &Screen, result: &Result<String, Failure>) -> String {
    let lines = relay::Lines::new(SIZE, true);
    let mut shown = screen.bytes();
    lines.feed(&shown);
    let (mark, message) = match result {
        Ok(message) => ('✓', message.as_str()),
        Err(failure) => ('✗', failure.message.as_str()),
    };
    let outcome = format!("{}{mark} {message}\n", lines.before_message());
    shown.extend_from_slice(outcome.replace('\n', "\r\n").as_bytes());
    String::from_utf8_lossy(&shown).into_owned()
}

/// With the pinned Codex in the login container, the sign-in page it
/// prints is opened on this computer once, with exactly that address (not
/// a look-alike before it, nor the address printed after it), and the
/// terminal says so; the sign-in is then stored and the container removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_container_sign_in_page_is_opened_once_on_this_computer() {
    let fixture = ContainerFixture::new("ok");
    let (bin, log) = fake_opener(&fixture.root, 0);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    let login = fixture.login(Some(opener), codex_container::DEFAULT_LIFETIME);
    let (result, screen) = run_container_login(&fixture, login, false, browser::OPENED).await;
    let message = result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(
        message.contains("the login container was removed"),
        "{message}"
    );
    assert_eq!(opened(&log), format!("argc=1\narg={SIGN_IN_ADDRESS}\n"));
    let shown = String::from_utf8_lossy(&screen.bytes()).into_owned();
    assert_eq!(shown.matches(browser::OPENED).count(), 1, "{shown}");
    // The note is a line of its own, after the address.
    assert!(
        shown.contains(&format!("\r\n{}\r\n", browser::OPENED)),
        "{shown}"
    );
    let flat: String = shown.split(['\r', '\n']).collect();
    assert!(flat.contains(SIGN_IN_ADDRESS), "{shown}");
    assert_not_shown(&screen, &fixture.access);
    assert!(fixture.stored().is_some());
    let calls = fixture.calls();
    assert!(calls.contains("--timeout=1800\n"), "{calls}");
    assert!(calls.contains("exec --interactive\n"), "{calls}");
    assert!(calls.trim_end().ends_with("rm --force"), "{calls}");
    // The address is in no file but the fake `podman` that prints it and
    // the fake opener's record of it (none in the data root).
    let mut hits = Vec::new();
    files_containing(
        &fixture.root,
        b"state=Qsk-pLkJhGfDsAeyJ0x",
        6,
        None,
        &[],
        &mut hits,
    );
    hits.sort();
    let mut expected = vec![fixture.root.join("bin/podman"), log];
    expected.sort();
    assert_eq!(hits, expected);
}

/// `--api-key` with the login container: the key in the `auth.json` read
/// from the container is stored exactly as `codex-openai` beside the
/// sign-in, never shown, and the container is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_container_sign_in_stores_the_api_key_with_api_key() {
    let key = fake_api_key();
    let fixture = ContainerFixture::new("ok").with_api_key(&key, codex::Keep::ApiKeyAndSignIn);
    let login = fixture.login(None, codex_container::DEFAULT_LIFETIME);
    let (result, screen) = run_container_login(&fixture, login, false, CODEX_PROMPT).await;
    let message = result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert!(
        message.contains("as secret codex-openai and your ChatGPT sign-in as secret"),
        "{message}"
    );
    assert!(
        message.contains("the login container was removed"),
        "{message}"
    );
    let data = fixture.root.join("data");
    assert_eq!(
        stored_secret(&data, "codex-openai").unwrap(),
        key.as_bytes()
    );
    assert!(fixture.stored().is_some());
    assert_not_shown(&screen, &key);
    assert_not_shown(&screen, &fixture.access);
    let mut hits = Vec::new();
    files_containing(&data, key.as_bytes(), 6, None, &[], &mut hits);
    assert_eq!(hits, [secret_store::secret_path(&data, "codex-openai")]);
    assert!(fixture.calls().trim_end().ends_with("rm --force"));
}

/// With `--device-auth` the device page is the one opened, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_device_page_is_opened_once_with_device_auth() {
    let fixture = ContainerFixture::new("ok");
    let (bin, log) = fake_opener(&fixture.root, 0);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    let login = fixture.login(Some(opener), codex_container::DEFAULT_LIFETIME);
    let (result, screen) = run_container_login(&fixture, login, true, browser::OPENED).await;
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert_eq!(opened(&log), format!("argc=1\narg={DEVICE_PAGE}\n"));
    assert!(screen.contains("WXYZ-1234"));
    assert!(fixture.stored().is_some());
}

/// `--no-browser` (no opener given to the login) opens nothing and says
/// nothing about a browser; the address stays on screen to open by hand.
/// An opener that fails is not mentioned either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_browser_or_a_failing_opener_leaves_the_printed_address() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: ConnectCommands,
    }
    let parsed = Cli::try_parse_from(["x", "codex", "--no-browser"]).unwrap();
    assert!(matches!(
        parsed.command,
        ConnectCommands::Codex {
            no_browser: true,
            ..
        }
    ));
    assert!(Cli::try_parse_from(["x", "claude-code", "--no-browser"]).is_err());

    let fixture = ContainerFixture::new("ok");
    let (_, log) = fake_opener(&fixture.root, 0);
    let login = fixture.login(None, codex_container::DEFAULT_LIFETIME);
    let (result, screen) = run_container_login(&fixture, login, false, CODEX_PROMPT).await;
    let terminal = with_outcome(&screen, &result);
    let message = result.unwrap_or_else(|failure| panic!("{}", failure.message));
    assert_eq!(opened(&log), "");
    assert!(!screen.contains(browser::OPENED));
    // Exactly one blank line between Codex's last line and the outcome.
    assert!(
        terminal.ends_with(&format!(
            "{CODEX_PROMPT}\r\n\r\nSuccessfully logged in\r\n\r\n✓ {}\r\n",
            message.replace('\n', "\r\n")
        )),
        "{terminal:?}"
    );
    let flat: String = String::from_utf8_lossy(&screen.bytes())
        .split(['\r', '\n'])
        .collect();
    assert!(flat.contains(SIGN_IN_ADDRESS));

    let fixture = ContainerFixture::new("ok");
    let (bin, log) = fake_opener(&fixture.root, 1);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    let login = fixture.login(Some(opener), codex_container::DEFAULT_LIFETIME);
    let (result, screen) = run_container_login(&fixture, login, false, CODEX_PROMPT).await;
    result.unwrap_or_else(|failure| panic!("{}", failure.message));
    // Wait out the opener's answer, then the relay has nothing to say.
    let deadline = Instant::now() + Duration::from_secs(5);
    while opened(&log).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(opened(&log), format!("argc=1\narg={SIGN_IN_ADDRESS}\n"));
    assert!(!screen.contains(browser::OPENED));
}

/// When the login container's lifetime ends the sign-in (Podman stops it
/// under `podman exec`, and the read finds no container), the message says
/// the sign-in was not completed in time, with exit code 2, and Podman's
/// own error about the exec session it lost is not shown; a container
/// that ended before its lifetime, or a read that failed while it still
/// runs, says so instead. Nothing is stored either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_the_container_outlived_says_it_was_not_completed_in_time() {
    assert_eq!(
        codex_container::timed_out(codex_container::DEFAULT_LIFETIME),
        "The Codex sign-in was not completed within 30 minutes; nothing was stored. Run \
         `axocoatl connect codex` again."
    );
    assert_eq!(
        codex_container::timed_out(Duration::from_secs(1)),
        "The Codex sign-in was not completed within 1 second; nothing was stored. Run \
         `axocoatl connect codex` again."
    );

    let fixture = ContainerFixture::new("timeout");
    let login = fixture.login(None, Duration::from_secs(1));
    let (result, screen) = run_container_login(&fixture, login, false, CODEX_PROMPT).await;
    let failure = result.expect_err("nothing is stored");
    assert_eq!(failure.code, exit::NO_TOKEN, "{}", failure.message);
    assert_eq!(
        failure.message,
        codex_container::timed_out(Duration::from_secs(1))
    );
    assert!(screen.contains(CODEX_PROMPT));
    assert!(!screen.contains("no exec session"));
    assert!(fixture.stored().is_none());
    let calls = fixture.calls();
    assert!(calls.contains("--timeout=1\n"), "{calls}");
    assert!(calls.contains("container exists\n"), "{calls}");
    assert!(calls.trim_end().ends_with("rm --force"), "{calls}");

    let fixture = ContainerFixture::new("timeout");
    let login = fixture.login(None, codex_container::DEFAULT_LIFETIME);
    let (result, screen) = run_container_login(&fixture, login, false, CODEX_PROMPT).await;
    let failure = result.expect_err("nothing is stored");
    assert_eq!(failure.code, exit::FAILURE);
    assert_eq!(failure.message, codex_container::ended_early());
    // Not the lifetime: Podman's error is shown.
    assert!(screen.contains("Error: no exec session with ID 0123abcd found\r\n"));
    assert!(fixture.stored().is_none());

    let fixture = ContainerFixture::new("readfail");
    let login = fixture.login(None, Duration::from_secs(1));
    let (result, _) = run_container_login(&fixture, login, false, CODEX_PROMPT).await;
    let failure = result.expect_err("nothing is stored");
    assert_eq!(failure.code, exit::FAILURE);
    assert!(
        failure
            .message
            .starts_with("Codex's auth.json could not be read from the login container"),
        "{}",
        failure.message
    );
    assert!(fixture.stored().is_none());

    assert_eq!(
        codex_container::exists_args("n"),
        ["container", "exists", "n"]
    );
    assert_eq!(
        codex_container::running_args("n"),
        [
            "container",
            "inspect",
            "--format",
            "{{.State.Running}}",
            "n"
        ]
    );
    // Podman reads `--timeout=0` as no timeout: never passed.
    let args = codex_container::run_args("n", "i", true, Duration::ZERO);
    assert!(args.iter().any(|arg| arg == "--timeout=1"), "{args:?}");
}

/// Podman's two errors about an exec session the container's lifetime
/// ended under it are not shown, after Codex's whole output; the same
/// errors are shown when the container ended before its lifetime, and
/// another Podman error is shown when the lifetime ended the container.
/// Either way exactly one blank line separates the last line shown from
/// the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_podmans_lost_exec_session_error_after_the_lifetime_is_not_shown() {
    const NO_SESSION: &str = "Error: no exec session with ID 0123abcd found\r\n";
    const NOT_RETRIEVED: &str = "Error: retrieving exec session 0123abcd from container \
        4567ef: no container with ID 4567ef found in database: no such container\r\n";
    const OTHER: &str =
        "Error: read unixpacket @->/proc/self/fd/7/attach: read: connection reset by peer\r\n";
    let second = Duration::from_secs(1);
    let fixtures = [
        ("timeout", second),
        ("timeout-retrieving", second),
        ("timeout-other", second),
        ("timeout-retrieving", codex_container::DEFAULT_LIFETIME),
    ]
    .map(|(mode, lifetime)| (ContainerFixture::new(mode), lifetime));
    let run = |index: usize| {
        let (fixture, lifetime) = &fixtures[index];
        run_container_login(fixture, fixture.login(None, *lifetime), false, CODEX_PROMPT)
    };
    let (no_session, not_retrieved, other, early) = tokio::join!(run(0), run(1), run(2), run(3));
    for (name, (result, screen)) in [("no session", no_session), ("not retrieved", not_retrieved)] {
        let terminal = with_outcome(&screen, &result);
        let failure = result.expect_err(name);
        assert_eq!(failure.code, exit::NO_TOKEN, "{name}");
        assert_eq!(
            failure.message,
            codex_container::timed_out(second),
            "{name}"
        );
        let shown = String::from_utf8_lossy(&screen.bytes()).into_owned();
        assert!(!shown.contains("Error:"), "{name}: {shown}");
        assert!(!shown.contains("exec session"), "{name}: {shown}");
        // Codex's output is all there.
        assert!(
            shown.contains(&format!("{CODEX_PROMPT}\r\n")),
            "{name}: {shown}"
        );
        let flat: String = shown.split(['\r', '\n']).collect();
        assert!(flat.contains(SIGN_IN_ADDRESS), "{name}: {shown}");
        // Codex's last line, the Enter typed after it (a blank line), and
        // the message: not another blank line for the line dropped.
        assert!(
            terminal.ends_with(&format!("{CODEX_PROMPT}\r\n\r\n✗ {}\r\n", failure.message)),
            "{name}: {terminal:?}"
        );
    }
    let (result, screen) = other;
    let terminal = with_outcome(&screen, &result);
    let failure = result.expect_err("other");
    assert_eq!(failure.message, codex_container::timed_out(second));
    assert!(screen.contains(OTHER));
    assert!(
        terminal.ends_with(&format!(
            "{CODEX_PROMPT}\r\n\r\n{OTHER}\r\n✗ {}\r\n",
            failure.message
        )),
        "{terminal:?}"
    );
    let (result, screen) = early;
    let terminal = with_outcome(&screen, &result);
    let failure = result.expect_err("early");
    assert_eq!(failure.message, codex_container::ended_early());
    assert!(screen.contains(NOT_RETRIEVED));
    assert!(
        terminal.ends_with(&format!(
            "{CODEX_PROMPT}\r\n\r\n{NOT_RETRIEVED}\r\n✗ {}\r\n",
            failure.message
        )),
        "{terminal:?}"
    );
    for (fixture, _) in &fixtures {
        assert!(fixture.stored().is_none());
    }

    for lost in [NO_SESSION, NOT_RETRIEVED] {
        assert!(
            codex_container::lost_exec_session(lost.as_bytes()),
            "{lost}"
        );
        assert!(codex_container::lost_exec_session(
            lost.trim_end().as_bytes()
        ));
        assert!(codex_container::lost_exec_session(
            lost.replace("\r\n", "\n").as_bytes()
        ));
    }
    for lost in [
        "Error: no exec session with ID 0123 found: no such exec session\n",
        "Error: no exec session with ID 0123 found in container 4567: no such exec session\n",
        "Error: retrieving exec session 0123 from container 4567: no exec session with ID \
         0123 found in container 4567: no such exec session\n",
    ] {
        assert!(
            codex_container::lost_exec_session(lost.as_bytes()),
            "{lost}"
        );
    }
    for kept in [
        OTHER,
        "",
        "\r\n",
        "Error: no such container\n",
        "Error: no container with name or ID \"x\" found: no such container\n",
        "Error: retrieving exec session 0123 from container 4567: connection refused\n",
        "no exec session with ID 0123 found\n",
        "Error logging in: no exec session with ID 0123 found\n",
        " Error: no exec session with ID 0123 found\n",
        "\x1b[31mError: no exec session with ID 0123 found\x1b[0m\n",
        "Error: no exec session with ID 0123 found\r\nmore\r\n",
        "Error: no exec session with ID 0123 found, retrying\n",
    ] {
        assert!(
            !codex_container::lost_exec_session(kept.as_bytes()),
            "{kept:?}"
        );
    }
    assert!(!codex_container::lost_exec_session(
        b"Error: no exec session with ID \xff found"
    ));
}

/// Where the relay starts holding back a last line that may be Podman's
/// error: only plain text from a line's start that starts with `Error: `
/// (or may still, while no line break ends it), followed by line breaks
/// alone.
#[test]
fn only_a_last_plain_line_starting_like_podmans_error_is_held() {
    use relay::held_from;
    let long = format!("x\r\nError: {}\r\n", "a".repeat(5000));
    for (bytes, line_start, held) in [
        (&b"Press Enter\r\nError: gone\r\n"[..], false, Some(13)),
        (b"Press Enter\nError: gone\n", false, Some(12)),
        (b"Press Enter\rError: gone", false, Some(12)),
        (b"Press Enter\r\nError: gone\r\n\r\n", false, Some(13)),
        (b"Error: gone\r\n", true, Some(0)),
        (b"Error: gone\r\n", false, None),
        (b"x\r\nErr", false, Some(3)),
        (b"x\r\nError:", false, Some(3)),
        (b"E", true, Some(0)),
        (b"x\r\nErr\r\n", false, None),
        (b"x\r\nError\r\n", false, None),
        (b"x\r\nEnter the code", false, None),
        (b"x\r\nError: gone\r\nmore", false, None),
        (b"x\r\nError: gone\r\n\x1b[?25h", false, None),
        (b"x\r\n\x1b[31mError: gone\x1b[0m\r\n", false, None),
        (b"x\r\nError:\tgone\r\n", false, None),
        (b"x\r\n  Error: gone\r\n", false, None),
        (b"x Error: gone\r\n", false, None),
        (b"", true, None),
        (b"\r\n", true, None),
        (long.as_bytes(), false, None),
    ] {
        assert_eq!(
            held_from(bytes, line_start),
            held,
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

/// The relay holds a last line that may be Podman's error back only when
/// asked to, and keeps it for the caller only when the program ends with
/// an error: it is shown when the program exits 0, when more output follows
/// it, and after a while when the program runs on.
#[test]
fn a_held_line_is_kept_only_when_the_program_ends_with_an_error() {
    fn start(script: &str, hold: bool) -> (std::thread::JoinHandle<relay::SetupRun>, Screen) {
        let screen = Screen::default();
        let mut output = screen.clone();
        let script = script.to_string();
        let running = std::thread::spawn(move || {
            let terminal = user_terminal();
            terminal.set_size(SIZE);
            let args = ["-c", script.as_str()];
            relay::run_program(
                &relay::Program {
                    path: Path::new("/bin/sh"),
                    args: &args,
                    cwd: None,
                    set_env: &[],
                    remove_env: &[],
                    filters: redact::FilterChain::codex,
                    open_sign_in: None,
                    hold_error_line: hold,
                },
                terminal.slave.as_fd(),
                &mut output,
                &std::sync::atomic::AtomicBool::new(false),
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap()
        });
        (running, screen)
    }
    let last = "printf 'Codex said this\\nError: last words\\n'; exit";
    let (running, screen) = start(&format!("{last} 3"), true);
    let run = running.join().unwrap();
    assert_eq!(run.exit_code, Some(3));
    assert_eq!(run.held_line, b"Error: last words\r\n");
    assert!(screen.contains("Codex said this\r\n"));
    assert!(!screen.contains("last words"));

    for (script, hold) in [
        (format!("{last} 0"), true),
        (format!("{last} 3"), false),
        (
            "printf 'Error: first words\\nlast words\\n'; exit 3".to_string(),
            true,
        ),
    ] {
        let (running, screen) = start(&script, hold);
        let run = running.join().unwrap();
        assert!(run.held_line.is_empty(), "{script}");
        assert!(screen.contains("Error: "), "{script}");
        assert!(screen.contains("last words\r\n"), "{script}");
    }

    // A program that runs on after it: shown while it runs.
    let (running, screen) = start("printf 'Error: and then\\n'; sleep 5; exit 3", true);
    let deadline = Instant::now() + Duration::from_secs(4);
    while !screen.contains("Error: and then\r\n") {
        assert!(Instant::now() < deadline, "not shown while it ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!running.is_finished());
    let run = running.join().unwrap();
    assert!(run.held_line.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&screen.bytes())
            .matches("and then")
            .count(),
        1
    );
}

/// The outcome after the relayed output (as `cmd_connect_codex` prints
/// it, after the line breaks [`relay::Lines`] asks for) is set off from the
/// last line shown by exactly one blank line however the output ended:
/// without a line break, with one, with a blank line, with a held line
/// that is dropped or shown, with escapes, with a mode the relay resets, or
/// with nothing at all (below the intro's blank line).
#[test]
fn the_outcome_follows_the_last_line_shown_after_exactly_one_blank_line() {
    fn terminal(script: &str, show_held: bool, outcome: &str) -> String {
        let terminal = user_terminal();
        terminal.set_size(SIZE);
        let screen = Screen::default();
        let lines = relay::Lines::new(SIZE, true);
        let mut output = lines.follow(screen.clone());
        let args = ["-c", script];
        let run = relay::run_program(
            &relay::Program {
                path: Path::new("/bin/sh"),
                args: &args,
                cwd: None,
                set_env: &[],
                remove_env: &[],
                filters: redact::FilterChain::codex,
                open_sign_in: None,
                hold_error_line: true,
            },
            terminal.slave.as_fd(),
            &mut output,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        if show_held {
            output.write_all(&run.held_line).unwrap();
        }
        let mut shown = screen.bytes();
        let outcome = format!("{}{outcome}\n", lines.before_message());
        shown.extend_from_slice(outcome.replace('\n', "\r\n").as_bytes());
        String::from_utf8(shown).unwrap()
    }
    let said = "Codex said this";
    for (script, show_held, outcome, expected) in [
        (
            "printf 'Codex said this'; exit 3",
            false,
            "✗ x",
            format!("{said}\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\n'; exit 3",
            false,
            "✗ x",
            format!("{said}\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\n\\n'; exit 3",
            false,
            "✗ x",
            format!("{said}\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\r'; exit 3",
            false,
            "✗ x",
            format!("{said}\r\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\nError: gone\\n'; exit 3",
            false,
            "✗ x",
            format!("{said}\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\n\\nError: gone\\n'; exit 3",
            false,
            "✗ x",
            format!("{said}\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\nError: gone\\n'; exit 3",
            true,
            "✗ x",
            format!("{said}\r\nError: gone\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this\\n\\nError: gone\\n'; exit 3",
            true,
            "✗ x",
            format!("{said}\r\n\r\nError: gone\r\n\r\n✗ x\r\n"),
        ),
        (
            "printf 'Codex said this'; exit 0",
            false,
            "✓ x",
            format!("{said}\r\n\r\n✓ x\r\n"),
        ),
        (
            "printf 'Codex said this\\n'; exit 0",
            false,
            "✓ x",
            format!("{said}\r\n\r\n✓ x\r\n"),
        ),
        (
            "printf '\\033[1mCodex said this\\033[0m\\n\\n'; exit 0",
            false,
            "✓ x",
            format!("\x1b[1m{said}\x1b[0m\r\n\r\n✓ x\r\n"),
        ),
        // Bold left on: the relay resets it, which shows nothing.
        (
            "printf '\\033[1mCodex said this\\n'; exit 3",
            false,
            "✗ x",
            format!("\x1b[1m{said}\r\n\x1b[0m\r\n✗ x\r\n"),
        ),
        ("exit 3", false, "✗ x", "✗ x\r\n".to_string()),
    ] {
        assert_eq!(terminal(script, show_held, outcome), expected, "{script}");
    }
}

/// The line breaks before the outcome, from what the terminal shows: the
/// cursor's line and the one above it, after line feeds without a carriage
/// return, lines of spaces, the alternate screen and scrolling.
#[test]
fn the_line_breaks_before_the_outcome_follow_the_cursor_and_the_line_above() {
    let many: String = (0..40).map(|line| format!("line {line}\r\n")).collect();
    for (shown, blank_above, before) in [
        ("", true, ""),
        ("", false, "\n"),
        ("text", true, "\n\n"),
        ("text\r", true, "\n\n"),
        ("text\r\n", true, "\n"),
        ("text\r\n\r\n", true, ""),
        ("text\r\n\r\n\r\n", true, ""),
        // Raw line feeds keep the column.
        ("text\n", true, "\n"),
        ("text\n\n", true, "\r"),
        ("text\r\n   \r\n", true, ""),
        ("text\r\n\x1b[31m\x1b[0m", true, "\n"),
        (
            "text\r\n\x1b[?1049h\x1b[2J\x1b[Hdrawn\r\n\x1b[?1049l",
            true,
            "\n",
        ),
        ("text\r\nmore\r\n\x1b[A", true, "\n\n"),
        (&many, true, "\n"),
    ] {
        let lines = relay::Lines::new(SIZE, blank_above);
        lines.feed(shown.as_bytes());
        assert_eq!(lines.before_message(), before, "{shown:?}");
    }
}

/// The relay's part: the page is opened once however often its address is
/// shown; once the opener has opened it, the note is one line (after a line
/// break when the cursor is not at the start of one), shown once; when the
/// opener fails, nothing is said.
#[test]
fn the_note_is_shown_once_and_only_when_the_page_opened() {
    fn answer(open_once: &mut browser::OpenOnce<'_>) -> Option<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while open_once.waiting() {
            if let Some(note) = open_once.note() {
                return Some(note);
            }
            assert!(Instant::now() < deadline, "the opener never answered");
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
    let directory = tempfile::tempdir().unwrap();
    let (bin, log) = fake_opener(directory.path(), 0);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    let mut open_once = browser::OpenOnce::new(browser::SignInPage::Device, &opener);
    assert!(open_once.note().is_none());
    open_once.watch(format!("   {DEVICE_PAGE}\r\n").as_bytes());
    assert_eq!(
        answer(&mut open_once),
        Some(format!("{}\r\n", browser::OPENED).into_bytes())
    );
    open_once.watch(format!("{DEVICE_PAGE}\r\nPress Enter").as_bytes());
    assert!(!open_once.waiting());
    assert!(open_once.note().is_none());
    assert_eq!(opened(&log), format!("argc=1\narg={DEVICE_PAGE}\n"));

    let directory = tempfile::tempdir().unwrap();
    let (bin, _) = fake_opener(directory.path(), 0);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    let mut open_once = browser::OpenOnce::new(browser::SignInPage::Device, &opener);
    open_once.watch(format!("{DEVICE_PAGE}\r\nPress Enter").as_bytes());
    assert_eq!(
        answer(&mut open_once),
        Some(format!("\r\n{}\r\n", browser::OPENED).into_bytes())
    );

    let directory = tempfile::tempdir().unwrap();
    let (bin, log) = fake_opener(directory.path(), 1);
    let opener = browser::Opener::find(Some(bin.as_os_str())).unwrap();
    let mut open_once = browser::OpenOnce::new(browser::SignInPage::Device, &opener);
    open_once.watch(format!("{DEVICE_PAGE}\r\n").as_bytes());
    assert!(open_once.waiting());
    assert_eq!(answer(&mut open_once), None);
    assert!(open_once.note().is_none());
    assert_eq!(opened(&log), format!("argc=1\narg={DEVICE_PAGE}\n"));
}
