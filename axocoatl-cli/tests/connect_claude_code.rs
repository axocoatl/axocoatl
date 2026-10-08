//! `axocoatl connect claude-code` and `axocoatl secret set --from-env` as
//! the built binary runs them. `connect` runs in a pseudo-terminal (it needs
//! one) with the fake `claude` in `fixtures/fake-claude.sh` (which can replay
//! what Claude Code's renderer writes, from `src/connect_cmd/ink_model.rs`)
//! and `--no-verify`; verification against a fake HTTPS endpoint is covered
//! by the crate's unit tests. Nothing here reaches a network or a real
//! account.
#![cfg(unix)]

#[path = "../src/connect_cmd/ink_model.rs"]
mod ink_model;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const BIN: &str = env!("CARGO_BIN_EXE_axocoatl");

fn fresh_token() -> String {
    let part = || uuid::Uuid::new_v4().simple().to_string();
    format!("sk-ant-oat01-{}{}-{}AA", part(), part(), &part()[..26])
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

struct Root {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

fn root() -> Root {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(path.join("home")).unwrap();
    Root {
        _directory: directory,
        path,
    }
}

fn secret_file(root: &Path, name: &str) -> PathBuf {
    root.join("data").join("secrets").join(name)
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(root)
        .env("AXOCOATL_DATA_DIR", root.join("data"))
        .env("HOME", root.join("home"))
        .env_remove("RUST_LOG");
    command
}

/// The built binary running in a pseudo-terminal, and what it showed.
struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    screen: Arc<Mutex<Vec<u8>>>,
    reading: std::thread::JoinHandle<()>,
    deadline: Instant,
}

/// The size of the terminal `connect` runs in.
const SIZE: (u16, u16) = (100, 30);

/// For mode `replay`: what Claude Code writes in a terminal of `size`.
fn write_replay(root: &Path, (cols, rows): (u16, u16), before: &[u8], after: &[u8]) {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join(format!("before-{cols}x{rows}")), before).unwrap();
    std::fs::write(bin.join(format!("after-{cols}x{rows}")), after).unwrap();
}

