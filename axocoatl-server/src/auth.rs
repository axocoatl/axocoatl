//! Authentication for the Axocoatl API server.
//!
//! Supports `x-api-key` and `Authorization: Bearer <token>`. Auth is wired in
//! [`crate::build_router`] and enforced on every route except the health
//! probes. The set of accepted credentials comes from the server config
//! (`server.auth`); see [`AuthConfig`].

use axocoatl_config::SecretString;
use axum::{
    extract::Request,
    http::{header, uri::Authority, HeaderMap, Method, StatusCode, Uri},
    middleware::Next,
    response::Response,
};
use std::str::FromStr;

/// Configuration for server authentication. Credentials are held as
/// `SecretString` so they are redacted in `Debug` / logs.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// API keys accepted via the `x-api-key` header.
    pub api_keys: Vec<SecretString>,
    /// Bearer tokens accepted via the `Authorization` header.
    pub bearer_tokens: Vec<SecretString>,
    /// When false, all requests pass through (loopback/local use).
    pub enabled: bool,
    /// Explicit operator escape hatch for unauthenticated non-loopback hosts.
    pub allow_unauthenticated_remote: bool,
}

impl AuthConfig {
    /// Build from the parsed `server.auth` config. Enabled automatically when
    /// any credential is present.
    pub fn new(api_keys: Vec<SecretString>, bearer_tokens: Vec<SecretString>) -> Self {
        let enabled = !api_keys.is_empty() || !bearer_tokens.is_empty();
        Self {
            api_keys,
            bearer_tokens,
            enabled,
            allow_unauthenticated_remote: false,
        }
    }

    /// Apply the server's explicit unauthenticated remote-bind decision.
    pub fn with_allow_unauthenticated_remote(mut self, allow: bool) -> Self {
        self.allow_unauthenticated_remote = allow;
        self
    }
}

/// Health/liveness probes stay open so orchestrators can reach them without a
/// credential. They expose no agent data or control surface.
pub fn is_public_path(path: &str) -> bool {
    matches!(path, "/health" | "/health/ready" | "/health/live")
}

/// Extract an API key from request headers.
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// Extract a Bearer token from the Authorization header.
fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from)
}

/// Whether the request carries a credential that this config accepts.
fn is_authorized(config: &AuthConfig, headers: &HeaderMap) -> bool {
    if let Some(key) = extract_api_key(headers) {
        if config.api_keys.iter().any(|k| k.expose_secret() == key) {
            return true;
        }
    }
    if let Some(token) = extract_bearer_token(headers) {
        if config
            .bearer_tokens
            .iter()
            .any(|t| t.expose_secret() == token)
        {
            return true;
        }
    }
    false
}

fn origin_matches_request_host(origin: &str, headers: &HeaderMap) -> bool {
    let Ok(origin_uri) = origin.parse::<Uri>() else {
        return false;
    };
    let Some(origin_authority) = origin_uri.authority() else {
        return false;
    };
    let Some(request_host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin_uri
        .scheme_str()
        .is_some_and(|scheme| matches!(scheme, "http" | "https"))
        && origin_authority.as_str().eq_ignore_ascii_case(request_host)
}

fn origin_is_explicitly_allowed(origin: &str, allowed_origins: &[String]) -> bool {
    let origin = origin.trim_end_matches('/');
    allowed_origins
        .iter()
        .any(|allowed| allowed.trim_end_matches('/') == origin)
}

fn request_needs_browser_write_guard(method: &Method, path: &str) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || path == "/ws"
        || path.ends_with("/ws")
}

fn host_is_canonical_local(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    Authority::from_str(host).ok().is_some_and(|authority| {
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .ok()
                .is_some_and(|ip| ip.is_loopback())
    })
}

fn unknown_host_requires_auth(config: &AuthConfig, headers: &HeaderMap) -> bool {
    !config.enabled && !config.allow_unauthenticated_remote && !host_is_canonical_local(headers)
}

/// Browser writes and WebSocket handshakes must originate from the workbench
/// itself or from an origin the operator explicitly allowed. CORS only protects
/// response reads; it does not stop a cross-origin form POST or a blind fetch.
/// Origin-less callers remain valid so CLI and local automation clients do not
/// acquire a browser-only CSRF requirement.
fn has_disallowed_browser_write_origin(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    allowed_origins: &[String],
) -> bool {
    if !request_needs_browser_write_guard(method, path) {
        return false;
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        // Browsers that suppress Origin still identify the relationship to
        // the target through Fetch Metadata. Preserve truly origin-less CLI
        // clients, while refusing a browser-declared cross-origin write.
        return headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| matches!(value, "cross-site" | "same-site"));
    };
    origin == "null"
        || (!origin_matches_request_host(origin, headers)
            && !origin_is_explicitly_allowed(origin, allowed_origins))
}

