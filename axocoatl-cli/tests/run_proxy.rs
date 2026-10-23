//! `axocoatl run` with proxy variables set: its requests to the daemon on
//! this computer, which carry the local API token, go straight to the
//! daemon and never to the proxy, which would see them in plain text
//! (reqwest sends even `127.0.0.1` through `HTTP_PROXY` unless told not to).
//! The daemon here is a loopback listener that records each request and
//! answers 401; nothing reaches a network.
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
