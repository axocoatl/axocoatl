//! The egress modes through the real supervisor binary: argument parsing,
//! the stdio control channel and exit statuses.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axocoatl_exec::egress::protocol::{self, DaemonFrame, SidecarFrame};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_axocoatl-exec-supervisor"))
}

fn socket_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn wait_with_deadline(child: &mut std::process::Child, deadline: Duration) -> Option<i32> {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    None
}

#[test]
fn the_proxy_speaks_the_protocol_over_stdio_and_exits_when_stdin_closes() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("proxy.sock");
    let mut child = binary()
        .args(["--egress-proxy", "--socket"])
        .arg(&socket)
        .args(["--max-connections", "16"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut frame = || {
        let mut line = Vec::new();
        stdout.read_until(b'\n', &mut line).unwrap();
        protocol::decode_sidecar(&line).unwrap()
    };
    match frame() {
        SidecarFrame::Hello {
            protocol,
            max_connections,
            version,
        } => {
            assert_eq!((protocol, max_connections), (2, 16));
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
        }
        other => panic!("{other:?}"),
    }
    stdin
        .write_all(&protocol::encode_daemon(&DaemonFrame::HelloAck { protocol: 2 }).unwrap())
        .unwrap();
    // Every user can connect: the socket is created 0666, never chmod-ed.
    assert_eq!(socket_mode(&socket), 0o666);
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .write_all(b"CONNECT data.attacker.test:443 HTTP/1.1\r\n\r\n")
        .unwrap();
    let SidecarFrame::Open {
        id,
        host,
        port,
        auth,
        ..
    } = frame()
    else {
        panic!("expected open")
    };
    assert_eq!(
        (host.as_str(), port, auth),
        ("data.attacker.test", 443, None)
    );
    stdin
        .write_all(
            &protocol::encode_daemon(&DaemonFrame::Deny {
                id,
                status: 403,
                reason: "not_allowed".into(),
                hint: "data.attacker.test:443 is not in this Session's egress allowlist.".into(),
            })
            .unwrap(),
        )
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{response}"
    );
    assert!(response.contains("X-Axocoatl-Egress: denied; reason=not_allowed"));
    stdin
        .write_all(&protocol::encode_daemon(&DaemonFrame::Ping).unwrap())
        .unwrap();
    assert_eq!(frame(), SidecarFrame::Pong);
    drop(stdin);
    assert_eq!(
        wait_with_deadline(&mut child, Duration::from_secs(5)),
        Some(1)
    );
}

/// The identity socket is a second listener, created 0666 like the first;
/// a connection on it must start with the identity line, which reaches the
/// daemon in the `open` frame.
#[test]
fn the_identity_socket_carries_the_peer_into_open() {
    use axocoatl_exec::egress::protocol::PeerIdentity;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("proxy.sock");
    let identity_socket = dir.path().join("identity.sock");
    let mut child = binary()
        .args(["--egress-proxy", "--socket"])
        .arg(&socket)
        .arg("--identity-socket")
        .arg(&identity_socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut frame = || {
        let mut line = Vec::new();
        stdout.read_until(b'\n', &mut line).unwrap();
        protocol::decode_sidecar(&line).unwrap()
    };
    assert!(matches!(frame(), SidecarFrame::Hello { protocol: 2, .. }));
    stdin
        .write_all(&protocol::encode_daemon(&DaemonFrame::HelloAck { protocol: 2 }).unwrap())
        .unwrap();
    assert_eq!(socket_mode(&socket), 0o666);
    assert_eq!(socket_mode(&identity_socket), 0o666);
    let peer = PeerIdentity {
        pid: Some(77),
        uid: Some(1000),
        exe: Some("/usr/bin/git".into()),
        ..PeerIdentity::default()
    };
    let mut client = UnixStream::connect(&identity_socket).unwrap();
    let mut request = peer.line().unwrap();
    request.extend_from_slice(b"CONNECT github.com:443 HTTP/1.1\r\n\r\n");
    client.write_all(&request).unwrap();
    let SidecarFrame::Open {
        id, peer: carried, ..
    } = frame()
    else {
        panic!("expected open")
    };
    assert_eq!(carried, Some(peer));
    stdin
        .write_all(
            &protocol::encode_daemon(&DaemonFrame::Deny {
                id,
                status: 403,
                reason: "not_allowed".into(),
                hint: "no".into(),
            })
            .unwrap(),
        )
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    drop(stdin);
    assert_eq!(
        wait_with_deadline(&mut child, Duration::from_secs(5)),
        Some(1)
    );
}

#[test]
fn shutdown_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("proxy.sock");
    let mut child = binary()
        .args(["--egress-proxy", "--socket"])
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = Vec::new();
    stdout.read_until(b'\n', &mut line).unwrap();
    assert!(matches!(
        protocol::decode_sidecar(&line).unwrap(),
        SidecarFrame::Hello {
            max_connections: 128,
            ..
        }
    ));
    for frame in [DaemonFrame::HelloAck { protocol: 2 }, DaemonFrame::Shutdown] {
        stdin
            .write_all(&protocol::encode_daemon(&frame).unwrap())
            .unwrap();
    }
    assert_eq!(
        wait_with_deadline(&mut child, Duration::from_secs(5)),
        Some(0)
    );
}

