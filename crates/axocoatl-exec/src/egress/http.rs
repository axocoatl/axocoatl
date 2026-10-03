//! Pure parser for the egress proxy's request head, and the refusal responses.
//!
//! Accepted: `CONNECT host:port` (a tunnel) and absolute-form
//! `METHOD http://host[:port]/path` (one plain-HTTP request per connection).
//! Everything else, including `https://` absolute-form, origin-form and `*`,
//! is refused with 400. The credential is the password of
//! `Proxy-Authorization: Basic base64(user:token)` and leaves this module only
//! as its SHA-256.

use super::protocol::{
    credential_hash, RequestKind, MAX_HEAD_BYTES, MAX_HOST_CHARS, MAX_PATH_CHARS,
};

/// One parsed proxy request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRequest {
    pub kind: RequestKind,
    /// The host as written: a name, an IPv4 literal, or `[v6]`.
    pub host: String,
    pub port: u16,
    /// SHA-256 hex of the presented credential.
    pub auth: Option<String>,
    /// `http` only.
    pub method: Option<String>,
    /// `http` only: the path without its query, at most 2048 characters.
    pub path: Option<String>,
    /// `http` only: the origin-form head to send upstream.
    pub upstream_head: Option<Vec<u8>>,
}

/// Why a head was refused. Each maps to a 400 reason code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    TooLarge,
    BadRequest,
    HostMismatch,
    HttpsAbsoluteForm,
    NotAProxyRequest,
}

impl HeadError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::TooLarge => "head_too_large",
            Self::BadRequest => "bad_request",
            Self::HostMismatch => "host_mismatch",
            Self::HttpsAbsoluteForm => "https_absolute_form",
            Self::NotAProxyRequest => "not_a_proxy_request",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::TooLarge => "The request head is larger than 16 KiB.",
            Self::BadRequest => {
                "Axocoatl's egress proxy accepts CONNECT host:port and absolute-form http:// requests."
            }
            Self::HostMismatch => "The Host header must match the request URL's host and port.",
            Self::HttpsAbsoluteForm => {
                "Send HTTPS through the proxy with CONNECT, as clients using HTTPS_PROXY do."
            }
            Self::NotAProxyRequest => {
                "This is Axocoatl's egress proxy; send requests with an absolute http:// URL or CONNECT."
            }
        }
    }
}

/// The index just past the head's terminating blank line, if present.
pub fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn is_host_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
}

