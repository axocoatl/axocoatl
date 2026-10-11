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
        }
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
        device_auth, codex, ..
    } = parsed.command
    else {
        panic!("codex")
    };
    assert!(!device_auth);
    assert!(codex.is_none());
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
    } = parsed.command
    else {
        panic!("codex")
    };
    assert!(device_auth);
    assert_eq!(codex.as_deref(), Some(Path::new("/opt/codex")));
    assert_eq!(config, Path::new("/tmp/a.yaml"));
    for argv in [
        &["x", "codex", "eyJhbGciOi.value.sig"][..],
        &["x", "codex", "--secret", "other"],
        &["x", "codex", "--token", "value"],
    ] {
        assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
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
/// removed by Podman after at most [`codex_container::MAX_LIFETIME_SECS`];
/// for the browser sign-in exactly one port published, on this computer's
/// loopback only, and with `--device-auth` none.
#[test]
fn the_login_container_publishes_only_the_callback_on_loopback_and_mounts_nothing() {
    use codex_container::*;
    let image = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let name = container_name();
    assert!(name.starts_with(NAME_PREFIX));
    assert_ne!(name, container_name());
    let browser = run_args(&name, image, false);
    let device = run_args(&name, image, true);
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
/// browser on this computer, so it says to open the address it prints.
#[test]
fn the_container_intro_does_not_promise_a_browser_codex_cannot_open() {
    let container = codex::sign_in_how(false, true);
    assert_eq!(
        container,
        "Open the address it prints in a browser on this computer."
    );
    assert!(!container.contains("it opens"));
    assert!(codex::sign_in_how(false, false).contains("browser it opens"));
    for in_container in [false, true] {
        assert!(codex::sign_in_how(true, in_container).contains("Enter the code"));
    }
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
    let container = codex_container::Container::start(&podman, "image-id", true).unwrap();
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
