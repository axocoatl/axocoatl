//! HTTP middleware for the Axocoatl API server.
//! Rate limiting, request logging, and CORS.

use std::time::Instant;

use axum::{
    extract::Request,
    http::{header, Method},
    middleware::Next,
    response::Response,
};
use dashmap::DashMap;

/// Rate limiter configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum requests per window.
    pub max_requests: u32,
    /// Window duration in seconds.
    pub window_secs: u64,
    /// If true, rate limiting is enabled.
    pub enabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 100,
            window_secs: 60,
            enabled: false,
        }
    }
}

/// In-memory rate limiter state.
pub struct RateLimiter {
    config: RateLimitConfig,
    /// IP → (count, window_start)
    state: DashMap<String, (u32, Instant)>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            state: DashMap::new(),
        }
    }

    /// Check if a request from the given IP should be allowed.
    pub fn check(&self, ip: &str) -> bool {
        if !self.config.enabled {
            return true;
        }

        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.config.window_secs);

        let mut entry = self.state.entry(ip.to_string()).or_insert((0, now));
        let (count, window_start) = entry.value_mut();

        if now.duration_since(*window_start) > window {
            // New window
            *count = 1;
            *window_start = now;
            true
        } else if *count < self.config.max_requests {
            *count += 1;
            true
        } else {
            false
        }
    }
}

/// Request logging middleware.
pub async fn request_logging(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let start = Instant::now();

    let response = next.run(request).await;

    let elapsed = start.elapsed();
    tracing::info!(
        method = %method,
        uri = %uri,
        status = %response.status(),
        latency_ms = elapsed.as_millis(),
        "HTTP request"
    );

    response
}

/// CORS layer for the Axocoatl API.
///
/// `origins` is the explicit allow-list from `server.cors_origins`. When empty
/// (the default) the API is **same-origin only** — the dashboard, which is
/// served from the same origin, keeps working, while arbitrary web pages cannot
/// drive the API from a victim's browser. Previously this allowed `Any` origin,
/// which let any site reach a loopback-bound server (DNS-rebinding / CSRF-style
/// attacks). Origins that fail to parse are skipped with a warning.
pub fn cors_layer(origins: &[String]) -> tower_http::cors::CorsLayer {
    let layer = tower_http::cors::CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION, header::HeaderName::from_static("x-api-key")]);

    if origins.is_empty() {
        return layer; // no cross-origin access; same-origin requests are unaffected by CORS
    }

    let parsed: Vec<header::HeaderValue> = origins
        .iter()
        .filter_map(|o| match o.parse::<header::HeaderValue>() {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(origin = %o, "ignoring invalid CORS origin");
                None
            }
        })
        .collect();

    layer.allow_origin(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_under_limit() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_requests: 5,
            window_secs: 60,
            enabled: true,
        });

        for _ in 0..5 {
            assert!(limiter.check("127.0.0.1"));
        }
    }

    #[test]
    fn rate_limiter_blocks_over_limit() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_requests: 2,
            window_secs: 60,
            enabled: true,
        });

        assert!(limiter.check("127.0.0.1"));
        assert!(limiter.check("127.0.0.1"));
        assert!(!limiter.check("127.0.0.1"));
    }

    #[test]
    fn rate_limiter_disabled_allows_all() {
        let limiter = RateLimiter::new(RateLimitConfig::default());

        for _ in 0..1000 {
            assert!(limiter.check("127.0.0.1"));
        }
    }

    #[test]
    fn rate_limiter_separate_ips() {
        let limiter = RateLimiter::new(RateLimitConfig {
            max_requests: 1,
            window_secs: 60,
            enabled: true,
        });

        assert!(limiter.check("1.1.1.1"));
        assert!(!limiter.check("1.1.1.1")); // blocked
        assert!(limiter.check("2.2.2.2")); // different IP, allowed
    }
}
