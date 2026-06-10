//! Authentication for the Axocoatl API server.
//!
//! Supports `x-api-key` and `Authorization: Bearer <token>`. Auth is wired in
//! [`crate::build_router`] and enforced on every route except the health
//! probes. The set of accepted credentials comes from the server config
//! (`server.auth`); see [`AuthConfig`].

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};

/// Configuration for server authentication.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// API keys accepted via the `x-api-key` header.
    pub api_keys: Vec<String>,
    /// Bearer tokens accepted via the `Authorization` header.
    pub bearer_tokens: Vec<String>,
    /// When false, all requests pass through (loopback/local use).
    pub enabled: bool,
}

impl AuthConfig {
    /// Build from the parsed `server.auth` config. Enabled automatically when
    /// any credential is present.
    pub fn new(api_keys: Vec<String>, bearer_tokens: Vec<String>) -> Self {
        let enabled = !api_keys.is_empty() || !bearer_tokens.is_empty();
        Self {
            api_keys,
            bearer_tokens,
            enabled,
        }
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
        if config.api_keys.contains(&key) {
            return true;
        }
    }
    if let Some(token) = extract_bearer_token(headers) {
        if config.bearer_tokens.contains(&token) {
            return true;
        }
    }
    false
}

/// Core auth check. Open requests (auth disabled or a public path) pass through;
/// everything else needs a valid credential.
pub async fn enforce(
    config: &AuthConfig,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
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
    enforce(&config, request, next).await
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
}
