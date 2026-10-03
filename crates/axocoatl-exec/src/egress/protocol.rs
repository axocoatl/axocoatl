//! Control protocol between the egress proxy (sidecar) and the daemon.
//!
//! JSON lines on the sidecar's stdin (daemon frames) and stdout (sidecar
//! frames). The sidecar never decides: it reports each request's host, port
//! and credential hash, and connects only to addresses the daemon returns.
//! The raw credential never crosses this channel.
//!
//! Version 2 adds relayed connections. For a `connect` request the daemon may
//! answer `relay` instead of `allow`: the sidecar then answers the client
//! `200 Connection Established` and carries the connection's bytes over this
//! channel in `data` frames (base64, at most [`MAX_DATA_BYTES`] each), with
//! `eof` for a half-close. Each direction starts with a window of
//! [`RELAY_WINDOW_BYTES`] and needs `credit` from the receiver to send more,
//! so neither side buffers more than one window per connection. Both sides
//! write control frames before queued `data` and `eof` frames.
//!
//! Version 2 also carries the identity of the program behind a connection
//! (`open.peer`) when the request arrived on the proxy's identity socket, where
//! the Session container's init process writes it ([`PeerIdentity`]).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;

pub const EGRESS_PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME_BYTES: usize = 65_536;
pub const MAX_HEAD_BYTES: usize = 16_384;
pub const MAX_PATH_CHARS: usize = 2048;
pub const MAX_HOST_CHARS: usize = 253;
pub const MAX_ALLOW_ADDRS: usize = 16;
pub const MAX_REASON_CHARS: usize = 64;
pub const MAX_HINT_CHARS: usize = 1024;
pub const MAX_DETAIL_CHARS: usize = 1024;
pub const MAX_REVOKE_IDS: usize = 1024;
pub const DECISION_TIMEOUT_MS: u64 = 10_000;
pub const HEARTBEAT_MS: u64 = 5_000;
pub const HEARTBEAT_DEAD_MS: u64 = 20_000;
pub const HEAD_TIMEOUT_MS: u64 = 10_000;
pub const CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const IDLE_TIMEOUT_MS: u64 = 15 * 60 * 1000;
pub const DEFAULT_MAX_CONNECTIONS: u32 = 128;
pub const MAX_MAX_CONNECTIONS: u32 = 256;
/// Statuses a daemon `deny` may carry.
pub const DENY_STATUSES: [u16; 5] = [400, 403, 407, 502, 503];
/// Bytes each direction of a relayed connection may send before the receiver
/// grants more with `credit`.
pub const RELAY_WINDOW_BYTES: u32 = 262_144;
/// Most raw bytes one `data` frame carries.
pub const MAX_DATA_BYTES: usize = 32_768;
/// Most relayed connections one sidecar carries at once; past it a relay is
/// answered 503 `relay_capacity`.
pub const MAX_RELAYS: usize = 32;
/// Longest program path in a [`PeerIdentity`].
pub const MAX_PEER_PATH_CHARS: usize = 1024;
/// Longest ancestor path in a [`PeerIdentity`].
pub const MAX_PEER_ANCESTOR_CHARS: usize = 256;
/// Most ancestors a [`PeerIdentity`] names.
pub const MAX_PEER_ANCESTORS: usize = 8;
/// Longest identity line, `AXO-PEER/1 ` and CRLF included.
pub const MAX_PEER_LINE_BYTES: usize = 4096;
/// How long the proxy waits for the identity line on its identity socket.
pub const PEER_LINE_TIMEOUT_MS: u64 = 2_000;
/// What starts the identity line the bridge writes before a client's bytes.
pub const PEER_LINE_PREFIX: &[u8] = b"AXO-PEER/1 ";

/// How the client asked to go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// `CONNECT host:port`, an opaque tunnel (HTTPS and anything else).
    Connect,
    /// A plain-HTTP absolute-form request, one per connection.
    Http,
}

