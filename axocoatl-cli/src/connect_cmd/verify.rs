//! Checking a token with Anthropic before it is stored: one request that
//! costs nothing (`GET /v1/models?limit=1`), sent the way Claude Code sends
//! its OAuth token (`Authorization: Bearer`, with the OAuth beta header).
//! The response body is never read or shown.
//!
//! The `Authorization` value is built once, in a buffer sized for it (so it
//! never moves), and handed to the HTTP client without a copy: the header
//! holds that buffer, which is zeroized when the request's headers are
//! dropped, right after the response arrives. The client's own copies on
//! the way out (hyper's HTTP/1 write buffer or the HTTP/2 encoder's, and
//! rustls's plaintext buffer) are not Axocoatl's to overwrite: they are
//! freed, not zeroized, when the request completes, and the client keeps no
//! idle connection, so none outlives the check.

use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use zeroize::Zeroizing;

/// The API host the token is checked against (and that Claude Code's
/// route serves).
pub(crate) const ANTHROPIC_API: &str = "https://api.anthropic.com";
/// The API version header every request carries.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The beta flag that lets the Messages API take a Claude Code OAuth token.
const OAUTH_BETA: &str = "oauth-2025-04-20";
const TIMEOUT: Duration = Duration::from_secs(20);
const BEARER: &str = "Bearer ";

/// What Anthropic said about a token.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Valid,
    /// `401` or `403`.
    Rejected(u16),
    /// No answer, or one that does not say (the reason never holds the
    /// token).
    Unverified(String),
}

/// The client and base URL a token is checked with.
pub(crate) struct Verifier {
    client: reqwest::Client,
    base: String,
}

impl Verifier {
    /// Anthropic's API, over the CLI's TLS stack (rustls with the Web PKI
    /// roots), HTTPS only and no redirects.
    pub(crate) fn anthropic() -> Result<Self, String> {
        Self::with_client(Self::client_builder(), ANTHROPIC_API)
    }

    fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            // The connection, and the buffers the request went through,
            // close with the request.
            .pool_max_idle_per_host(0)
            .timeout(TIMEOUT)
            .user_agent(concat!("axocoatl/", env!("CARGO_PKG_VERSION")))
    }

    fn with_client(builder: reqwest::ClientBuilder, base: &str) -> Result<Self, String> {
        let client = builder
            .build()
            .map_err(|error| format!("could not set up HTTPS: {error}"))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// A verifier for a local test server: `base` and a root certificate it
    /// trusts besides the Web PKI's. Tests only.
    #[cfg(test)]
    pub(crate) fn for_test(
        base: &str,
        root: reqwest::Certificate,
        resolve: Option<(&str, std::net::SocketAddr)>,
    ) -> Self {
        let mut builder = Self::client_builder()
            .add_root_certificate(root)
            .timeout(Duration::from_secs(5));
        if let Some((host, address)) = resolve {
            builder = builder.resolve(host, address);
        }
        Self::with_client(builder, base).unwrap()
    }

    /// The host checked, for messages.
    pub(crate) fn host(&self) -> &str {
        self.base
            .strip_prefix("https://")
            .unwrap_or(&self.base)
            .split('/')
            .next()
            .unwrap_or_default()
    }

    pub(crate) async fn check(&self, token: &str) -> Verdict {
        let mut bearer = Zeroizing::new(Vec::with_capacity(BEARER.len() + token.len()));
        bearer.extend_from_slice(BEARER.as_bytes());
        bearer.extend_from_slice(token.as_bytes());
        // The header holds `bearer` itself; it is zeroized when the last
        // reference to it (the request's header map) is dropped.
        let mut authorization = match HeaderValue::from_maybe_shared(Bytes::from_owner(bearer)) {
            Ok(value) => value,
            Err(_) => {
                return Verdict::Unverified("the token is not a valid header value".into());
            }
        };
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETA));
        let request = self
            .client
            .get(format!("{}/v1/models?limit=1", self.base))
            .headers(headers)
            .send();
        match request.await {
            Ok(response) => {
                let status = response.status().as_u16();
                // The body is dropped unread.
                drop(response);
                match status {
                    200..=299 => Verdict::Valid,
                    401 | 403 => Verdict::Rejected(status),
                    other => Verdict::Unverified(format!("{} answered HTTP {other}", self.host())),
                }
            }
            Err(error) => {
                let what = if error.is_timeout() {
                    "timed out"
                } else if error.is_connect() {
                    "could not connect"
                } else {
                    "failed"
                };
                let mut detail = String::new();
                let mut source = std::error::Error::source(&error);
                while let Some(cause) = source {
                    detail = cause.to_string();
                    source = cause.source();
                }
                let detail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                };
                Verdict::Unverified(format!("the request to {} {what}{detail}", self.host()))
            }
        }
    }
}
