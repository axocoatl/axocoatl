//! `axocoatl` with proxy variables set. `axocoatl run`'s requests to the
//! daemon on this computer, which carry the local API token, go straight to
//! the daemon and never to the proxy, which would see them in plain text
//! (reqwest sends even `127.0.0.1` through `HTTP_PROXY` unless told not to).
//! The daemon here is a loopback listener that records each request and
//! answers 401; nothing reaches a network. And before a command runs,
//! `axocoatl` adds this computer to `NO_PROXY` and `no_proxy`, which every
//! HTTP client it builds and every program it starts read.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_axocoatl");

/// A loopback listener that records each request's head and answers 401;
/// its port and what it saw.
fn listener() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            let body = br#"{"error":"no"}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.write_all(body);
        }
    });
    (port, seen)
}

#[test]
fn run_never_sends_the_daemon_requests_or_its_token_through_a_proxy() {
    let (daemon, to_daemon) = listener();
    let (proxy, to_proxy) = listener();
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    let token = format!("axo-test-{}", uuid::Uuid::new_v4().simple());
    let proxy_url = format!("http://127.0.0.1:{proxy}");
    let output = Command::new(BIN)
        .args(["run", "fix", "--task", "fix it", "--url"])
        .arg(format!("http://127.0.0.1:{daemon}"))
        .arg("-c")
        .arg(root.path().join("config.yaml"))
        .arg("--repo")
        .arg(&repo)
        .args(["--model", "writer=ollama:a", "--model", "reviewer=ollama:b"])
        .env("HOME", root.path())
        .env("AXOCOATL_DATA_DIR", root.path().join("data"))
        .env("AXOCOATL_TOKEN", &token)
        .env("HTTP_PROXY", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&output.stderr);
    // The daemon refused the token (401), so the run did not start.
    assert_ne!(output.status.code(), Some(0), "{shown}");
    let to_daemon = to_daemon.lock().unwrap().clone();
    assert!(!to_daemon.is_empty(), "the daemon saw nothing: {shown}");
    assert!(
        to_daemon[0].starts_with("GET /api/loadouts/fix "),
        "{to_daemon:?}"
    );
    assert!(to_daemon[0].contains(&token), "{to_daemon:?}");
    assert!(
        to_proxy.lock().unwrap().is_empty(),
        "the proxy saw {:?}",
        to_proxy.lock().unwrap()
    );
}

/// `axocoatl doctor` with `HTTP_PROXY` and only `no_proxy` set: the Podman
/// it starts (a shim that writes down both variables and fails) sees this
/// computer added to them, after what `no_proxy` listed. `main` sets them
/// before any command runs, so a command's own HTTP clients read the same
/// values (`proxy_env`'s tests show such a client keeps off the proxy).
#[test]
fn every_command_runs_with_this_computer_in_no_proxy() {
    use std::os::unix::fs::PermissionsExt;
    let (proxy, _) = listener();
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let seen = root.path().join("podman-env");
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            "#!/bin/sh\nprintf 'NO_PROXY=%s no_proxy=%s\\n' \"${{NO_PROXY-unset}}\" \"${{no_proxy-unset}}\" >>'{}'\nexit 125\n",
            seen.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
    // No Ollama CLI and no Ollama server: doctor's verdict does not matter.
    let ollama = bin.join("ollama");
    std::fs::write(&ollama, "#!/bin/sh\nexit 127\n").unwrap();
    std::fs::set_permissions(&ollama, std::fs::Permissions::from_mode(0o755)).unwrap();
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = root.path().join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            "agents:\n  - id: lead\n    name: Lead\n    provider: ollama\n    model: \"a\"\n\
             providers:\n  ollama:\n    base_url: \"http://127.0.0.1:{closed}\"\n\
             server:\n  port: {closed}\n"
        ),
    )
    .unwrap();
    let proxy_url = format!("http://127.0.0.1:{proxy}");
    let output = Command::new(BIN)
        .args(["doctor", "--config"])
        .arg(&config)
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("AXOCOATL_DATA_DIR", root.path().join("data"))
        .env("AXOCOATL_SOCKET_PATH", root.path().join("no-daemon.sock"))
        .env("HTTP_PROXY", &proxy_url)
        .env("no_proxy", "corp.example")
        .env_remove("NO_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .env_remove("CONTAINER_CONNECTION")
        .env_remove("CONTAINER_HOST")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&output.stdout);
    let seen = std::fs::read_to_string(&seen)
        .unwrap_or_else(|error| panic!("doctor started no Podman ({error}):\n{shown}"));
    let expected = "NO_PROXY=corp.example,localhost,127.0.0.0/8,::1 \
                    no_proxy=corp.example,localhost,127.0.0.0/8,::1";
    assert!(!seen.is_empty(), "{shown}");
    for line in seen.lines() {
        assert_eq!(line, expected, "{shown}");
    }
}