/// How an allowed connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseOutcome {
    Closed,
    Reset,
    UpstreamFailed,
    Interrupted,
    Revoked,
    IdleTimeout,
}

/// The program behind one proxied connection, as the Session container's init
/// process found it: the process holding the client's socket, its executable
/// and that file's SHA-256, its user and group, and up to
/// [`MAX_PEER_ANCESTORS`] parent executables (nearest first). What could not
/// be found is left out and `error` says why (`no_access`, `not_found`,
/// `timeout`, `unsupported`, `path_too_long`, `hash_failed`, or
/// `foreign_namespace` for a process in another mount namespace or under
/// another root, whose path is left out because it may name another file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// Lowercase hex SHA-256 of the executable's contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PeerIdentity {
    /// An identity that only says why it is missing.
    pub fn failed(reason: &str) -> Self {
        Self {
            error: Some(reason.to_string()),
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let ok = self
            .exe
            .as_deref()
            .is_none_or(|exe| !exe.is_empty() && bounded(exe, MAX_PEER_PATH_CHARS))
            && self.exe_sha256.as_deref().is_none_or(is_digest)
            && self.ancestors.len() <= MAX_PEER_ANCESTORS
            && self
                .ancestors
                .iter()
                .all(|path| bounded(path, MAX_PEER_ANCESTOR_CHARS))
            && self.error.as_deref().is_none_or(is_reason_code);
        ok.then_some(())
            .ok_or_else(|| "peer identity is out of bounds".to_string())
    }

    /// The line the bridge writes before a client's bytes:
    /// `AXO-PEER/1 {json}` and CRLF, at most [`MAX_PEER_LINE_BYTES`].
    pub fn line(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let mut line = PEER_LINE_PREFIX.to_vec();
        line.extend(serde_json::to_vec(self).map_err(|error| error.to_string())?);
        line.extend_from_slice(b"\r\n");
        if line.len() > MAX_PEER_LINE_BYTES {
            return Err("peer identity line exceeds 4 KiB".into());
        }
        Ok(line)
    }

    /// Parse an identity line without its CRLF.
    pub fn parse_line(line: &[u8]) -> Result<Self, String> {
        if line.len() + 2 > MAX_PEER_LINE_BYTES {
            return Err("peer identity line exceeds 4 KiB".into());
        }
        let json = line
            .strip_prefix(PEER_LINE_PREFIX)
            .ok_or("not a peer identity line")?;
        let identity: Self = serde_json::from_slice(json).map_err(|error| error.to_string())?;
        identity.validate()?;
        Ok(identity)
    }
}

/// Frames the sidecar writes on stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum SidecarFrame {
    Hello {
        protocol: u32,
        version: String,
        max_connections: u32,
    },
    Open {
        id: u64,
        kind: RequestKind,
        /// The host as the client wrote it (a name or an IP literal).
        host: String,
        port: u16,
        /// Lowercase hex SHA-256 of the presented credential.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        /// Request path without its query, for `http` requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The program behind the connection, when it came through the
        /// identity socket.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<PeerIdentity>,
    },
    Close {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ip: Option<IpAddr>,
        up: u64,
        down: u64,
        ms: u64,
        outcome: CloseOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Client bytes of a relayed connection, base64.
    Data {
        id: u64,
        b: String,
    },
    /// The client closed its sending side of a relayed connection.
    Eof {
        id: u64,
    },
    /// The client took this many more of the daemon's bytes.
    Credit {
        id: u64,
        bytes: u32,
    },
    Pong,
    Fatal {
        detail: String,
    },
}

