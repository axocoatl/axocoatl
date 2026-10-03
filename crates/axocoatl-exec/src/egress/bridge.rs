//! The bridge (`--bridge`): byte forwarders between loopback TCP and Unix
//! sockets on shared volumes. In a Session container it runs as PID 1 and
//! serves `127.0.0.1:3128` to the egress proxy's socket, and each exposed
//! port as a Unix socket for the browser and Previews.
//!
//! The bridge decides nothing and never resolves a name. Its targets are
//! fixed at startup: `--unix-to-tcp` targets must be loopback, and
//! `--tcp-to-unix` listeners must be loopback unless
//! `--allow-nonloopback-listen` is given (Preview containers only).

use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::http::sidecar_unavailable_response;
use super::protocol::{
    CONNECT_TIMEOUT_MS, DEFAULT_MAX_CONNECTIONS, IDLE_TIMEOUT_MS, MAX_MAX_CONNECTIONS,
};
use super::proxy::bind_unix_listener;
use super::pump::pump;

/// Most listeners one bridge may open.
pub const MAX_LISTENERS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forward {
    /// Listen on TCP, forward to a Unix socket. With `http_errors`, a missing
    /// or refusing socket gets a 502 JSON answer instead of a bare close.
    TcpToUnix {
        listen: SocketAddr,
        target: PathBuf,
        http_errors: bool,
    },
    /// Listen on a Unix socket (mode 0666), forward to loopback TCP.
    UnixToTcp { listen: PathBuf, target: SocketAddr },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    pub forwards: Vec<Forward>,
    pub max_connections: usize,
    pub allow_nonloopback_listen: bool,
}

