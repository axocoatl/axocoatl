//! `axocoatl connect codex` as the built binary runs it: in a
//! pseudo-terminal (it needs one), with the fake `codex` in
//! `fixtures/fake-codex.sh`, a private `HOME`, and Codex's own credential
//! and endpoint variables set in its environment (which `codex login` must
//! not inherit). Nothing here reaches a network or a real account.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const BIN: &str = env!("CARGO_BIN_EXE_axocoatl");

fn jwt() -> String {
    let part = || uuid::Uuid::new_v4().simple().to_string();
    format!("eyJhbGciOiJSUzI1NiJ9.eyJ{}{}.{}", part(), part(), part())
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

/// Every path under `directory` (no link followed).
fn tree(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        out.push(path.clone());
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            tree(&path, out);
        }
    }
}

/// The rows a terminal `cols` wide shows for `text` (lines ending in CR
/// LF), each without its trailing spaces: a longer line wraps onto the next
/// rows. Escapes are counted as characters, so only rows without them are
/// exact.
fn rows(text: &str, cols: usize) -> Vec<String> {
    text.split("\r\n")
        .flat_map(|line| {
            let chars: Vec<char> = line.chars().collect();
            if chars.is_empty() {
                return vec![String::new()];
            }
            chars
                .chunks(cols)
                .map(|row| row.iter().collect::<String>().trim_end().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn connect_codex_signs_in_with_a_private_codex_home_and_stores_the_sign_in() {
    connect_codex_with_data_root(false);
}

/// A relative data root (`AXOCOATL_DATA_DIR=data`, or `./data` beside the
/// configuration file): Codex runs in its temporary home and reads a
/// relative `CODEX_HOME` from there, so it must get an absolute one.
#[test]
fn connect_codex_works_with_a_relative_data_root() {
    connect_codex_with_data_root(true);
}

fn connect_codex_with_data_root(relative: bool) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&home).unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh");
    std::fs::copy(&fake, bin.join("codex")).unwrap();
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(bin.join("mode"), "ok").unwrap();
    let (access, refresh) = (jwt(), format!("rt_{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(
        bin.join("auth"),
        serde_json::to_string_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": format!("sk-proj-{}", uuid::Uuid::new_v4().simple()),
            "tokens": {"id_token": jwt(), "access_token": access,
                "refresh_token": refresh, "account_id": "acct-e2e"},
            "last_refresh": "2026-10-11T12:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
    let shown_key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    std::fs::write(bin.join("screen"), format!("{shown_key}\n{}\n", jwt())).unwrap();
    // The user's own Codex sign-in, which must stay untouched.
    std::fs::create_dir(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/auth.json"), "{\"user\":\"own\"}").unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut axocoatl = CommandBuilder::new(BIN);
    axocoatl.args(["connect", "codex", "--codex"]);
    axocoatl.arg(bin.join("codex"));
    axocoatl.arg("-c");
    axocoatl.arg(root.join("axocoatl.yaml"));
    axocoatl.cwd(&root);
    if relative {
        axocoatl.env("AXOCOATL_DATA_DIR", "data");
    } else {
        axocoatl.env("AXOCOATL_DATA_DIR", root.join("data"));
    }
    axocoatl.env("HOME", &home);
    axocoatl.env_remove("RUST_LOG");
    for variable in [
        "CODEX_API_KEY",
        "OPENAI_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        "CODEX_SQLITE_HOME",
    ] {
        axocoatl.env(variable, "set-by-the-test");
    }
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
    let shown = || String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !shown().contains("Press Enter once you have signed in") {
        assert!(
            child.try_wait().unwrap().is_none(),
            "exited early:\n{}",
            shown()
        );
        assert!(Instant::now() < deadline, "no prompt:\n{}", shown());
        std::thread::sleep(Duration::from_millis(20));
    }
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "no exit:\n{}", shown());
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(writer);
    drop(pair.master);
    let _ = reading.join();
    let all = screen.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&all).into_owned();
    assert_eq!(status.exit_code(), 0, "{text}");
    assert!(
        text.contains("Connected Codex: stored your ChatGPT sign-in as secret codex-chatgpt"),
        "{text}"
    );
    // Exactly one blank row between Codex's last line and the outcome: the
    // spaces the hidden token is padded with wrap onto one, so no line
    // break is added to it.
    let before = &text[..text.find("✓ Connected Codex").unwrap()];
    let rows = rows(before, 100);
    let [.., last, blank, outcome] = rows.as_slice() else {
        panic!("{text:?}");
    };
    assert!(last.starts_with("debug: token "), "{text:?}");
    assert!(blank.is_empty() && outcome.is_empty(), "{text:?}");
    for secret in [&access, &refresh, &shown_key] {
        assert!(leaked_positions(&all, secret).is_empty(), "{text}");
    }
    // Codex ran without the variables that would change its sign-in.
    let seen = std::fs::read_to_string(bin.join("seen")).unwrap();
    for variable in [
        "CODEX_API_KEY",
        "OPENAI_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        "CODEX_SQLITE_HOME",
    ] {
        assert!(seen.contains(&format!("{variable}=unset\n")), "{seen}");
    }
    let codex_home = seen
        .lines()
        .find_map(|line| line.strip_prefix("codex_home="))
        .unwrap();
    assert!(
        codex_home.starts_with(&root.join("data").display().to_string()),
        "{seen}"
    );
    // Stored as one secret; the data root holds nothing else new, the
    // temporary home is gone and the user's ~/.codex is as it was.
    let secret = root.join("data/secrets/codex-chatgpt");
    let stored = std::fs::read_to_string(&secret).unwrap();
    assert!(stored.starts_with("{\"format\":\"axocoatl.codex-chatgpt/1\""));
    assert!(stored.contains(&access) && stored.contains(&refresh));
    assert!(stored.contains("acct-e2e"));
    assert!(!stored.contains("sk-proj-"));
    assert_eq!(
        std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!Path::new(codex_home).exists());
    let mut everything = Vec::new();
    tree(&root, &mut everything);
    for path in &everything {
        if path.is_file() && path != &secret && path != &bin.join("auth") {
            let bytes = std::fs::read(path).unwrap();
            for value in [&access, &refresh] {
                assert!(
                    !bytes
                        .windows(value.len())
                        .any(|window| window == value.as_bytes()),
                    "{}",
                    path.display()
                );
            }
        }
    }
    let mut in_home = Vec::new();
    tree(&home, &mut in_home);
    assert_eq!(
        in_home,
        [home.join(".codex"), home.join(".codex/auth.json")]
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".codex/auth.json")).unwrap(),
        "{\"user\":\"own\"}"
    );
    // `secret list` names it, never its value.
    let listed = Command::new(BIN)
        .args(["secret", "list", "-c"])
        .arg(root.join("axocoatl.yaml"))
        .env("AXOCOATL_DATA_DIR", root.join("data"))
        .env("HOME", &home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "codex-chatgpt\n");
}

#[test]
fn connect_codex_refuses_without_an_interactive_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .args(["connect", "codex", "-c"])
        .arg(directory.path().join("axocoatl.yaml"))
        .env("AXOCOATL_DATA_DIR", directory.path().join("data"))
        .env("HOME", directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs an interactive terminal"), "{stderr}");
    assert!(stderr.contains("codex login"), "{stderr}");
}

/// Run the built `axocoatl connect codex --codex <fake> ARGS` in a
/// pseudo-terminal in `root`, pressing Enter at the fake's prompt only when
/// `prompted` (a run refused before `codex login` never shows it); its exit
/// code and everything it showed.
fn run_in_terminal_until(root: &Path, args: &[&str], prompted: bool) -> (u32, Vec<u8>) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut axocoatl = CommandBuilder::new(BIN);
    axocoatl.args(["connect", "codex", "--codex"]);
    axocoatl.arg(root.join("bin/codex"));
    axocoatl.arg("-c");
    axocoatl.arg(root.join("axocoatl.yaml"));
    axocoatl.args(args);
    axocoatl.cwd(root);
    axocoatl.env("AXOCOATL_DATA_DIR", root.join("data"));
    axocoatl.env("HOME", root.join("home"));
    axocoatl.env("TMPDIR", root.join("tmp"));
    axocoatl.env_remove("RUST_LOG");
    axocoatl.env("OPENAI_API_KEY", "sk-proj-set-by-the-test-on-the-host");
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
    let shown = || String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    while prompted && !shown().contains("Press Enter once you have signed in") {
        assert!(
            child.try_wait().unwrap().is_none(),
            "exited early:\n{}",
            shown()
        );
        assert!(Instant::now() < deadline, "no prompt:\n{}", shown());
        std::thread::sleep(Duration::from_millis(20));
    }
    if prompted {
        writer.write_all(b"\r").unwrap();
        writer.flush().unwrap();
    }
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "no exit:\n{}", shown());
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(writer);
    drop(pair.master);
    let _ = reading.join();
    let all = screen.lock().unwrap().clone();
    (status.exit_code(), all)
}