impl Session {
    /// `axocoatl connect claude-code` with `args`, the fake `claude` (in
    /// mode `mode`, printing `token`) and the root's data directory.
    fn connect(root: &Path, mode: &str, token: &str, args: &[&str]) -> Self {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.sh");
        std::fs::copy(&fake, bin.join("claude")).unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(bin.join("mode"), mode).unwrap();
        std::fs::write(bin.join("token"), token).unwrap();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: SIZE.1,
                cols: SIZE.0,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut axocoatl = CommandBuilder::new(BIN);
        axocoatl.args(["connect", "claude-code", "--claude"]);
        axocoatl.arg(bin.join("claude"));
        axocoatl.arg("-c");
        axocoatl.arg(root.join("axocoatl.yaml"));
        axocoatl.args(args);
        axocoatl.cwd(root);
        axocoatl.env("AXOCOATL_DATA_DIR", root.join("data"));
        axocoatl.env("HOME", root.join("home"));
        axocoatl.env_remove("RUST_LOG");
        let child = pair.slave.spawn_command(axocoatl).unwrap();
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
        Session {
            child,
            writer,
            master: pair.master,
            screen,
            reading,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    fn shown(&self) -> String {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned()
    }

    /// Wait until the screen shows `text`.
    fn wait_for(&mut self, text: &str) {
        while !self.shown().contains(text) {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("axocoatl exited early ({status:?}):\n{}", self.shown());
            }
            assert!(
                Instant::now() < self.deadline,
                "never shown: {text}\n{}",
                self.shown()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the fake's prompt and press Enter.
    fn press_enter_at_the_prompt(&mut self) {
        self.press_enter_at("Press Enter to continue");
    }

    /// Wait for `prompt` and press Enter.
    fn press_enter_at(&mut self, prompt: &str) {
        while !self.shown().contains(prompt) {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("axocoatl exited early ({status:?}):\n{}", self.shown());
            }
            assert!(
                Instant::now() < self.deadline,
                "no prompt:\n{}",
                self.shown()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        self.writer.write_all(b"\r").unwrap();
        self.writer.flush().unwrap();
    }

    /// Wait for the exit; its code and everything shown.
    fn finish(mut self) -> (Option<i32>, Vec<u8>) {
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < self.deadline, "no exit:\n{}", self.shown());
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(self.writer);
        drop(self.master);
        let _ = self.reading.join();
        let code = Some(status.exit_code() as i32);
        let shown = self.screen.lock().unwrap().clone();
        (code, shown)
    }
}

#[test]
fn connect_runs_claude_in_a_terminal_and_stores_the_token_it_never_shows() {
    let root = root();
    let token = fresh_token();
    let mut session = Session::connect(&root.path, "colored", &token, &["--no-verify"]);
    session.press_enter_at_the_prompt();
    let (code, screen) = session.finish();
    let text = String::from_utf8_lossy(&screen).into_owned();
    assert_eq!(code, Some(0), "{text}");
    let leaks = leaked_positions(&screen, &token);
    assert!(leaks.is_empty(), "token bytes at {leaks:?} were shown");
    assert!(text.contains("[token hidden by axocoatl]"), "{text}");
    assert!(
        text.contains(
            "Connected Claude Code: stored secret claude-code-oauth (the token was never shown)"
        ),
        "{text}"
    );
    let path = secret_file(&root.path, "claude-code-oauth");
    assert_eq!(std::fs::read(&path).unwrap(), token.as_bytes());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // `secret list` names it and never shows it.
    let listed = command(&root.path)
        .args(["secret", "list", "-c"])
        .arg(root.path.join("axocoatl.yaml"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout),
        "claude-code-oauth\n"
    );
}

/// What Claude Code 2.1.271 writes for `claude setup-token`, replayed by the
/// fake: `claude` runs in a terminal as large as the user's, the token Ink
/// wraps into rows placed by cursor movement is stored exactly, and the
/// user sees the mask where it was.
#[test]
fn connect_stores_the_token_claude_code_draws_and_never_shows_it() {
    let root = root();
    let token = fresh_token();
    let (before, after) = ink_model::session(
        &token,
        usize::from(SIZE.0),
        usize::from(SIZE.1),
        ink_model::Shape::Claude,
    );
    write_replay(&root.path, SIZE, &before, &after);
    let mut session = Session::connect(&root.path, "replay", &token, &["--no-verify"]);
    session.press_enter_at(ink_model::PROMPT);
    let (code, screen) = session.finish();
    let text = String::from_utf8_lossy(&screen).into_owned();
    assert_eq!(code, Some(0), "{text}");
    let leaks = leaked_positions(&screen, &token);
    assert!(leaks.is_empty(), "token bytes at {leaks:?} were shown");
    assert!(text.contains("[token hidden by axocoatl]"), "{text}");
    assert!(text.contains("Your OAuth token (valid for 1 year):"));
    let path = secret_file(&root.path, "claude-code-oauth");
    assert_eq!(std::fs::read(&path).unwrap(), token.as_bytes());
}

/// The terminal is resized while Claude Code waits for the sign-in: the
/// command gets `SIGWINCH`, resizes `claude`'s terminal (which gets the
/// signal and sees the new size), and the token Claude Code then draws for
/// the new width is stored exactly and never shown.
#[test]
fn connect_resizes_claudes_terminal_with_the_users() {
    let root = root();
    let token = fresh_token();
    let new = (60u16, 30u16);
    let (before, after) = ink_model::session(
        &token,
        usize::from(SIZE.0),
        usize::from(SIZE.1),
        ink_model::Shape::Claude,
    );
    write_replay(&root.path, SIZE, &before, &after);
    let resized = ink_model::resized_session(
        &token,
        (usize::from(SIZE.0), usize::from(SIZE.1)),
        (usize::from(new.0), usize::from(new.1)),
    );
    std::fs::write(
        root.path.join(format!("bin/after-{}x{}", new.0, new.1)),
        &resized,
    )
    .unwrap();
    let mut session = Session::connect(&root.path, "replay", &token, &["--no-verify"]);
    session.wait_for(ink_model::PROMPT);
    session
        .master
        .resize(PtySize {
            rows: new.1,
            cols: new.0,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    session.wait_for("[size 30 60]");
    session.press_enter_at("[size 30 60]");
    let (code, screen) = session.finish();
    let text = String::from_utf8_lossy(&screen).into_owned();
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("[SIGWINCH]"), "{text}");
    let leaks = leaked_positions(&screen, &token);
    assert!(leaks.is_empty(), "token bytes at {leaks:?} were shown");
    let path = secret_file(&root.path, "claude-code-oauth");
    assert_eq!(std::fs::read(&path).unwrap(), token.as_bytes());
}

/// A store that could not take the token (a link or a directory where the
/// secret goes, or a full store) is refused before `claude setup-token`
/// runs, so the user never signs in to make a token that is then lost.
#[test]
fn connect_refuses_a_store_that_cannot_take_the_token_before_the_sign_in() {
    let fill = |root: &Path| {
        for index in 0..64 {
            let output = command(root)
                .args(["secret", "set", &format!("filler-{index}"), "-c"])
                .arg(root.join("axocoatl.yaml"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    child.stdin.take().unwrap().write_all(b"sk-test-filler")?;
                    child.wait_with_output()
                })
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    };
    type Prepare<'a> = &'a dyn Fn(&Path);
    let cases: [(&str, Prepare, &str); 3] = [
        (
            "a link",
            &|root: &Path| {
                std::fs::create_dir_all(root.join("data/secrets")).unwrap();
                std::fs::create_dir(root.join("elsewhere")).unwrap();
                std::os::unix::fs::symlink(
                    root.join("elsewhere"),
                    secret_file(root, "claude-code-oauth"),
                )
                .unwrap();
            },
            "unexpected file type",
        ),
        (
            "a directory",
            &|root: &Path| {
                std::fs::create_dir_all(secret_file(root, "claude-code-oauth")).unwrap();
            },
            "unexpected file type",
        ),
        ("a full store", &fill, "already holds 64 secrets"),
    ];
    for (case, prepare, expected) in cases {
        let root = root();
        std::fs::create_dir(root.path.join("data")).unwrap();
        std::fs::set_permissions(
            root.path.join("data"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        prepare(&root.path);
        let token = fresh_token();
        let session = Session::connect(&root.path, "colored", &token, &["--no-verify"]);
        let (code, screen) = session.finish();
        let text = String::from_utf8_lossy(&screen).into_owned();
        assert_eq!(code, Some(5), "{case}: {text}");
        assert!(text.contains(expected), "{case}: {text}");
        assert!(
            text.contains("`claude setup-token` was not run"),
            "{case}: {text}"
        );
        assert!(!text.contains("Press Enter to continue"), "{case}: {text}");
        let secret = secret_file(&root.path, "claude-code-oauth");
        assert!(
            !secret.is_file() || std::fs::symlink_metadata(&secret).unwrap().is_symlink(),
            "{case}"
        );
    }
}

/// `--secret "$TOKEN"` is refused without the value being shown.
#[test]
fn connect_never_shows_a_value_passed_as_the_secret_name() {
    let root = root();
    let value = fresh_token();
    let session = Session::connect(&root.path, "colored", &value, &["--secret", &value]);
    let (code, screen) = session.finish();
    let text = String::from_utf8_lossy(&screen).into_owned();
    assert_eq!(code, Some(3), "{text}");
    assert!(text.contains("not a secret name"), "{text}");
    let leaks = leaked_positions(&screen, &value);
    assert!(
        leaks.is_empty(),
        "value bytes at {leaks:?} were shown:\n{text}"
    );
    assert!(!text.contains("Press Enter to continue"), "{text}");
}

#[test]
fn connect_refuses_without_an_interactive_terminal() {
    let root = root();
    let output = command(&root.path)
        .args(["connect", "claude-code", "-c"])
        .arg(root.path.join("axocoatl.yaml"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("axocoatl connect claude-code needs an interactive terminal"),
        "{stderr}"
    );
    assert!(stderr.contains("--from-env"), "{stderr}");
    assert!(!root.path.join("data").join("secrets").exists());
}

#[test]
fn secret_set_from_env_stores_the_variable_without_echoing_it() {
    let root = root();
    let config = root.path.join("axocoatl.yaml");
    let value = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    let output = command(&root.path)
        .args([
            "secret",
            "set",
            "codex-openai",
            "--from-env",
            "AXO_TEST_KEY",
            "-c",
        ])
        .arg(&config)
        .env("AXO_TEST_KEY", &value)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let all = [output.stdout, output.stderr].concat();
    assert!(!String::from_utf8_lossy(&all).contains(&value[8..]));
    assert!(String::from_utf8_lossy(&all).contains("Stored secret codex-openai"));
    let path = secret_file(&root.path, "codex-openai");
    assert_eq!(std::fs::read(&path).unwrap(), value.as_bytes());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    for (environment, expected) in [
        (None, "named by --from-env is not set"),
        (Some(""), "named by --from-env is empty"),
    ] {
        let mut command = command(&root.path);
        command
            .args([
                "secret",
                "set",
                "codex-openai",
                "--from-env",
                "AXO_TEST_KEY",
                "-c",
            ])
            .arg(&config)
            .stdin(Stdio::null());
        match environment {
            Some(value) => command.env("AXO_TEST_KEY", value),
            None => command.env_remove("AXO_TEST_KEY"),
        };
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{output:?}"
        );
    }
    // The refusals left the stored value alone.
    assert_eq!(std::fs::read(&path).unwrap(), value.as_bytes());
}

/// The common slips that put a value where a name belongs are refused, and
/// no message repeats the value: `--from-env "$TOKEN"`, `--from-env
/// VAR=value`, `secret set NAME "$TOKEN"` and `secret set "$TOKEN"`.
#[test]
fn secret_set_never_shows_a_value_passed_on_the_command_line() {
    let root = root();
    let config = root.path.join("axocoatl.yaml");
    let token = fresh_token();
    let github = format!("ghp_{}", uuid::Uuid::new_v4().simple());
    let with_name = format!("OPENAI_API_KEY={token}");
    let cases: Vec<(Vec<&str>, &str, &str)> = vec![
        (
            vec!["set", "claude-code-oauth", "--from-env", &token],
            &token,
            "--from-env takes the name",
        ),
        (
            vec!["set", "codex-openai", "--from-env", &with_name],
            &token,
            "--from-env takes the name",
        ),
        (
            vec!["set", "codex-openai", "--from-env", &github],
            &github,
            "named by --from-env is not set",
        ),
        (
            vec!["set", "claude-code-oauth", &token],
            &token,
            "never a value on the command line",
        ),
        (
            vec!["set", "codex-openai", &github],
            &github,
            "never a value on the command line",
        ),
        (vec!["set", &token], &token, "not a secret name"),
        (vec!["remove", &token], &token, "not a secret name"),
    ];
    for (args, value, expected) in cases {
        let output = command(&root.path)
            .arg("secret")
            .args(&args)
            .arg("-c")
            .arg(&config)
            .env_remove(&github)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let all = [output.stdout.clone(), output.stderr.clone()].concat();
        let text = String::from_utf8_lossy(&all);
        assert_eq!(output.status.code(), Some(3), "{args:?}: {text}");
        assert!(text.contains(expected), "{args:?}: {text}");
        let leaks = leaked_positions(&all, value);
        assert!(
            leaks.is_empty(),
            "{args:?}: value bytes at {leaks:?}: {text}"
        );
    }
    assert!(!root
        .path
        .join("data")
        .join("secrets")
        .join("claude-code-oauth")
        .exists());
    assert!(!root
        .path
        .join("data")
        .join("secrets")
        .join("codex-openai")
        .exists());
}
