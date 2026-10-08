//! `axocoatl connect claude-code` and `axocoatl secret set --from-env` as
//! the built binary runs them. `connect` runs in a pseudo-terminal (it needs
//! one) with the fake `claude` in `fixtures/fake-claude.sh` and
//! `--no-verify`; verification against a fake HTTPS endpoint is covered by
//! the crate's unit tests. Nothing here reaches a network or a real account.
#![cfg(unix)]

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

#[test]
fn connect_runs_claude_in_a_terminal_and_stores_the_token_it_never_shows() {
    let root = root();
    let bin = root.path.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.sh");
    std::fs::copy(&fake, bin.join("claude")).unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(bin.join("mode"), "colored").unwrap();
    let token = fresh_token();
    std::fs::write(bin.join("token"), &token).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut axocoatl = CommandBuilder::new(BIN);
    axocoatl.args(["connect", "claude-code", "--no-verify", "--claude"]);
    axocoatl.arg(bin.join("claude"));
    axocoatl.arg("-c");
    axocoatl.arg(root.path.join("axocoatl.yaml"));
    axocoatl.cwd(&root.path);
    axocoatl.env("AXOCOATL_DATA_DIR", root.path.join("data"));
    axocoatl.env("HOME", root.path.join("home"));
    axocoatl.env_remove("RUST_LOG");
    let mut child = pair.slave.spawn_command(axocoatl).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
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
    let shown = |screen: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8_lossy(&screen.lock().unwrap()).into_owned()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !shown(&screen).contains("Press Enter to continue") {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("axocoatl exited early ({status:?}):\n{}", shown(&screen));
        }
        assert!(Instant::now() < deadline, "no prompt:\n{}", shown(&screen));
        std::thread::sleep(Duration::from_millis(20));
    }
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "no exit:\n{}", shown(&screen));
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(writer);
    drop(pair.master);
    let _ = reading.join();
    let text = shown(&screen);
    assert!(status.success(), "{status:?}\n{text}");
    let leaks = leaked_positions(&screen.lock().unwrap(), &token);
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
        (None, "AXO_TEST_KEY is not set"),
        (Some(""), "AXO_TEST_KEY is empty"),
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
