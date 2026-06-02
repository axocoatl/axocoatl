//! Authentication middleware for the Axocoatl API server.
//! Supports Bearer token and API key authentication.

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};

/// Configuration for server authentication.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// API keys that are allowed to access the server.
    pub api_keys: Vec<String>,
    /// Bearer tokens that are allowed.
    pub bearer_tokens: Vec<String>,
    /// If true, authentication is required. If false, all requests are allowed.
    pub enabled: bool,
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

/// Authentication middleware layer.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    // Get auth config from request extensions
    let config = request
        .extensions()
        .get::<AuthConfig>()
        .cloned()
        .unwrap_or_default();

    if !config.enabled {
        return Ok(next.run(request).await);
    }

    let headers = request.headers();

    // Try API key first
    if let Some(key) = extract_api_key(headers) {
        if config.api_keys.contains(&key) {
            return Ok(next.run(request).await);
        }
    }

    // Try Bearer token
    if let Some(token) = extract_bearer_token(headers) {
        if config.bearer_tokens.contains(&token) {
            return Ok(next.run(request).await);
        }
    }

    Err(StatusCode::UNAUTHORIZED)
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
}