/// Split `host[:port]` or `[v6][:port]`. With `default_port` absent the port
/// is required.
fn parse_authority(authority: &str, default_port: Option<u16>) -> Result<(String, u16), HeadError> {
    if authority.is_empty() || authority.contains('@') {
        return Err(HeadError::BadRequest);
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']').ok_or(HeadError::BadRequest)?;
        let inner = &rest[..close];
        if inner.contains('%') || inner.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(HeadError::BadRequest);
        }
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port),
            None if after.is_empty() => None,
            None => return Err(HeadError::BadRequest),
        };
        (format!("[{inner}]"), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty()
        || host.len() > MAX_HOST_CHARS + 2
        || (!host.starts_with('[') && !host.bytes().all(is_host_byte))
    {
        return Err(HeadError::BadRequest);
    }
    let port = match port {
        Some(port) => {
            if port.is_empty()
                || port.len() > 5
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || (port.len() > 1 && port.starts_with('0'))
            {
                return Err(HeadError::BadRequest);
            }
            let port: u32 = port.parse().map_err(|_| HeadError::BadRequest)?;
            if port == 0 || port > 65_535 {
                return Err(HeadError::BadRequest);
            }
            port as u16
        }
        None => default_port.ok_or(HeadError::BadRequest)?,
    };
    Ok((host, port))
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64 with optional padding.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim_end_matches('=');
    if text.len() - trimmed.len() > 2 || trimmed.len() % 4 == 1 {
        return None;
    }
    if text.len() != trimmed.len() && !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in trimmed.bytes() {
        accumulator = (accumulator << 6) | u32::from(base64_value(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero padding, so each value has one spelling.
    (accumulator == 0).then_some(out)
}

/// The token from `Basic base64(user:token)`. Anything else is no credential.
pub fn basic_credential(value: &str) -> Option<String> {
    let value = value.trim();
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(base64_decode(encoded.trim())?).ok()?;
    let (_, token) = decoded.split_once(':')?;
    (!token.is_empty()).then(|| token.to_string())
}

const DROPPED_UPSTREAM_HEADERS: [&str; 4] = [
    "proxy-authorization",
    "proxy-connection",
    "connection",
    "keep-alive",
];

/// Parse a complete head, including its final blank line.
pub fn parse_head(head: &[u8]) -> Result<ProxyRequest, HeadError> {
    if head.len() > MAX_HEAD_BYTES {
        return Err(HeadError::TooLarge);
    }
    let body = head
        .strip_suffix(b"\r\n\r\n")
        .ok_or(HeadError::BadRequest)?;
    // Lines end in CRLF; a bare CR or LF anywhere else is refused.
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < body.len() {
        match body[index] {
            b'\r' if body.get(index + 1) == Some(&b'\n') => {
                lines.push(&body[start..index]);
                index += 2;
                start = index;
            }
            b'\r' | b'\n' => return Err(HeadError::BadRequest),
            _ => index += 1,
        }
    }
    lines.push(&body[start..]);
    let request_line = std::str::from_utf8(lines[0]).map_err(|_| HeadError::BadRequest)?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HeadError::BadRequest);
    };
    if method.is_empty()
        || method.len() > 32
        || !method.bytes().all(is_token_byte)
        || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
        || target.is_empty()
        || target
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(HeadError::BadRequest);
    }
    let mut headers: Vec<(String, &[u8])> = Vec::new();
    for line in &lines[1..] {
        if line.is_empty() {
            return Err(HeadError::BadRequest);
        }
        if line[0] == b' ' || line[0] == b'\t' {
            // Obsolete line folding.
            return Err(HeadError::BadRequest);
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(HeadError::BadRequest)?;
        let name = &line[..colon];
        if name.is_empty() || !name.iter().all(|byte| is_token_byte(*byte)) {
            return Err(HeadError::BadRequest);
        }
        let value = &line[colon + 1..];
        if value
            .iter()
            .any(|byte| (byte.is_ascii_control() && *byte != b'\t') || *byte == 0x7f)
        {
            return Err(HeadError::BadRequest);
        }
        headers.push((String::from_utf8_lossy(name).to_ascii_lowercase(), *line));
    }
    let header_value = |line: &[u8]| -> String {
        let colon = line.iter().position(|byte| *byte == b':').unwrap_or(0);
        String::from_utf8_lossy(&line[colon + 1..])
            .trim()
            .to_string()
    };
    let auth = headers
        .iter()
        .find(|(name, _)| name == "proxy-authorization")
        .and_then(|(_, line)| basic_credential(&header_value(line)))
        .map(|token| credential_hash(&token));

    if method == "CONNECT" {
        let (host, port) = parse_authority(target, None)?;
        return Ok(ProxyRequest {
            kind: RequestKind::Connect,
            host,
            port,
            auth,
            method: None,
            path: None,
            upstream_head: None,
        });
    }
    let lower = target
        .get(..8)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if lower.starts_with("https://") {
        return Err(HeadError::HttpsAbsoluteForm);
    }
    if !lower.starts_with("http://") {
        return Err(if target.starts_with('/') || target == "*" {
            HeadError::NotAProxyRequest
        } else {
            HeadError::BadRequest
        });
    }
    let after_scheme = &target[7..];
    if after_scheme.contains('#') {
        return Err(HeadError::BadRequest);
    }
    let authority_end = after_scheme.find(['/', '?']).unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let (host, port) = parse_authority(authority, Some(80))?;
    let mut origin = after_scheme[authority_end..].to_string();
    if origin.is_empty() {
        origin = "/".into();
    } else if origin.starts_with('?') {
        origin.insert(0, '/');
    }
    let hosts: Vec<String> = headers
        .iter()
        .filter(|(name, _)| name == "host")
        .map(|(_, line)| header_value(line))
        .collect();
    if hosts.len() != 1 || !hosts[0].eq_ignore_ascii_case(authority) {
        return Err(HeadError::HostMismatch);
    }
    let path: String = origin
        .split('?')
        .next()
        .unwrap_or("/")
        .chars()
        .take(MAX_PATH_CHARS)
        .collect();
    let mut upstream = format!("{method} {origin} {version}\r\n").into_bytes();
    for (name, line) in &headers {
        if DROPPED_UPSTREAM_HEADERS.contains(&name.as_str()) {
            continue;
        }
        upstream.extend_from_slice(line);
        upstream.extend_from_slice(b"\r\n");
    }
    upstream.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(ProxyRequest {
        kind: RequestKind::Http,
        host,
        port,
        auth,
        method: Some(method.to_string()),
        path: Some(path),
        upstream_head: Some(upstream),
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

/// A refusal written to the client: JSON body, the reason in a header, and
/// `Proxy-Authenticate` for 407.
pub fn refusal_response(status: u16, reason: &str, host: &str, port: u16, hint: &str) -> Vec<u8> {
    let (error, header) = if status >= 500 {
        ("egress_unavailable", "unavailable")
    } else {
        ("egress_denied", "denied")
    };
    let body = format!(
        "{{\"error\":\"{error}\",\"reason\":{},\"host\":{},\"port\":{port},\"hint\":{}}}",
        json_string(reason),
        json_string(host),
        json_string(hint)
    );
    let mut response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nX-Axocoatl-Egress: {header}; reason={reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
        reason_phrase(status),
        body.len()
    );
    if status == 407 {
        response.push_str("Proxy-Authenticate: Basic realm=\"axocoatl-egress\"\r\n");
    }
    response.push_str("\r\n");
    response.push_str(&body);
    response.into_bytes()
}

/// The bridge's answer when the proxy socket is missing or refuses.
pub fn sidecar_unavailable_response() -> Vec<u8> {
    let body = "{\"error\":\"egress_unavailable\",\"reason\":\"sidecar_unavailable\",\"hint\":\"Axocoatl's egress proxy is restarting or stopped; retry shortly.\"}";
    format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nX-Axocoatl-Egress: unavailable\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The reply that opens a tunnel.
pub const CONNECT_ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user_token: &str) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = user_token.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let value = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..4 {
                if index <= chunk.len() {
                    out.push(TABLE[((value >> (18 - index * 6)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn head(text: &str) -> Vec<u8> {
        text.replace('\n', "\r\n").into_bytes()
    }

    #[test]
    fn connect_requests() {
        let request = parse_head(&head(&format!(
            "CONNECT registry.npmjs.org:443 HTTP/1.1\nHost: registry.npmjs.org:443\nProxy-Authorization: Basic {}\n\n",
            basic("axo:axe_token")
        )))
        .unwrap();
        assert_eq!(request.kind, RequestKind::Connect);
        assert_eq!(request.host, "registry.npmjs.org");
        assert_eq!(request.port, 443);
        assert_eq!(
            request.auth.as_deref(),
            Some(credential_hash("axe_token").as_str())
        );
        assert!(
            request.method.is_none() && request.path.is_none() && request.upstream_head.is_none()
        );

        let v6 = parse_head(&head("CONNECT [2606:4700::1]:443 HTTP/1.1\n\n")).unwrap();
        assert_eq!(
            (v6.host.as_str(), v6.port, v6.auth),
            ("[2606:4700::1]", 443, None)
        );
        let numeric = parse_head(&head("CONNECT 2130706433:80 HTTP/1.1\n\n")).unwrap();
        assert_eq!(numeric.host, "2130706433");
    }

    #[test]
    fn absolute_form_requests_are_rewritten_to_origin_form() {
        let request = parse_head(&head(&format!(
            "GET http://example.com:8080/a/b?q=secret HTTP/1.1\nHost: Example.com:8080\nProxy-Authorization: Basic {}\nProxy-Connection: keep-alive\nConnection: keep-alive\nKeep-Alive: 5\nAccept: */*\n\n",
            basic("axo:tok")
        )))
        .unwrap();
        assert_eq!(request.kind, RequestKind::Http);
        assert_eq!((request.host.as_str(), request.port), ("example.com", 8080));
        assert_eq!(request.method.as_deref(), Some("GET"));
        assert_eq!(request.path.as_deref(), Some("/a/b"));
        let upstream = String::from_utf8(request.upstream_head.unwrap()).unwrap();
        assert_eq!(
            upstream,
            "GET /a/b?q=secret HTTP/1.1\r\nHost: Example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        let default_port = parse_head(&head(
            "POST http://example.com HTTP/1.0\nHost: example.com\n\n",
        ))
        .unwrap();
        assert_eq!(default_port.port, 80);
        assert_eq!(default_port.path.as_deref(), Some("/"));
        let query_only = parse_head(&head(
            "GET http://example.com?x=1 HTTP/1.1\nHost: example.com\n\n",
        ))
        .unwrap();
        assert!(String::from_utf8(query_only.upstream_head.unwrap())
            .unwrap()
            .starts_with("GET /?x=1 HTTP/1.1\r\n"));
        let long = format!(
            "GET http://example.com/{} HTTP/1.1\nHost: example.com\n\n",
            "a".repeat(5000)
        );
        let long = parse_head(&head(&long)).unwrap();
        assert_eq!(long.path.unwrap().chars().count(), MAX_PATH_CHARS);
    }

    #[test]
    fn refused_heads() {
        let cases: &[(&str, HeadError)] = &[
            ("CONNECT example.com HTTP/1.1\n\n", HeadError::BadRequest),
            ("CONNECT example.com:0 HTTP/1.1\n\n", HeadError::BadRequest),
            (
                "CONNECT example.com:65536 HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
            (
                "CONNECT example.com:0443 HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
            (
                "CONNECT example.com:+443 HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
            ("CONNECT :443 HTTP/1.1\n\n", HeadError::BadRequest),
            ("CONNECT ::1:443 HTTP/1.1\n\n", HeadError::BadRequest),
            (
                "CONNECT [fe80::1%25eth0]:443 HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
            ("CONNECT [::1:443 HTTP/1.1\n\n", HeadError::BadRequest),
            (
                "CONNECT user@example.com:443 HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
            (
                "CONNECT exa mple.com:443 HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
            (
                "CONNECT example.com:443 HTTP/2.0\n\n",
                HeadError::BadRequest,
            ),
            ("CONNECT example.com:443\n\n", HeadError::BadRequest),
            (
                "GET https://example.com/ HTTP/1.1\nHost: example.com\n\n",
                HeadError::HttpsAbsoluteForm,
            ),
            (
                "GET / HTTP/1.1\nHost: example.com\n\n",
                HeadError::NotAProxyRequest,
            ),
            (
                "OPTIONS * HTTP/1.1\nHost: example.com\n\n",
                HeadError::NotAProxyRequest,
            ),
            ("GET ftp://example.com/ HTTP/1.1\n\n", HeadError::BadRequest),
            (
                "GET http://example.com/ HTTP/1.1\nHost: other.com\n\n",
                HeadError::HostMismatch,
            ),
            (
                "GET http://example.com:8080/ HTTP/1.1\nHost: example.com\n\n",
                HeadError::HostMismatch,
            ),
            (
                "GET http://example.com/ HTTP/1.1\n\n",
                HeadError::HostMismatch,
            ),
            (
                "GET http://example.com/ HTTP/1.1\nHost: example.com\nHost: example.com\n\n",
                HeadError::HostMismatch,
            ),
            (
                "GET http://u:p@example.com/ HTTP/1.1\nHost: example.com\n\n",
                HeadError::BadRequest,
            ),
            (
                "GET http://example.com/#frag HTTP/1.1\nHost: example.com\n\n",
                HeadError::BadRequest,
            ),
            (
                "GET http://example.com/ HTTP/1.1\nHost: example.com\n folded\n\n",
                HeadError::BadRequest,
            ),
            (
                "GET http://example.com/ HTTP/1.1\nBad Header: x\n\n",
                HeadError::BadRequest,
            ),
            (
                "GET http://example.com/ HTTP/1.1\nNoColon\n\n",
                HeadError::BadRequest,
            ),
            (
                "G\u{1}T http://example.com/ HTTP/1.1\n\n",
                HeadError::BadRequest,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_head(&head(text)), Err(*expected), "{text:?}");
        }
        // Bare LF line endings and a missing blank line are refused.
        assert_eq!(
            parse_head(b"CONNECT example.com:443 HTTP/1.1\n\r\n\r\n"),
            Err(HeadError::BadRequest)
        );
        assert_eq!(
            parse_head(b"CONNECT example.com:443 HTTP/1.1\r\n"),
            Err(HeadError::BadRequest)
        );
        let mut oversized = b"CONNECT example.com:443 HTTP/1.1\r\nX: ".to_vec();
        oversized.extend(std::iter::repeat_n(b'a', MAX_HEAD_BYTES));
        oversized.extend_from_slice(b"\r\n\r\n");
        assert_eq!(parse_head(&oversized), Err(HeadError::TooLarge));
        for error in [
            HeadError::TooLarge,
            HeadError::BadRequest,
            HeadError::HostMismatch,
            HeadError::HttpsAbsoluteForm,
            HeadError::NotAProxyRequest,
        ] {
            assert!(super::super::protocol::is_reason_code(error.reason()));
            assert!(!error.hint().is_empty());
        }
    }

    #[test]
    fn basic_credentials() {
        assert_eq!(
            basic_credential(&format!("Basic {}", basic("axo:axe_x"))).as_deref(),
            Some("axe_x")
        );
        assert_eq!(
            basic_credential(&format!("basic  {}", basic("axo:a:b"))).as_deref(),
            Some("a:b")
        );
        assert_eq!(
            basic_credential(&format!("Basic {}", basic(":tok"))).as_deref(),
            Some("tok")
        );
        for refused in [
            format!("Bearer {}", basic("axo:tok")),
            format!("Basic {}", basic("axo:")),
            format!("Basic {}", basic("no-colon")),
            "Basic !!!!".to_string(),
            "Basic YQ".to_string() + "=",
            "Basic".to_string(),
            String::new(),
            format!("Basic {}", basic("axo:tok")).replace('=', "") + "===",
        ] {
            assert_eq!(basic_credential(&refused), None, "{refused}");
        }
        // Malformed credentials reach the daemon as no credential (407 there).
        let request = parse_head(&head(
            "CONNECT example.com:443 HTTP/1.1\nProxy-Authorization: Basic %%%\n\n",
        ))
        .unwrap();
        assert_eq!(request.auth, None);
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YWI").unwrap(), b"ab");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert!(base64_decode("YR==").is_none());
        assert!(base64_decode("Y").is_none());
        assert!(base64_decode("YWJ=a").is_none());
    }

    #[test]
    fn head_end_and_responses() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
        let response = String::from_utf8(refusal_response(
            407,
            "no_credential",
            "a.example",
            443,
            "This process has no egress credential.",
        ))
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 407 Proxy Authentication Required\r\n"));
        assert!(response.contains("X-Axocoatl-Egress: denied; reason=no_credential\r\n"));
        assert!(response.contains("Proxy-Authenticate: Basic realm=\"axocoatl-egress\"\r\n"));
        let body: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["error"], "egress_denied");
        assert_eq!(body["reason"], "no_credential");
        assert_eq!(body["host"], "a.example");
        assert_eq!(body["port"], 443);
        let response =
            String::from_utf8(refusal_response(503, "decision_timeout", "a\"b", 1, "x")).unwrap();
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(!response.contains("Proxy-Authenticate"));
        let body: serde_json::Value =
            serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["error"], "egress_unavailable");
        assert_eq!(body["host"], "a\"b");
        let unavailable = String::from_utf8(sidecar_unavailable_response()).unwrap();
        assert!(unavailable.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(unavailable.contains("X-Axocoatl-Egress: unavailable\r\n"));
        let body: serde_json::Value =
            serde_json::from_str(unavailable.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["reason"], "sidecar_unavailable");
        let length: usize = unavailable
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(length, unavailable.split("\r\n\r\n").nth(1).unwrap().len());
    }
}
