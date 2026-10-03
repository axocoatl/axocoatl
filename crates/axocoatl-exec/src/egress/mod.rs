//! Egress modes of the supervisor binary: the proxy that the daemon drives
//! over stdio (`--egress-proxy`), the loopback/Unix-socket bridge
//! (`--bridge`, which can name the program behind each connection with
//! `--peer-identity`) and a socket probe (`--probe-unix`).
//!
//! The proxy and bridge never resolve names and never execute anything. The
//! daemon decides every destination; see [`protocol`].

pub mod http;
pub mod never;
pub mod peer;
pub mod protocol;

#[cfg(unix)]
pub mod bridge;
#[cfg(unix)]
pub mod proxy;
#[cfg(unix)]
pub mod pump;
