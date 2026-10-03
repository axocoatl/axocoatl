//! The bridge's `--peer-identity` through the real supervisor binary: the
//! identity line names the process that opened the connection, from `/proc`.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axocoatl_exec::egress::peer::PeerLookup;
use axocoatl_exec::egress::protocol::PeerIdentity;
use sha2::{Digest, Sha256};

const FIXTURE: &str = "AXOCOATL_PEER_TEST_CONNECT";

fn own_exe() -> String {
    std::fs::read_link("/proc/self/exe")
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// A child process of this test that connects to `address`, sends `hello`
/// and stays connected until its stdin closes.
fn connecting_child(address: std::net::SocketAddr) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "connect_fixture", "--nocapture"])
        .env(FIXTURE, address.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

// A no-op in the normal run; the child of the tests below runs it.
#[test]
fn connect_fixture() {
    let Ok(address) = std::env::var(FIXTURE) else {
        return;
    };
    let mut stream = TcpStream::connect(address).unwrap();
    stream.write_all(b"hello").unwrap();
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
    std::process::exit(0);
}

#[test]
fn the_bridge_names_the_child_process_behind_a_connection() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("identity.sock");
    let proxy = UnixListener::bind(&socket).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_axocoatl-exec-supervisor"))
        .arg("--bridge")
        .arg("--tcp-to-unix")
        .arg(format!("127.0.0.1:{port}={}", socket.display()))
        .arg("--http-errors")
        .arg("--peer-identity")
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let address: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(address).is_err() {
        assert!(Instant::now() < deadline, "the bridge never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    // That probe connection reaches the socket too; skip it.
    let (probe, _) = proxy.accept().unwrap();
    drop(probe);
    let mut child = connecting_child(address);
    let (accepted, _) = proxy.accept().unwrap();
    let mut reader = BufReader::new(accepted);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).unwrap();
    assert!(line.ends_with(b"\r\n"), "{line:?}");
    let identity = PeerIdentity::parse_line(&line[..line.len() - 2]).unwrap();
    let mut hello = [0u8; 5];
    reader.read_exact(&mut hello).unwrap();
    assert_eq!(&hello, b"hello");
    // SAFETY: geteuid and getegid have no arguments and cannot fail.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let exe = own_exe();
    assert_eq!(identity.error, None, "{identity:?}");
    assert_eq!(identity.pid, Some(child.id()));
    assert_eq!((identity.uid, identity.gid), (Some(uid), Some(gid)));
    assert_eq!(identity.exe.as_deref(), Some(exe.as_str()));
    assert_eq!(
        identity.exe_sha256,
        Some(format!(
            "{:x}",
            Sha256::digest(std::fs::read(&exe).unwrap())
        ))
    );
    // Its parent is this test process, the same program.
    assert_eq!(identity.ancestors.first(), Some(&exe), "{identity:?}");
    assert!(identity.ancestors.len() <= 8);
    drop(child.stdin.take());
    child.wait().unwrap();
    bridge.kill().unwrap();
    bridge.wait().unwrap();
}

/// A connection whose program has already gone carries only why it is
/// unnamed, never another process's identity.
#[test]
fn an_exited_peer_is_reported_as_not_found() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut child = connecting_child(listener.local_addr().unwrap());
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    let (accepted, peer) = listener.accept().unwrap();
    let identity = PeerLookup::new().identify(peer, accepted.local_addr().unwrap());
    assert_eq!(identity.error.as_deref(), Some("not_found"), "{identity:?}");
    assert_eq!((identity.pid, identity.exe), (None, None));
}
