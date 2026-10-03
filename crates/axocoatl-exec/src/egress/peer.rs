//! Who is behind a loopback TCP connection: the bridge's `--peer-identity`.
//!
//! On Linux the bridge finds the client's socket in `/proc/net/tcp` (or
//! `tcp6`) by its address pair, then the process that holds that socket by
//! scanning `/proc/<pid>/fd`, and reads that process's executable, user,
//! group and parent chain from `/proc`. It hashes the executable's contents,
//! caching each file's hash by device, inode, size and modification time.
//! Reading another user's `/proc/<pid>/fd` and `exe` needs `CAP_SYS_PTRACE`;
//! without it the identity carries the socket's user and `error: no_access`.
//!
//! A program can be made to act for another (for example with `LD_PRELOAD`,
//! an interpreter, or a descriptor passed to a child), so an identity narrows
//! what a connection is used for rather than proving it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::Duration;

use super::protocol::PeerIdentity;
#[cfg(target_os = "linux")]
use super::protocol::{MAX_PEER_ANCESTORS, MAX_PEER_ANCESTOR_CHARS, MAX_PEER_PATH_CHARS};

/// Most processes one lookup inspects.
pub const MAX_SCANNED_PIDS: usize = 4096;
/// Longest one socket-holder scan may take.
#[cfg(target_os = "linux")]
pub const SCAN_BUDGET: Duration = Duration::from_millis(100);
/// Executables larger than this are not hashed.
pub const MAX_HASHED_BYTES: u64 = 512 * 1024 * 1024;
/// Most cached executable hashes.
#[cfg(target_os = "linux")]
const MAX_CACHED_HASHES: usize = 256;

/// The identity of a file's contents, for the hash cache.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

/// Looks up connection peers; keeps executable hashes between lookups.
#[derive(Debug, Default)]
pub struct PeerLookup {
    hashes: Mutex<HashMap<FileKey, String>>,
}

/// A path as text: lossy UTF-8 with control characters replaced, so it can
/// never break the identity line or a record.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn path_text(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect()
}

impl PeerLookup {
    pub fn new() -> Self {
        Self::default()
    }

    /// The identity of the process behind the accepted connection whose
    /// peer address is `client` and local address is `server`.
    pub fn identify(&self, client: SocketAddr, server: SocketAddr) -> PeerIdentity {
        #[cfg(target_os = "linux")]
        {
            self.identify_linux(client, server)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (client, server, &self.hashes);
            PeerIdentity::failed("unsupported")
        }
    }

    /// The identity line for this connection, trimmed to fit
    /// `MAX_PEER_LINE_BYTES`: ancestors go first, then the executable path.
    pub fn line(&self, client: SocketAddr, server: SocketAddr) -> Vec<u8> {
        fit_line(self.identify(client, server))
    }

    #[cfg(target_os = "linux")]
    fn identify_linux(&self, client: SocketAddr, server: SocketAddr) -> PeerIdentity {
        let (inode, socket_uid) = match linux::find_socket(client, server) {
            Ok(found) => found,
            Err(reason) => return PeerIdentity::failed(reason),
        };
        let mut identity = PeerIdentity {
            uid: Some(socket_uid),
            ..PeerIdentity::default()
        };
        let pid = match linux::find_holder(inode, SCAN_BUDGET) {
            Ok(pid) => pid,
            Err(reason) => {
                identity.error = Some(reason.to_string());
                return identity;
            }
        };
        identity.pid = Some(pid);
        let fail = |identity: &mut PeerIdentity, reason: &str| {
            identity.error.get_or_insert_with(|| reason.to_string());
        };
        let mut parent = None;
        match linux::status(pid) {
            Ok(status) => {
                identity.uid = Some(status.uid);
                identity.gid = Some(status.gid);
                parent = (status.ppid > 0).then_some(status.ppid);
            }
            Err(reason) => fail(&mut identity, reason),
        }
        match linux::exe(pid) {
            Ok(path) => {
                let text = path_text(&path);
                if text.chars().count() > MAX_PEER_PATH_CHARS {
                    fail(&mut identity, "path_too_long");
                } else {
                    identity.exe = Some(text);
                }
            }
            Err(reason) => fail(&mut identity, reason),
        }
        match self.hash(pid) {
            Ok(Some(hash)) => identity.exe_sha256 = Some(hash),
            Ok(None) => {}
            Err(reason) => fail(&mut identity, reason),
        }
        while let Some(ancestor) = parent {
            if identity.ancestors.len() >= MAX_PEER_ANCESTORS {
                break;
            }
            let Ok(path) = linux::exe(ancestor) else {
                break;
            };
            let text = path_text(&path);
            if text.chars().count() > MAX_PEER_ANCESTOR_CHARS {
                break;
            }
            identity.ancestors.push(text);
            parent = linux::status(ancestor)
                .ok()
                .and_then(|status| (status.ppid > 0).then_some(status.ppid));
        }
        identity
    }