fn loopback(address: &SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

fn split_pair(value: &str) -> Result<(&str, &str), String> {
    value
        .split_once('=')
        .filter(|(left, right)| !left.is_empty() && !right.is_empty())
        .ok_or_else(|| format!("expected <from>=<to>, got {value:?}"))
}

fn socket_address(value: &str) -> Result<SocketAddr, String> {
    value
        .parse()
        .map_err(|_| format!("{value:?} is not an ip:port address"))
}

fn socket_path(value: &str) -> Result<PathBuf, String> {
    if !value.starts_with('/') || value.contains('\0') || value.len() > 100 {
        return Err(format!("{value:?} is not an absolute socket path"));
    }
    Ok(PathBuf::from(value))
}

impl BridgeConfig {
    /// Parse the arguments after `--bridge`.
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut config = Self {
            forwards: Vec::new(),
            max_connections: DEFAULT_MAX_CONNECTIONS as usize,
            allow_nonloopback_listen: false,
        };
        let mut index = 0;
        while index < arguments.len() {
            let value = |index: usize| {
                arguments
                    .get(index + 1)
                    .map(String::as_str)
                    .ok_or_else(|| format!("{} needs a value", arguments[index]))
            };
            match arguments[index].as_str() {
                "--tcp-to-unix" => {
                    let (listen, target) = split_pair(value(index)?)?;
                    config.forwards.push(Forward::TcpToUnix {
                        listen: socket_address(listen)?,
                        target: socket_path(target)?,
                        http_errors: false,
                    });
                    index += 2;
                }
                "--http-errors" => {
                    match config.forwards.last_mut() {
                        Some(Forward::TcpToUnix { http_errors, .. }) => *http_errors = true,
                        _ => {
                            return Err("--http-errors must follow a --tcp-to-unix listener".into())
                        }
                    }
                    index += 1;
                }
                "--unix-to-tcp" => {
                    let (listen, target) = split_pair(value(index)?)?;
                    config.forwards.push(Forward::UnixToTcp {
                        listen: socket_path(listen)?,
                        target: socket_address(target)?,
                    });
                    index += 2;
                }
                "--max-connections" => {
                    config.max_connections = value(index)?
                        .parse()
                        .map_err(|_| "--max-connections needs a number".to_string())?;
                    index += 2;
                }
                "--allow-nonloopback-listen" => {
                    config.allow_nonloopback_listen = true;
                    index += 1;
                }
                other => return Err(format!("unknown bridge argument {other:?}")),
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.forwards.len() > MAX_LISTENERS {
            return Err(format!("at most {MAX_LISTENERS} listeners"));
        }
        if self.max_connections == 0 || self.max_connections > MAX_MAX_CONNECTIONS as usize {
            return Err(format!("--max-connections must be 1-{MAX_MAX_CONNECTIONS}"));
        }
        for forward in &self.forwards {
            match forward {
                Forward::TcpToUnix { listen, .. }
                    if !self.allow_nonloopback_listen && !loopback(listen) =>
                {
                    return Err(format!("{listen} is not a loopback listen address"))
                }
                Forward::UnixToTcp { target, .. } if !loopback(target) => {
                    return Err(format!("{target} is not a loopback target"))
                }
                _ => {}
            }
        }
        Ok(())
    }
}

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
    fn fd(&self) -> RawFd {
        match self {
            Self::Tcp(listener) => listener.as_raw_fd(),
            Self::Unix(listener) => listener.as_raw_fd(),
        }
    }
}

enum Accepted {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Accepted {
    fn fd(&self) -> RawFd {
        match self {
            Self::Tcp(stream) => stream.as_raw_fd(),
            Self::Unix(stream) => stream.as_raw_fd(),
        }
    }
}

struct Slots {
    max: usize,
    active: Mutex<usize>,
    freed: Condvar,
}

/// A running bridge. Dropping it does not stop it; call [`Bridge::stop`].
pub struct Bridge {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Bridge {
    /// Bind every listener, then serve them. A bind failure is returned before
    /// anything is served, so a container with a bad bridge fails to start.
    pub fn start(config: BridgeConfig) -> Result<Self, String> {
        config.validate()?;
        let mut bound = Vec::new();
        for forward in &config.forwards {
            let listener = match forward {
                Forward::TcpToUnix { listen, .. } => Listener::Tcp(
                    TcpListener::bind(listen)
                        .map_err(|error| format!("listen on {listen}: {error}"))?,
                ),
                Forward::UnixToTcp { listen, .. } => Listener::Unix(
                    bind_unix_listener(listen)
                        .map_err(|error| format!("listen on {}: {error}", listen.display()))?,
                ),
            };
            bound.push((listener, forward.clone()));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let slots = Arc::new(Slots {
            max: config.max_connections,
            active: Mutex::new(0),
            freed: Condvar::new(),
        });
        let threads = bound
            .into_iter()
            .map(|(listener, forward)| {
                let stop = stop.clone();
                let slots = slots.clone();
                std::thread::spawn(move || serve(listener, forward, slots, stop))
            })
            .collect();
        Ok(Self { stop, threads })
    }

    /// Stop accepting and wait for the listener threads. Open connections
    /// finish on their own.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads {
            let _ = thread.join();
        }
    }
}

fn accept(listener: &Listener) -> io::Result<Accepted> {
    match listener {
        Listener::Tcp(listener) => listener.accept().map(|(stream, _)| Accepted::Tcp(stream)),
        Listener::Unix(listener) => listener.accept().map(|(stream, _)| Accepted::Unix(stream)),
    }
}

fn serve(listener: Listener, forward: Forward, slots: Arc<Slots>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        {
            let mut active = slots
                .active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            while *active >= slots.max && !stop.load(Ordering::Acquire) {
                active = slots
                    .freed
                    .wait_timeout(active, Duration::from_millis(200))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
            if stop.load(Ordering::Acquire) {
                return;
            }
        }
        let mut entry = libc::pollfd {
            fd: listener.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd.
        let ready = unsafe { libc::poll(&mut entry, 1, 200) };
        if ready <= 0 {
            continue;
        }
        let accepted = match accept(&listener) {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        *slots
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) += 1;
        let forward = forward.clone();
        let slots = slots.clone();
        std::thread::spawn(move || {
            connect_and_pump(accepted, &forward);
            let mut active = slots
                .active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *active -= 1;
            slots.freed.notify_one();
        });
    }
}

fn connect_and_pump(accepted: Accepted, forward: &Forward) {
    let never_revoked = AtomicBool::new(false);
    let idle = Duration::from_millis(IDLE_TIMEOUT_MS);
    match forward {
        Forward::TcpToUnix {
            target,
            http_errors,
            ..
        } => match UnixStream::connect(target) {
            Ok(upstream) => {
                pump(
                    accepted.fd(),
                    upstream.as_raw_fd(),
                    &[],
                    idle,
                    &never_revoked,
                );
            }
            Err(_) => {
                if *http_errors {
                    if let Accepted::Tcp(mut stream) = accepted {
                        let _ = stream.write_all(&sidecar_unavailable_response());
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        // Let the client read the answer before the close.
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut sink = [0u8; 1024];
                        let _ = io::Read::read(&mut stream, &mut sink);
                    }
                }
            }
        },
        Forward::UnixToTcp { target, .. } => {
            if let Ok(upstream) =
                TcpStream::connect_timeout(target, Duration::from_millis(CONNECT_TIMEOUT_MS))
            {
                pump(
                    accepted.fd(),
                    upstream.as_raw_fd(),
                    &[],
                    idle,
                    &never_revoked,
                );
            }
        }
    }
}

/// Reap orphaned children every 500 ms when running as PID 1. Installs no
/// signal handler: inside its PID namespace PID 1 then ignores SIGTERM and
/// SIGKILL from other processes there.
pub fn reap_orphans_if_init() {
    // SAFETY: getpid has no arguments and cannot fail.
    if unsafe { libc::getpid() } != 1 {
        return;
    }
    std::thread::spawn(|| loop {
        loop {
            let mut status = 0;
            // SAFETY: waitpid with a valid status pointer.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    });
}

/// `--bridge ...`: start, then serve forever. Exits 1 on a setup error.
pub fn main(arguments: &[String]) -> i32 {
    let config = match BridgeConfig::parse(arguments) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("bridge: {error}");
            return 1;
        }
    };
    reap_orphans_if_init();
    super::proxy::connectable_sockets_by_default();
    match Bridge::start(config) {
        Ok(bridge) => {
            // The bridge never exits after startup; removal kills it from
            // outside the container.
            let _bridge = bridge;
            loop {
                std::thread::park();
            }
        }
        Err(error) => {
            eprintln!("bridge: {error}");
            1
        }
    }
}

/// `--probe-unix <path>`: whether a connection to the socket succeeds within
/// one second.
pub fn probe_unix(path: PathBuf) -> bool {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(UnixStream::connect(path).is_ok());
    });
    receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn arguments_parse_and_hard_rules_hold() {
        let config = BridgeConfig::parse(&args(&[
            "--tcp-to-unix",
            "127.0.0.1:3128=/run/axocoatl-egress/proxy.sock",
            "--http-errors",
            "--unix-to-tcp",
            "/run/axocoatl-svc/5173.sock=127.0.0.1:5173",
            "--tcp-to-unix",
            "[::1]:5173=/run/axocoatl-svc/5173.sock",
            "--max-connections",
            "64",
        ]))
        .unwrap();
        assert_eq!(config.max_connections, 64);
        assert_eq!(
            config.forwards,
            vec![
                Forward::TcpToUnix {
                    listen: "127.0.0.1:3128".parse().unwrap(),
                    target: "/run/axocoatl-egress/proxy.sock".into(),
                    http_errors: true,
                },
                Forward::UnixToTcp {
                    listen: "/run/axocoatl-svc/5173.sock".into(),
                    target: "127.0.0.1:5173".parse().unwrap(),
                },
                Forward::TcpToUnix {
                    listen: "[::1]:5173".parse().unwrap(),
                    target: "/run/axocoatl-svc/5173.sock".into(),
                    http_errors: false,
                },
            ]
        );
        for (refused, expected) in [
            (vec!["--http-errors"], "must follow"),
            (
                vec!["--unix-to-tcp", "/x.sock=10.0.0.1:80"],
                "not a loopback target",
            ),
            (
                vec!["--unix-to-tcp", "/x.sock=localhost:80"],
                "not an ip:port",
            ),
            (
                vec!["--tcp-to-unix", "0.0.0.0:80=/x.sock"],
                "not a loopback listen",
            ),
            (
                vec!["--tcp-to-unix", "127.0.0.1:80=relative.sock"],
                "absolute",
            ),
            (
                vec!["--tcp-to-unix", "127.0.0.1:80"],
                "expected <from>=<to>",
            ),
            (vec!["--tcp-to-unix"], "needs a value"),
            (vec!["--max-connections", "0"], "1-256"),
            (vec!["--max-connections", "257"], "1-256"),
            (vec!["--exec", "sh"], "unknown bridge argument"),
        ] {
            let error = BridgeConfig::parse(&args(&refused)).unwrap_err();
            assert!(error.contains(expected), "{refused:?}: {error}");
        }
        let preview = BridgeConfig::parse(&args(&[
            "--allow-nonloopback-listen",
            "--tcp-to-unix",
            "0.0.0.0:8080=/run/axocoatl-svc/8080.sock",
        ]))
        .unwrap();
        assert!(preview.allow_nonloopback_listen);
        let many: Vec<String> = (0..65)
            .flat_map(|index| {
                [
                    "--unix-to-tcp".to_string(),
                    format!("/run/s{index}.sock=127.0.0.1:{}", 1000 + index),
                ]
            })
            .collect();
        assert!(BridgeConfig::parse(&many)
            .unwrap_err()
            .contains("at most 64"));
    }

    fn free_tcp() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[test]
    fn tcp_to_unix_and_unix_to_tcp_carry_a_mebibyte_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        // App: TCP echo server on loopback.
        let app = TcpListener::bind("127.0.0.1:0").unwrap();
        let app_address = app.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in app.incoming() {
                let mut stream = stream.unwrap();
                std::thread::spawn(move || {
                    let mut buffer = vec![0u8; 65536];
                    loop {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => stream.write_all(&buffer[..read]).unwrap(),
                        }
                    }
                });
            }
        });
        let service = dir.path().join("app.sock");
        let front = free_tcp();
        let bridge = Bridge::start(BridgeConfig {
            forwards: vec![
                Forward::UnixToTcp {
                    listen: service.clone(),
                    target: app_address,
                },
                Forward::TcpToUnix {
                    listen: front,
                    target: service.clone(),
                    http_errors: false,
                },
            ],
            max_connections: 16,
            allow_nonloopback_listen: false,
        })
        .unwrap();
        // TCP -> Unix -> TCP app and back.
        let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 253) as u8).collect();
        let mut client = TcpStream::connect(front).unwrap();
        let mut writer = client.try_clone().unwrap();
        let upload = payload.clone();
        let sender = std::thread::spawn(move || {
            writer.write_all(&upload).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let mut received = Vec::new();
        client.read_to_end(&mut received).unwrap();
        sender.join().unwrap();
        assert_eq!(received, payload);
        // Its mode (0666) comes from the umask the binary sets; see
        // tests/egress_binary.rs.
        assert!(probe_unix(service.clone()));
        bridge.stop();
        assert!(!probe_unix(dir.path().join("missing.sock")));
    }

    #[test]
    fn http_errors_answer_502_when_the_proxy_socket_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let listen = free_tcp();
        let bridge = Bridge::start(BridgeConfig {
            forwards: vec![Forward::TcpToUnix {
                listen,
                target: dir.path().join("proxy.sock"),
                http_errors: true,
            }],
            max_connections: 4,
            allow_nonloopback_listen: false,
        })
        .unwrap();
        let mut client = TcpStream::connect(listen).unwrap();
        client
            .write_all(b"CONNECT a.test:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
            "{response}"
        );
        assert!(response.contains("sidecar_unavailable"));
        bridge.stop();
    }

    #[test]
    fn a_bind_failure_is_a_setup_error() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let error = Bridge::start(BridgeConfig {
            forwards: vec![Forward::TcpToUnix {
                listen: taken.local_addr().unwrap(),
                target: "/tmp/x.sock".into(),
                http_errors: false,
            }],
            max_connections: 4,
            allow_nonloopback_listen: false,
        })
        .err()
        .unwrap();
        assert!(error.contains("listen on"), "{error}");
        assert_eq!(main(&args(&["--unix-to-tcp", "/x.sock=10.0.0.1:1"])), 1);
    }

    #[test]
    fn two_hundred_parallel_connections_with_a_cap_of_128_all_complete() {
        let dir = tempfile::tempdir().unwrap();
        let app = TcpListener::bind("127.0.0.1:0").unwrap();
        let app_address = app.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in app.incoming() {
                let mut stream = stream.unwrap();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 64];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = stream.write_all(&buffer[..read]);
                });
            }
        });
        let service = dir.path().join("app.sock");
        let bridge = Bridge::start(BridgeConfig {
            forwards: vec![Forward::UnixToTcp {
                listen: service.clone(),
                target: app_address,
            }],
            max_connections: 128,
            allow_nonloopback_listen: false,
        })
        .unwrap();
        let clients: Vec<_> = (0..200)
            .map(|index| {
                let service = service.clone();
                std::thread::spawn(move || {
                    // Linux blocks a connect while the listen backlog is
                    // full; macOS refuses it, so retry briefly there.
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    let mut stream = loop {
                        match UnixStream::connect(&service) {
                            Ok(stream) => break stream,
                            Err(error)
                                if error.kind() == io::ErrorKind::ConnectionRefused
                                    && std::time::Instant::now() < deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(error) => panic!("{error}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(30)))
                        .unwrap();
                    let message = format!("m{index}");
                    stream.write_all(message.as_bytes()).unwrap();
                    let mut reply = vec![0u8; message.len()];
                    stream.read_exact(&mut reply).unwrap();
                    assert_eq!(reply, message.as_bytes());
                })
            })
            .collect();
        for client in clients {
            client.join().unwrap();
        }
        bridge.stop();
    }
}
