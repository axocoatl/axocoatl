//! Control protocol between the egress proxy (sidecar) and the daemon.
//!
//! JSON lines on the sidecar's stdin (daemon frames) and stdout (sidecar
//! frames). The sidecar never decides: it reports each request's host, port
//! and credential hash, and connects only to addresses the daemon returns.
//! The raw credential never crosses this channel.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;

pub const EGRESS_PROTOCOL_VERSION: u32 = 1;
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

impl SidecarFrame {
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
            }
            Self::Close { error, .. } => error
                .as_deref()
                .is_none_or(|error| bounded(error, MAX_DETAIL_CHARS)),
            Self::Pong => true,
            Self::Fatal { detail } => bounded(detail, MAX_DETAIL_CHARS),
        };
        ok.then_some(())
            .ok_or_else(|| "egress sidecar frame is out of bounds".to_string())
    }
}

impl DaemonFrame {
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
                protocol: 1,
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
            },
            SidecarFrame::Open {
                id: 8,
                kind: RequestKind::Connect,
                host: "[2606:4700::1]".into(),
                port: 443,
                auth: None,
                method: None,
                path: None,
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
            DaemonFrame::HelloAck { protocol: 1 },
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
            serde_json::from_slice(&encode_daemon(&DaemonFrame::HelloAck { protocol: 1 }).unwrap())
                .unwrap();
        assert_eq!(wire, serde_json::json!({"t": "hello_ack", "protocol": 1}));
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
            r#"{"t":"hello_ack","protocol":2}"#,
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
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.example".into(),
                port: 0,
                auth: None,
                method: None,
                path: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.example".into(),
                port: 443,
                auth: Some("axe_raw_token_never_sent".into()),
                method: None,
                path: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Connect,
                host: "a.example".into(),
                port: 443,
                auth: None,
                method: Some("GET".into()),
                path: None,
            },
            SidecarFrame::Open {
                id: 1,
                kind: RequestKind::Http,
                host: "a.example".into(),
                port: 80,
                auth: None,
                method: Some("GET".into()),
                path: Some("/".repeat(MAX_PATH_CHARS + 1)),
            },
            SidecarFrame::Hello {
                protocol: 1,
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
}