/// A private root with the fake `codex` (mode `ok`), writing `auth.json`
/// with `api_key` as its `OPENAI_API_KEY`; the access and refresh tokens.
fn api_key_root(root: &Path, api_key: serde_json::Value) -> (String, String) {
    for directory in ["bin", "home", "tmp"] {
        std::fs::create_dir(root.join(directory)).unwrap();
    }
    let bin = root.join("bin");
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-codex.sh");
    std::fs::copy(&fake, bin.join("codex")).unwrap();
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(bin.join("mode"), "ok").unwrap();
    let (access, refresh) = (jwt(), format!("rt_{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(
        bin.join("auth"),
        serde_json::to_string_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": api_key,
            "tokens": {"id_token": jwt(), "access_token": access,
                "refresh_token": refresh, "account_id": "acct-e2e"},
            "last_refresh": "2026-10-11T12:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
    let shown_key = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    std::fs::write(bin.join("screen"), format!("{shown_key}\n{}\n", jwt())).unwrap();
    (access, refresh)
}

/// `connect codex --api-key` and `--api-key-only` as the built binary runs
/// them: this version refuses both before anything runs (the route was
/// never tried with an OpenAI Platform organization), with exit code 3 and a
/// message that points to the ChatGPT sign-in. `codex login` never runs,
/// even when Codex would have issued a key, nothing is stored, and the data
/// root is not even made.
#[test]
fn connect_codex_refuses_the_api_key_route_and_points_to_the_chatgpt_sign_in() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let key = format!(
        "sk-proj-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    for (name, flag) in [("key", "--api-key"), ("key-only", "--api-key-only")] {
        let run = root.join(name);
        std::fs::create_dir(&run).unwrap();
        api_key_root(&run, serde_json::json!(key));
        let (code, shown) = run_in_terminal_until(&run, &[flag], false);
        let text = String::from_utf8_lossy(&shown).into_owned();
        assert_eq!(code, 3, "{text}");
        let flat: String = text.split(['\r', '\n']).collect();
        assert!(
            flat.contains(&format!(
                "axocoatl connect codex {flag} is not available in Axocoatl {}: storing the \
                 OpenAI API key OpenAI issues with the sign-in was never tried with an OpenAI \
                 Platform organization, so nothing was run or stored. Sign in with your ChatGPT \
                 plan instead: `axocoatl connect codex` stores your ChatGPT sign-in as \
                 codex-chatgpt",
                env!("CARGO_PKG_VERSION")
            )),
            "{text}"
        );
        assert!(!run.join("bin/seen").exists(), "codex login ran:\n{text}");
        assert!(!run.join("data").exists(), "{text}");
        assert!(leaked_positions(&shown, &key).is_empty(), "{text}");
    }
    // Without a terminal, and with --device-auth, the same refusal comes
    // first.
    for args in [&["--api-key"][..], &["--api-key-only", "--device-auth"]] {
        let output = Command::new(BIN)
            .args(["connect", "codex", "-c"])
            .arg(root.join("axocoatl.yaml"))
            .args(args)
            .env("AXOCOATL_DATA_DIR", root.join("data"))
            .env("HOME", &root)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(3), "{args:?}: {stderr}");
        assert!(stderr.contains("is not available in Axocoatl"), "{stderr}");
        assert!(!root.join("data").exists(), "{args:?}");
    }
    // Neither flag is offered in the help.
    let help = Command::new(BIN)
        .args(["connect", "codex", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(!help.contains("--api-key"), "{help}");
}

/// `--api-key` (refused in this version, also with `--device-auth`), and a
/// key typed on the command line (after `--api-key`, as `--api-key=KEY`, as
/// one argument with `--api-key`, to `--sign-in-timeout` or to `--url`), are
/// refused before anything runs, with exit code 3; the value is never
/// repeated.
#[test]
fn connect_codex_refuses_device_auth_and_a_typed_key_with_api_key() {
    let directory = tempfile::tempdir().unwrap();
    let typed = format!("sk-proj-{}", uuid::Uuid::new_v4().simple());
    let with_equals = format!("--api-key={typed}");
    let timeout_equals = format!("--sign-in-timeout={typed}");
    let one_argument = format!("--api-key {typed}");
    for (args, said) in [
        (
            vec!["--api-key", "--device-auth"],
            "axocoatl connect codex --api-key is not available in Axocoatl",
        ),
        (
            vec!["--api-key-only", "--device-auth"],
            "axocoatl connect codex --api-key-only is not available in Axocoatl",
        ),
        (
            vec!["--api-key", typed.as_str()],
            "axocoatl connect codex takes no key, token or other value on its command line",
        ),
        (vec![with_equals.as_str()], "--api-key takes no value"),
        (
            vec!["--sign-in-timeout", typed.as_str()],
            "the value given to --sign-in-timeout is not one it takes",
        ),
        (
            vec![timeout_equals.as_str()],
            "the value given to --sign-in-timeout is not one it takes",
        ),
        (
            vec![one_argument.as_str()],
            "an argument that starts with --api-key is not one axocoatl connect takes",
        ),
        (
            vec!["--refresh", "--url", typed.as_str()],
            "--url is not an http:// or https:// URL",
        ),
    ] {
        let output = Command::new(BIN)
            .args(["connect", "codex", "-c"])
            .arg(directory.path().join("axocoatl.yaml"))
            .args(&args)
            .env("AXOCOATL_DATA_DIR", directory.path().join("data"))
            .env("HOME", directory.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(3), "{args:?}: {stderr}");
        assert!(stderr.contains(said), "{args:?}: {stderr}");
        let all = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
        assert!(leaked_positions(&all, &typed).is_empty(), "{stderr}");
        assert!(!directory.path().join("data").exists(), "{args:?}");
    }
}