/// Frames the daemon writes on the sidecar's stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum DaemonFrame {
    HelloAck {
        protocol: u32,
    },
    Allow {
        id: u64,
        addrs: Vec<IpAddr>,
    },
    Deny {
        id: u64,
        status: u16,
        reason: String,
        hint: String,
    },
    Revoke {
        ids: Vec<u64>,
    },
    /// Answer a `connect` request with `200 Connection Established` and carry
    /// its bytes over this channel.
    Relay {
        id: u64,
    },
    /// Bytes for the client of a relayed connection, base64.
    Data {
        id: u64,
        b: String,
    },
    /// Nothing more for the client: half-close its receiving side.
    Eof {
        id: u64,
    },
    /// The daemon took this many more of the client's bytes.
    Credit {
        id: u64,
        bytes: u32,
    },
    Ping,
    Shutdown,
}

fn bounded(value: &str, max_chars: usize) -> bool {
    value.chars().count() <= max_chars && !value.chars().any(char::is_control)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A reason is a short snake_case code such as `not_allowed`.
pub fn is_reason_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REASON_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_credit(bytes: u32) -> bool {
    (1..=RELAY_WINDOW_BYTES).contains(&bytes)
}

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            out.push(if index <= chunk.len() {
                TABLE[((value >> (18 - index * 6)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The bytes of a `data` frame: 1 to [`MAX_DATA_BYTES`] of them.
pub fn decode_data(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.is_empty() || encoded.len() > MAX_DATA_BYTES.div_ceil(3) * 4 {
        return Err("relay data frame is empty or larger than 32 KiB".into());
    }
    let bytes = super::http::base64_decode(encoded).ok_or("relay data is not base64")?;
    if bytes.is_empty() || bytes.len() > MAX_DATA_BYTES {
        return Err("relay data frame is empty or larger than 32 KiB".into());
    }
    Ok(bytes)
}

impl SidecarFrame {
    /// Frames that belong to one relayed connection's byte stream and must
    /// stay in order with it. Everything else is a control frame.
    pub fn is_stream(&self) -> bool {
        matches!(self, Self::Data { .. } | Self::Eof { .. })
    }

    pub fn validate(&self) -> Result<(), String> {
        let ok = match self {
            Self::Hello {
                protocol,
                version,
                max_connections,
            } => {
                *protocol == EGRESS_PROTOCOL_VERSION
                    && bounded(version, 64)
                    && (1..=MAX_MAX_CONNECTIONS).contains(max_connections)
            }
            Self::Open {
                host,
                port,
                auth,
                method,
                path,
                kind,
                peer,
                ..
            } => {
                !host.is_empty()
                    && bounded(host, MAX_HOST_CHARS + 2)
                    && *port != 0
                    && auth.as_deref().is_none_or(is_digest)
                    && method.as_deref().is_none_or(|method| bounded(method, 32))
                    && path
                        .as_deref()
                        .is_none_or(|path| bounded(path, MAX_PATH_CHARS))
                    && (*kind == RequestKind::Http || (method.is_none() && path.is_none()))
                    && peer.as_ref().is_none_or(|peer| peer.validate().is_ok())
            }
            Self::Close { error, .. } => error
                .as_deref()
                .is_none_or(|error| bounded(error, MAX_DETAIL_CHARS)),
            Self::Data { b, .. } => decode_data(b).is_ok(),
            Self::Eof { .. } => true,
            Self::Credit { bytes, .. } => valid_credit(*bytes),
            Self::Pong => true,
            Self::Fatal { detail } => bounded(detail, MAX_DETAIL_CHARS),
        };
        ok.then_some(())
            .ok_or_else(|| "egress sidecar frame is out of bounds".to_string())
    }
}

impl DaemonFrame {
    /// Frames that belong to one relayed connection's byte stream and must
    /// stay in order with it. Everything else is a control frame.
    pub fn is_stream(&self) -> bool {
        matches!(self, Self::Data { .. } | Self::Eof { .. })
    }

    pub fn validate(&self) -> Result<(), String> {
        let ok = match self {
            Self::HelloAck { protocol } => *protocol == EGRESS_PROTOCOL_VERSION,
            Self::Allow { addrs, .. } => (1..=MAX_ALLOW_ADDRS).contains(&addrs.len()),
            Self::Deny {
                status,
                reason,
                hint,
                ..
            } => {
                DENY_STATUSES.contains(status)
                    && is_reason_code(reason)
                    && bounded(hint, MAX_HINT_CHARS)
            }
            Self::Revoke { ids } => !ids.is_empty() && ids.len() <= MAX_REVOKE_IDS,
            Self::Data { b, .. } => decode_data(b).is_ok(),
            Self::Relay { .. } | Self::Eof { .. } => true,
            Self::Credit { bytes, .. } => valid_credit(*bytes),
            Self::Ping | Self::Shutdown => true,
        };
        ok.then_some(())
            .ok_or_else(|| "egress daemon frame is out of bounds".to_string())
    }
}

/// One frame as a JSON line, newline included. Refuses invalid or oversized
/// frames.
pub fn encode<T: Serialize>(frame: &T) -> Result<Vec<u8>, String> {
    let mut line = serde_json::to_vec(frame).map_err(|error| error.to_string())?;
    line.push(b'\n');
    if line.len() > MAX_FRAME_BYTES {
        return Err("egress control frame exceeds 64 KiB".into());
    }
    Ok(line)
}

pub fn encode_sidecar(frame: &SidecarFrame) -> Result<Vec<u8>, String> {
    frame.validate()?;
    encode(frame)
}

pub fn encode_daemon(frame: &DaemonFrame) -> Result<Vec<u8>, String> {
    frame.validate()?;
    encode(frame)
}

/// Decode one line (with or without its newline).
pub fn decode_sidecar(line: &[u8]) -> Result<SidecarFrame, String> {
    if line.len() > MAX_FRAME_BYTES {
        return Err("egress control frame exceeds 64 KiB".into());
    }
    let frame: SidecarFrame = serde_json::from_slice(line).map_err(|error| error.to_string())?;
    frame.validate()?;
    Ok(frame)
}

pub fn decode_daemon(line: &[u8]) -> Result<DaemonFrame, String> {
    if line.len() > MAX_FRAME_BYTES {
        return Err("egress control frame exceeds 64 KiB".into());
    }
    let frame: DaemonFrame = serde_json::from_slice(line).map_err(|error| error.to_string())?;
    frame.validate()?;
    Ok(frame)
}

/// The credential hash the sidecar reports: lowercase hex SHA-256.
pub fn credential_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// First 16 hex of the credential hash: the tag records and logs show.
pub fn credential_tag(hash: &str) -> String {
    hash.chars().take(16).collect()
}

/// Read one newline-terminated frame of at most `MAX_FRAME_BYTES`. `Ok(None)`
/// at end of input; an oversized or unterminated final line is an error.
pub fn read_frame(reader: &mut impl std::io::BufRead) -> Result<Option<Vec<u8>>, String> {
    use std::io::BufRead;
    let mut line = Vec::new();
    let read = std::io::Read::take(&mut *reader, MAX_FRAME_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|error| error.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_FRAME_BYTES {
            "egress control frame exceeds 64 KiB".into()
        } else {
            "egress control channel ended inside a frame".into()
        });
    }
    line.pop();
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        credential_hash("axe_secret")
    }

    #[test]
    fn frames_round_trip_with_their_wire_names() {
        let sidecar = [
            SidecarFrame::Hello {
                protocol: 2,
                version: "1.1.2".into(),
                max_connections: 128,
            },
            SidecarFrame::Open {
                id: 7,
                kind: RequestKind::Http,
                host: "registry.npmjs.org".into(),
                port: 80,
                auth: Some(digest()),
                method: Some("GET".into()),
                path: Some("/left-pad".into()),
                peer: None,
            },
            SidecarFrame::Open {
                id: 8,
                kind: RequestKind::Connect,
                host: "[2606:4700::1]".into(),
                port: 443,
                auth: None,
                method: None,
                path: None,
                peer: None,
            },
            SidecarFrame::Close {
                id: 7,
                ip: Some("104.16.0.1".parse().unwrap()),
                up: 10,
                down: 20,
                ms: 30,
                outcome: CloseOutcome::IdleTimeout,
                error: None,
            },
            SidecarFrame::Pong,
            SidecarFrame::Fatal {
                detail: "listener failed".into(),
            },
        ];
        for frame in sidecar {
            let line = encode_sidecar(&frame).unwrap();
            assert!(line.ends_with(b"\n"));
            assert_eq!(decode_sidecar(&line[..line.len() - 1]).unwrap(), frame);
        }
        let daemon = [
            DaemonFrame::HelloAck { protocol: 2 },
            DaemonFrame::Allow {
                id: 7,
                addrs: vec![
                    "104.16.0.1".parse().unwrap(),
                    "2606:4700::1".parse().unwrap(),
                ],
            },
            DaemonFrame::Deny {
                id: 8,
                status: 407,
                reason: "no_credential".into(),
                hint: "This process has no egress credential.".into(),
            },
            DaemonFrame::Revoke { ids: vec![1, 2] },
            DaemonFrame::Ping,
            DaemonFrame::Shutdown,
        ];
        for frame in daemon {
            let line = encode_daemon(&frame).unwrap();
            assert_eq!(decode_daemon(&line).unwrap(), frame);
        }
        let wire: serde_json::Value =
            serde_json::from_slice(&encode_sidecar(&SidecarFrame::Pong).unwrap()).unwrap();
        assert_eq!(wire, serde_json::json!({"t": "pong"}));
        let wire: serde_json::Value =
            serde_json::from_slice(&encode_daemon(&DaemonFrame::HelloAck { protocol: 2 }).unwrap())
                .unwrap();
        assert_eq!(wire, serde_json::json!({"t": "hello_ack", "protocol": 2}));
    }

    #[test]
    fn invalid_frames_are_refused() {
        for line in [
            r#"{"t":"allow","id":1,"addrs":[]}"#,
            r#"{"t":"allow","id":1,"addrs":["1.1.1.1"],"extra":1}"#,
            r#"{"t":"allow","id":1,"addrs":["not-an-ip"]}"#,
            r#"{"t":"deny","id":1,"status":200,"reason":"ok","hint":""}"#,
            r#"{"t":"deny","id":1,"status":403,"reason":"Not Allowed","hint":""}"#,
            r#"{"t":"deny","id":1,"status":403,"reason":"x","hint":"line\nbreak"}"#,
            r#"{"t":"revoke","ids":[]}"#,
            r#"{"t":"hello_ack","protocol":1}"#,
            r#"{"t":"launch","argv":["sh"]}"#,
            r#"{"kind":"ping"}"#,
            "not json",
        ] {
            assert!(decode_daemon(line.as_bytes()).is_err(), "{line}");
        }
        let addrs = vec!["1.1.1.1".parse().unwrap(); 17];
        assert!(encode_daemon(&DaemonFrame::Allow { id: 1, addrs }).is_err());
        for frame in [
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: String::new(),
                port: 443,
                auth: None,
                method: None,
                path: None,
                peer: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.example".into(),
                port: 0,
                auth: None,
                method: None,
                path: None,
                peer: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.example".into(),
                port: 443,
                auth: Some("axe_raw_token_never_sent".into()),
                method: None,
                path: None,
                peer: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.example".into(),
                port: 443,
                auth: None,
                method: Some("GET".into()),
                path: None,
                peer: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Http,
                host: "a.example".into(),
                port: 80,
                auth: None,
                method: Some("GET".into()),
                path: Some("/".repeat(MAX_PATH_CHARS + 1)),
                peer: None,
            },
            SidecarFrame::Hello {
                protocol: 2,
                version: "x".into(),
                max_connections: 0,
            },
        ] {
            assert!(encode_sidecar(&frame).is_err(), "{frame:?}");
        }
    }

    #[test]
    fn frame_size_is_bounded() {
        let huge = DaemonFrame::Deny {
            id: 1,
            status: 403,
            reason: "not_allowed".into(),
            hint: "x".repeat(MAX_FRAME_BYTES),
        };
        assert!(encode(&huge).is_err());
        assert!(decode_daemon(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());

        let mut input = std::io::Cursor::new(b"{\"t\":\"ping\"}\n{\"t\":\"shutdown\"}".to_vec());
        assert_eq!(
            read_frame(&mut input).unwrap().as_deref(),
            Some(&b"{\"t\":\"ping\"}"[..])
        );
        assert!(read_frame(&mut input)
            .unwrap_err()
            .contains("inside a frame"));
        let mut oversized = std::io::Cursor::new(vec![b'a'; MAX_FRAME_BYTES + 10]);
        assert!(read_frame(&mut oversized).unwrap_err().contains("64 KiB"));
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(read_frame(&mut empty).unwrap(), None);
    }

    #[test]
    fn credentials_cross_only_as_hashes_and_tags() {
        let hash = credential_hash("axe_secret");
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains("secret"));
        assert_eq!(credential_tag(&hash), &hash[..16]);
        assert!(is_reason_code("not_allowed"));
        assert!(!is_reason_code("Not allowed"));
        assert!(!is_reason_code(""));
    }

    #[test]
    fn relay_frames_round_trip_and_are_bounded() {
        let full = vec![0xa5u8; MAX_DATA_BYTES];
        let encoded = base64_encode(&full);
        assert_eq!(decode_data(&encoded).unwrap(), full);
        let largest = encode_sidecar(&SidecarFrame::Data {
            id: u64::MAX,
            b: encoded.clone(),
        })
        .unwrap();
        assert!(largest.len() < MAX_FRAME_BYTES, "{}", largest.len());
        for frame in [
            SidecarFrame::Data {
                id: 3,
                b: base64_encode(b"client bytes"),
            },
            SidecarFrame::Eof { id: 3 },
            SidecarFrame::Credit {
                id: 3,
                bytes: RELAY_WINDOW_BYTES,
            },
        ] {
            let line = encode_sidecar(&frame).unwrap();
            assert_eq!(decode_sidecar(&line).unwrap(), frame);
        }
        for frame in [
            DaemonFrame::Relay { id: 3 },
            DaemonFrame::Data {
                id: 3,
                b: encoded.clone(),
            },
            DaemonFrame::Eof { id: 3 },
            DaemonFrame::Credit { id: 3, bytes: 1 },
        ] {
            let line = encode_daemon(&frame).unwrap();
            assert_eq!(decode_daemon(&line).unwrap(), frame);
        }
        let wire: serde_json::Value =
            serde_json::from_slice(&encode_daemon(&DaemonFrame::Relay { id: 9 }).unwrap()).unwrap();
        assert_eq!(wire, serde_json::json!({"t": "relay", "id": 9}));
        assert!(DaemonFrame::Data {
            id: 1,
            b: "AA==".into()
        }
        .is_stream());
        assert!(DaemonFrame::Eof { id: 1 }.is_stream());
        assert!(!DaemonFrame::Credit { id: 1, bytes: 1 }.is_stream());
        assert!(!DaemonFrame::Relay { id: 1 }.is_stream());
        assert!(SidecarFrame::Eof { id: 1 }.is_stream());
        assert!(!SidecarFrame::Pong.is_stream());

        let oversized = base64_encode(&vec![0u8; MAX_DATA_BYTES + 1]);
        for line in [
            format!(r#"{{"t":"data","id":1,"b":"{oversized}"}}"#),
            r#"{"t":"data","id":1,"b":""}"#.to_string(),
            r#"{"t":"data","id":1,"b":"!!!!"}"#.to_string(),
            r#"{"t":"data","id":1}"#.to_string(),
            r#"{"t":"credit","id":1,"bytes":0}"#.to_string(),
            format!(
                r#"{{"t":"credit","id":1,"bytes":{}}}"#,
                RELAY_WINDOW_BYTES + 1
            ),
            r#"{"t":"relay","id":1,"addrs":["1.1.1.1"]}"#.to_string(),
            r#"{"t":"eof"}"#.to_string(),
        ] {
            assert!(decode_daemon(line.as_bytes()).is_err(), "{line}");
            assert!(decode_sidecar(line.as_bytes()).is_err(), "{line}");
        }
        // Only the daemon relays.
        assert!(decode_sidecar(br#"{"t":"relay","id":1}"#).is_err());
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn peer_identities_travel_on_one_bounded_line() {
        let identity = PeerIdentity {
            pid: Some(4242),
            uid: Some(1000),
            gid: Some(1000),
            exe: Some("/usr/bin/curl".into()),
            exe_sha256: Some(digest()),
            ancestors: vec!["/usr/bin/bash".into(), "/axocoatl-exec-supervisor".into()],
            error: None,
        };
        let line = identity.line().unwrap();
        assert!(line.starts_with(PEER_LINE_PREFIX) && line.ends_with(b"\r\n"));
        assert_eq!(
            PeerIdentity::parse_line(&line[..line.len() - 2]).unwrap(),
            identity
        );
        let failed = PeerIdentity::failed("no_access");
        assert_eq!(
            String::from_utf8(failed.line().unwrap()).unwrap(),
            "AXO-PEER/1 {\"error\":\"no_access\"}\r\n"
        );
        let open = SidecarFrame::Open {
            id: 1,
            kind: RequestKind::Connect,
            host: "registry.npmjs.org".into(),
            port: 443,
            auth: Some(digest()),
            method: None,
            path: None,
            peer: Some(identity.clone()),
        };
        assert_eq!(
            decode_sidecar(&encode_sidecar(&open).unwrap()).unwrap(),
            open
        );
        for broken in [
            PeerIdentity {
                exe: Some(String::new()),
                ..identity.clone()
            },
            PeerIdentity {
                exe: Some("/".repeat(MAX_PEER_PATH_CHARS + 1)),
                ..identity.clone()
            },
            PeerIdentity {
                exe: Some("/usr/bin/cu\nrl".into()),
                ..identity.clone()
            },
            PeerIdentity {
                exe_sha256: Some("not-a-digest".into()),
                ..identity.clone()
            },
            PeerIdentity {
                ancestors: vec!["/bin/sh".into(); MAX_PEER_ANCESTORS + 1],
                ..identity.clone()
            },
            PeerIdentity {
                ancestors: vec!["/".repeat(MAX_PEER_ANCESTOR_CHARS + 1)],
                ..identity.clone()
            },
            PeerIdentity {
                error: Some("Not A Code".into()),
                ..identity.clone()
            },
        ] {
            assert!(broken.line().is_err(), "{broken:?}");
            let open = SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.test".into(),
                port: 443,
                auth: None,
                method: None,
                path: None,
                peer: Some(broken),
            };
            assert!(encode_sidecar(&open).is_err());
        }
        for line in [
            &b"AXO-PEER/2 {}"[..],
            b"AXO-PEER/1 ",
            b"AXO-PEER/1 {\"pid\":-1}",
            b"AXO-PEER/1 {\"shell\":\"sh\"}",
            b"CONNECT a.test:443 HTTP/1.1",
        ] {
            assert!(PeerIdentity::parse_line(line).is_err(), "{line:?}");
        }
        assert!(PeerIdentity::parse_line(&vec![b'A'; MAX_PEER_LINE_BYTES]).is_err());
    }
}
