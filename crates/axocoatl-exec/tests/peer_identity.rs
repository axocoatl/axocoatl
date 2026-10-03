//! The bridge's `--peer-identity` through the real supervisor binary: the
//! identity line names the process that opened the connection, from `/proc`.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axocoatl_exec::egress::peer::{PeerLookup, HASH_CACHE_SETTLE};
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
    connecting_program(&std::env::current_exe().unwrap(), address)
}

/// [`connecting_child`] run from `program`, a copy of this test binary.
fn connecting_program(program: &Path, address: std::net::SocketAddr) -> std::process::Child {
    let mut attempts = 0;
    loop {
        let spawned = Command::new(program)
            .args(["--exact", "connect_fixture", "--nocapture"])
            .env(FIXTURE, address.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => return child,
            // A child another test thread forked while this file was open
            // for writing holds it until that child execs.
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("spawn {}: {error}", program.display()),
        }
    }
}

/// Accept one connection within ten seconds.
fn accept_within(listener: &TcpListener) -> (TcpStream, std::net::SocketAddr) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                stream.set_nonblocking(false).unwrap();
                return (stream, peer);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "nothing connected");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
}

/// The identity of `child`'s connection on `listener`, after its `hello`.
fn identity_of(
    lookup: &PeerLookup,
    listener: &TcpListener,
    mut child: std::process::Child,
) -> PeerIdentity {
    let (mut accepted, peer) = accept_within(listener);
    accepted
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut hello = [0u8; 5];
    accepted.read_exact(&mut hello).unwrap();
    assert_eq!(&hello, b"hello");
    let identity = lookup.identify(peer, accepted.local_addr().unwrap());
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    identity
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// A program started in a private mount namespace, where another file is
/// bound over the path it runs from, is not named by that path: the
/// identity leaves out the program and its parents and says
/// `foreign_namespace`, and keeps the SHA-256 of the file that really runs.
#[test]
fn a_program_in_another_mount_namespace_is_not_named_by_its_path() {
    let available = Command::new("unshare")
        .args(["-Urm", "true"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("unshare -Urm is not available here; skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::copy(own_exe(), &real).unwrap();
    make_executable(&real);
    let shown = dir.path().join("git");
    std::fs::write(&shown, b"#!/bin/sh\nexit 0\n").unwrap();
    make_executable(&shown);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let script = format!(
        "mount --bind '{}' '{}' && exec '{}' --exact connect_fixture --nocapture",
        real.display(),
        shown.display(),
        shown.display()
    );
    let child = Command::new("unshare")
        .args(["-Urm", "sh", "-c", &script])
        .env(FIXTURE, listener.local_addr().unwrap().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let identity = identity_of(&PeerLookup::new(), &listener, child);
    assert_eq!(identity.pid, Some(pid), "{identity:?}");
    assert_eq!(
        identity.error.as_deref(),
        Some("foreign_namespace"),
        "{identity:?}"
    );
    assert_eq!(identity.exe, None, "{identity:?}");
    assert!(identity.ancestors.is_empty(), "{identity:?}");
    assert_eq!(
        identity.exe_sha256,
        Some(sha256_hex(&std::fs::read(&real).unwrap())),
        "{identity:?}"
    );
}

/// A program rewritten in place at the same size, with its modification time
/// put back, is hashed again: the cache key includes the change time, which
/// no user can set.
#[test]
fn a_program_rewritten_in_place_is_hashed_again() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("program");
    // This test binary with trailing bytes the loader never maps, so they can
    // change while the program still runs.
    let mut bytes = std::fs::read(own_exe()).unwrap();
    bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(&program, &bytes).unwrap();
    make_executable(&program);
    // Old enough for its hash to be cached.
    std::thread::sleep(HASH_CACHE_SETTLE + Duration::from_millis(200));
    let lookup = PeerLookup::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let first = identity_of(&lookup, &listener, connecting_program(&program, address));
    assert_eq!(first.exe_sha256, Some(sha256_hex(&bytes)), "{first:?}");
    // Cached: the same answer again.
    let again = identity_of(&lookup, &listener, connecting_program(&program, address));
    assert_eq!(again.exe_sha256, first.exe_sha256);

    let modified = std::fs::metadata(&program).unwrap().modified().unwrap();
    {
        use std::os::unix::fs::FileExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&program)
            .unwrap();
        file.write_all_at(b"rewritten", (bytes.len() - 16) as u64)
            .unwrap();
        file.set_modified(modified).unwrap();
    }
    let metadata = std::fs::metadata(&program).unwrap();
    assert_eq!(metadata.modified().unwrap(), modified);
    assert_eq!(metadata.len(), bytes.len() as u64);
    let rewritten = std::fs::read(&program).unwrap();
    assert_ne!(rewritten, bytes);
    let second = identity_of(&lookup, &listener, connecting_program(&program, address));
    assert_eq!(
        second.exe_sha256,
        Some(sha256_hex(&rewritten)),
        "{second:?}"
    );
}