    /// SHA-256 of the process's executable, from the cache when the file is
    /// unchanged. `Ok(None)` for files above [`MAX_HASHED_BYTES`].
    #[cfg(target_os = "linux")]
    fn hash(&self, pid: u32) -> Result<Option<String>, &'static str> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let mut file = std::fs::File::open(format!("/proc/{pid}/exe")).map_err(linux::reason)?;
        let metadata = file.metadata().map_err(linux::reason)?;
        if metadata.len() > MAX_HASHED_BYTES {
            return Ok(None);
        }
        let key = FileKey {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        };
        if let Some(hash) = self
            .hashes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&key)
        {
            return Ok(Some(hash.clone()));
        }
        let mut digest = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let read = file.read(&mut buffer).map_err(|_| "hash_failed")?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > MAX_HASHED_BYTES {
                return Ok(None);
            }
            digest.update(&buffer[..read]);
        }
        let hash = format!("{:x}", digest.finalize());
        let mut hashes = self
            .hashes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if hashes.len() >= MAX_CACHED_HASHES {
            hashes.clear();
        }
        hashes.insert(key, hash.clone());
        Ok(Some(hash))
    }
}

/// The identity line, trimmed until it fits.
pub fn fit_line(mut identity: PeerIdentity) -> Vec<u8> {
    loop {
        if let Ok(line) = identity.line() {
            return line;
        }
        if identity.ancestors.pop().is_some() {
            continue;
        }
        if identity.exe.take().is_some() {
            identity.error.get_or_insert_with(|| "path_too_long".into());
            continue;
        }
        return PeerIdentity::failed("not_found")
            .line()
            .expect("a bare identity fits");
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::MAX_SCANNED_PIDS;

    pub(super) fn reason(error: io::Error) -> &'static str {
        match error.kind() {
            io::ErrorKind::PermissionDenied => "no_access",
            _ => "not_found",
        }
    }

    /// `0100007F:0C38` (IPv4) or 32 hex digits and a port (IPv6), as the
    /// kernel prints them: each 32-bit word in host byte order.
    pub(super) fn parse_address(text: &str) -> Option<SocketAddr> {
        let (address, port) = text.split_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let word = |index: usize| -> Option<[u8; 4]> {
            let digits = address.get(index * 8..index * 8 + 8)?;
            Some(u32::from_str_radix(digits, 16).ok()?.to_ne_bytes())
        };
        let ip = match address.len() {
            8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
            32 => {
                let mut bytes = [0u8; 16];
                for index in 0..4 {
                    bytes[index * 4..index * 4 + 4].copy_from_slice(&word(index)?);
                }
                IpAddr::V6(Ipv6Addr::from(bytes))
            }
            _ => return None,
        };
        Some(SocketAddr::new(ip.to_canonical(), port))
    }

    fn canonical(address: SocketAddr) -> SocketAddr {
        SocketAddr::new(address.ip().to_canonical(), address.port())
    }

    /// The client socket's inode and owner: the line whose local address is
    /// `client` and remote address is `server`.
    pub(super) fn find_socket(
        client: SocketAddr,
        server: SocketAddr,
    ) -> Result<(u64, u32), &'static str> {
        let (client, server) = (canonical(client), canonical(server));
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(table) else {
                continue;
            };
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() < 10 {
                    continue;
                }
                if parse_address(fields[1]) != Some(client)
                    || parse_address(fields[2]) != Some(server)
                {
                    continue;
                }
                let (Ok(uid), Ok(inode)) = (fields[7].parse::<u32>(), fields[9].parse::<u64>())
                else {
                    continue;
                };
                // A closed socket keeps its line with no inode for a while.
                if inode != 0 {
                    return Ok((inode, uid));
                }
            }
        }
        Err("not_found")
    }

    /// The process that holds socket `inode`, found within `budget`.
    pub(super) fn find_holder(inode: u64, budget: Duration) -> Result<u32, &'static str> {
        let deadline = Instant::now() + budget;
        let wanted = format!("socket:[{inode}]");
        let mut denied = false;
        let entries = std::fs::read_dir("/proc").map_err(reason)?;
        let mut pids: Vec<u32> = entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .collect();
        pids.sort_unstable();
        pids.truncate(MAX_SCANNED_PIDS);
        for pid in pids {
            if Instant::now() > deadline {
                return Err("timeout");
            }
            let fds = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
                Ok(fds) => fds,
                Err(error) => {
                    denied |= error.kind() == io::ErrorKind::PermissionDenied;
                    continue;
                }
            };
            for fd in fds.flatten() {
                match std::fs::read_link(fd.path()) {
                    Ok(target) if target.as_os_str() == wanted.as_str() => return Ok(pid),
                    Ok(_) => {}
                    Err(error) => denied |= error.kind() == io::ErrorKind::PermissionDenied,
                }
            }
        }
        Err(if denied { "no_access" } else { "not_found" })
    }

    pub(super) struct Status {
        pub(super) ppid: u32,
        pub(super) uid: u32,
        pub(super) gid: u32,
    }

    /// Parent, effective user and effective group from `/proc/<pid>/status`.
    pub(super) fn status(pid: u32) -> Result<Status, &'static str> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(reason)?;
        let field = |name: &str, index: usize| -> Option<u32> {
            text.lines()
                .find_map(|line| line.strip_prefix(name))?
                .split_whitespace()
                .nth(index)?
                .parse()
                .ok()
        };
        Ok(Status {
            ppid: field("PPid:", 0).ok_or("not_found")?,
            uid: field("Uid:", 1).ok_or("not_found")?,
            gid: field("Gid:", 1).ok_or("not_found")?,
        })
    }

    pub(super) fn exe(pid: u32) -> Result<PathBuf, &'static str> {
        std::fs::read_link(format!("/proc/{pid}/exe")).map_err(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{MAX_PEER_ANCESTOR_CHARS, MAX_PEER_PATH_CHARS};
    use super::*;

    #[test]
    fn identities_too_long_for_a_line_are_trimmed() {
        let long = PeerIdentity {
            pid: Some(7),
            uid: Some(1000),
            gid: Some(1000),
            exe: Some(format!("/{}", "\u{e9}".repeat(MAX_PEER_PATH_CHARS - 1))),
            exe_sha256: None,
            ancestors: vec![format!("/{}", "\u{e9}".repeat(MAX_PEER_ANCESTOR_CHARS - 1)); 8],
            error: None,
        };
        let line = fit_line(long.clone());
        assert!(line.len() <= super::super::protocol::MAX_PEER_LINE_BYTES);
        let parsed = PeerIdentity::parse_line(&line[..line.len() - 2]).unwrap();
        assert_eq!(parsed.pid, Some(7));
        assert!(parsed.ancestors.len() < 8);
        let short = PeerIdentity {
            ancestors: Vec::new(),
            ..long
        };
        let parsed = fit_line(short.clone());
        assert_eq!(
            PeerIdentity::parse_line(&parsed[..parsed.len() - 2]).unwrap(),
            short
        );
        assert_eq!(
            path_text(std::path::Path::new("/tmp/a\nb")),
            "/tmp/a\u{fffd}b"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn other_systems_report_unsupported() {
        let identity = PeerLookup::new().identify(
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        );
        assert_eq!(identity.error.as_deref(), Some("unsupported"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_addresses_parse_in_host_byte_order() {
        assert_eq!(
            linux::parse_address("0100007F:0C38"),
            Some("127.0.0.1:3128".parse().unwrap())
        );
        assert_eq!(
            linux::parse_address("00000000000000000000000001000000:1F90"),
            Some("[::1]:8080".parse().unwrap())
        );
        // A mapped IPv4 address in tcp6 compares as IPv4.
        assert_eq!(
            linux::parse_address("0000000000000000FFFF00000100007F:0C38"),
            Some("127.0.0.1:3128".parse().unwrap())
        );
        assert_eq!(linux::parse_address("XYZ:0C38"), None);
        assert_eq!(linux::parse_address("0100007F"), None);
    }

    /// The test process's own connection: it holds the client socket.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_connection_names_its_own_process() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, peer) = listener.accept().unwrap();
        let lookup = PeerLookup::new();
        let identity = lookup.identify(peer, accepted.local_addr().unwrap());
        assert_eq!(identity.error, None, "{identity:?}");
        assert_eq!(identity.pid, Some(std::process::id()));
        // SAFETY: getuid and getgid have no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert_eq!((identity.uid, identity.gid), (Some(uid), Some(gid)));
        let exe = std::fs::read_link("/proc/self/exe").unwrap();
        assert_eq!(identity.exe.as_deref(), exe.to_str());
        let bytes = std::fs::read(&exe).unwrap();
        use sha2::Digest;
        assert_eq!(
            identity.exe_sha256,
            Some(format!("{:x}", sha2::Sha256::digest(&bytes)))
        );
        // Cached the second time, same answer.
        assert_eq!(
            lookup.identify(peer, accepted.local_addr().unwrap()),
            identity
        );
        drop(client);
    }
}