#[test]
fn bad_arguments_and_probes() {
    for arguments in [
        vec!["--egress-proxy"],
        vec!["--egress-proxy", "--socket", "relative.sock"],
        vec![
            "--egress-proxy",
            "--socket",
            "/tmp/x.sock",
            "--max-connections",
            "0",
        ],
        vec![
            "--egress-proxy",
            "--socket",
            "/tmp/x.sock",
            "--max-connections",
            "257",
        ],
        vec![
            "--egress-proxy",
            "--socket",
            "/tmp/x.sock",
            "--identity-socket",
            "/tmp/x.sock",
        ],
        vec![
            "--egress-proxy",
            "--socket",
            "/tmp/x.sock",
            "--identity-socket",
            "relative.sock",
        ],
        vec![
            "--egress-proxy",
            "--socket",
            "/tmp/x.sock",
            "--socket",
            "/tmp/y.sock",
        ],
        vec!["--probe-unix"],
        vec!["--probe-unix", "relative"],
        vec!["--harden"],
        vec!["--harden", "--serve"],
        vec!["--serve", "--harden", "--harden"],
        vec!["--serve", "--landlock"],
        vec!["--anything"],
        vec![],
    ] {
        let status = binary()
            .args(&arguments)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(2), "{arguments:?}");
    }
    let status = binary()
        .args(["--bridge", "--unix-to-tcp", "/tmp/x.sock=10.0.0.1:80"])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sock");
    let status = binary().arg("--probe-unix").arg(&missing).status().unwrap();
    assert_eq!(status.code(), Some(1));
    let listening = dir.path().join("listening.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&listening).unwrap();
    let status = binary()
        .arg("--probe-unix")
        .arg(&listening)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn the_bridge_serves_until_killed() {
    let dir = tempfile::tempdir().unwrap();
    let app = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = app.local_addr().unwrap().port();
    let socket = dir.path().join("app.sock");
    let mut child = binary()
        .arg("--bridge")
        .arg("--unix-to-tcp")
        .arg(format!("{}=127.0.0.1:{port}", socket.display()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match UnixStream::connect(&socket) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => panic!("{error}"),
        }
    };
    assert_eq!(socket_mode(&socket), 0o666);
    let (mut accepted, _) = app.accept().unwrap();
    stream.write_all(b"hello").unwrap();
    let mut received = [0u8; 5];
    accepted.read_exact(&mut received).unwrap();
    assert_eq!(&received, b"hello");
    assert_eq!(child.try_wait().unwrap(), None);
    child.kill().unwrap();
    child.wait().unwrap();
}