/// Core auth check. Open requests (auth disabled or a public path) pass through;
/// everything else needs a valid credential.
pub async fn enforce(
    config: &AuthConfig,
    allowed_origins: &[String],
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Local mode is intentionally credential-free, so its Host header must be
    // one of the canonical loopback names. Otherwise a DNS-rebinding page can
    // become same-origin with an unauthenticated daemon. Non-loopback serving
    // already requires configured auth and keeps those operator hostnames.
    if unknown_host_requires_auth(config, request.headers()) {
        return Err(StatusCode::MISDIRECTED_REQUEST);
    }
    if has_disallowed_browser_write_origin(
        request.method(),
        request.uri().path(),
        request.headers(),
        allowed_origins,
    ) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !config.enabled || is_public_path(request.uri().path()) {
        return Ok(next.run(request).await);
    }

    if is_authorized(config, request.headers()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Extension-based middleware: reads [`AuthConfig`] from request extensions.
/// Retained for callers that inject the config via an `Extension` layer;
/// [`crate::build_router`] uses [`enforce`] with a captured config instead.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    let config = request
        .extensions()
        .get::<AuthConfig>()
        .cloned()
        .unwrap_or_default();
    enforce(&config, &[], request, next).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_api_key_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "test-key-123".parse().unwrap());
        assert_eq!(extract_api_key(&headers), Some("test-key-123".to_string()));
    }

    #[test]
    fn extract_bearer_token_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer my-token".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), Some("my-token".to_string()));
    }

    #[test]
    fn extract_missing_headers() {
        let headers = HeaderMap::new();
        assert!(extract_api_key(&headers).is_none());
        assert!(extract_bearer_token(&headers).is_none());
    }

    #[test]
    fn auth_config_default_disabled() {
        let config = AuthConfig::default();
        assert!(!config.enabled);
    }

    #[test]
    fn new_enables_when_credentials_present() {
        assert!(!AuthConfig::new(vec![], vec![]).enabled);
        assert!(AuthConfig::new(vec!["k".into()], vec![]).enabled);
        assert!(AuthConfig::new(vec![], vec!["t".into()]).enabled);
    }

    #[test]
    fn authorized_matches_configured_credentials() {
        let config = AuthConfig::new(vec!["secret-key".into()], vec!["secret-token".into()]);

        let mut ok_key = HeaderMap::new();
        ok_key.insert("x-api-key", "secret-key".parse().unwrap());
        assert!(is_authorized(&config, &ok_key));

        let mut ok_bearer = HeaderMap::new();
        ok_bearer.insert("authorization", "Bearer secret-token".parse().unwrap());
        assert!(is_authorized(&config, &ok_bearer));

        let mut wrong = HeaderMap::new();
        wrong.insert("x-api-key", "nope".parse().unwrap());
        assert!(!is_authorized(&config, &wrong));

        assert!(!is_authorized(&config, &HeaderMap::new()));
    }

    #[test]
    fn health_paths_are_public() {
        assert!(is_public_path("/health"));
        assert!(is_public_path("/health/ready"));
        assert!(is_public_path("/health/live"));
        assert!(!is_public_path("/api/agents"));
        assert!(!is_public_path("/ws"));
        assert!(!is_public_path("/"));
    }

    #[test]
    fn null_browser_origin_cannot_write_or_open_control_websocket() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "null".parse().unwrap());
        assert!(has_disallowed_browser_write_origin(
            &Method::POST,
            "/api/sessions/s1/environment/rebuild",
            &headers,
            &[],
        ));
        assert!(has_disallowed_browser_write_origin(
            &Method::GET,
            "/ws",
            &headers,
            &[],
        ));
        assert!(!has_disallowed_browser_write_origin(
            &Method::GET,
            "/health",
            &headers,
            &[],
        ));
        assert!(!has_disallowed_browser_write_origin(
            &Method::POST,
            "/api/sessions/s1/environment/rebuild",
            &HeaderMap::new(),
            &[],
        ));
        let mut origin_suppressed_browser = HeaderMap::new();
        origin_suppressed_browser.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(has_disallowed_browser_write_origin(
            &Method::POST,
            "/api/sessions/s1/environment/rebuild",
            &origin_suppressed_browser,
            &[],
        ));
    }

    #[test]
    fn browser_write_origin_guard_separates_preview_from_workbench() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:18080".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://ses-123-p5173.localhost:18080".parse().unwrap(),
        );
        assert!(has_disallowed_browser_write_origin(
            &Method::POST,
            "/api/sessions/ses-123/environment/rebuild",
            &headers,
            &[],
        ));

        headers.insert(header::ORIGIN, "http://127.0.0.1:18080".parse().unwrap());
        assert!(!has_disallowed_browser_write_origin(
            &Method::POST,
            "/api/sessions/ses-123/environment/rebuild",
            &headers,
            &[],
        ));
    }

    #[test]
    fn browser_write_origin_guard_preserves_cli_and_configured_cors() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:8080".parse().unwrap());
        assert!(!has_disallowed_browser_write_origin(
            &Method::DELETE,
            "/api/sessions/ses-123",
            &headers,
            &[],
        ));

        headers.insert(header::ORIGIN, "https://operator.example".parse().unwrap());
        assert!(!has_disallowed_browser_write_origin(
            &Method::PATCH,
            "/api/sessions/ses-123",
            &headers,
            &["https://operator.example/".to_string()],
        ));
        assert!(has_disallowed_browser_write_origin(
            &Method::GET,
            "/ws",
            &headers,
            &[],
        ));
    }

    #[test]
    fn unauthenticated_local_mode_rejects_dns_rebinding_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "attacker.example:8080".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "http://attacker.example:8080".parse().unwrap(),
        );
        assert!(unknown_host_requires_auth(&AuthConfig::default(), &headers));
        assert!(!unknown_host_requires_auth(
            &AuthConfig::new(vec!["secret".into()], vec![]),
            &headers,
        ));
        assert!(!unknown_host_requires_auth(
            &AuthConfig::default().with_allow_unauthenticated_remote(true),
            &headers,
        ));

        for host in [
            "localhost:8080",
            "127.0.0.1:8080",
            "127.0.0.2:8080",
            "[::1]:8080",
        ] {
            headers.insert(header::HOST, host.parse().unwrap());
            assert!(!unknown_host_requires_auth(
                &AuthConfig::default(),
                &headers
            ));
        }
    }
}
