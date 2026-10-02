//! Authentication for the Axocoatl API server.
//!
//! Two modes share [`enforce`], which [`crate::build_router`] wires in front of
//! every route:
//!
//! - **Configured credentials** (`server.auth.api_keys` / `bearer_tokens`):
//!   callers send `x-api-key` or `Authorization: Bearer <token>`. Only the
//!   health probes are public.
//! - **Local token** (loopback bind, no configured credentials and no
//!   `allow_unauthenticated`): the daemon keeps one random secret in
//!   `<data root>/local-api-token`. Scripts send it in either header. A browser
//!   signs in once by opening `/?token=<secret>`, which sets an HttpOnly,
//!   SameSite=Strict cookie. Health, static assets and the dashboard shell's
//!   sign-in page are public; every route that reads or changes state needs
//!   the secret.

use axocoatl_config::SecretString;
use axocoatl_core::{SecureDir, SecureLeaf};
use axum::{
    body::Body,
    extract::Request,
    http::{header, uri::Authority, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::str::FromStr;

/// File in the data root that holds the per-daemon local API token.
pub const LOCAL_TOKEN_FILE: &str = "local-api-token";
/// Cookie names are `axocoatl-token-<port>`: cookies ignore ports, and two
/// daemons on one machine must not overwrite each other's sign-in.
pub const LOCAL_TOKEN_COOKIE_PREFIX: &str = "axocoatl-token-";
/// Query parameter that carries the secret in a browser sign-in link.
pub const SIGN_IN_QUERY_KEY: &str = "token";
const LOCAL_TOKEN_BYTES: usize = 32;
/// 32 random bytes encode to 43 base64url characters without padding.
const LOCAL_TOKEN_MIN_LEN: usize = 43;
const LOCAL_TOKEN_MAX_FILE_BYTES: usize = 256;
const LOCAL_TOKEN_COOKIE_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

/// The per-daemon local API secret and the cookie name a browser stores it
/// under. `SecretString` keeps the value out of `Debug` output.
#[derive(Debug, Clone)]
pub struct LocalToken {
    secret: SecretString,
    cookie_name: String,
}

/// Configuration for server authentication. Credentials are held as
/// `SecretString` so they are redacted in `Debug` / logs.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// API keys accepted via the `x-api-key` header.
    pub api_keys: Vec<SecretString>,
    /// Bearer tokens accepted via the `Authorization` header.
    pub bearer_tokens: Vec<SecretString>,
    /// When false, all requests pass through (explicitly unauthenticated use).
    pub enabled: bool,
    /// Explicit operator escape hatch for an unauthenticated non-loopback
    /// listener. It also skips the canonical-Host check, so a loopback
    /// listener never sets it.
    pub allow_unauthenticated_remote: bool,
    /// Loopback-only per-daemon secret (see the module docs).
    pub local_token: Option<LocalToken>,
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
            local_token: None,
        }
    }

    /// Apply the server's explicit unauthenticated remote-bind decision.
    pub fn with_allow_unauthenticated_remote(mut self, allow: bool) -> Self {
        self.allow_unauthenticated_remote = allow;
        self
    }

    /// Require the per-daemon local token. `port` is the listener port, which
    /// names the browser cookie.
    pub fn with_local_token(mut self, secret: SecretString, port: u16) -> Self {
        self.local_token = Some(LocalToken {
            secret,
            cookie_name: local_token_cookie_name(port),
        });
        self.enabled = true;
        self
    }
}

fn local_token_cookie_name(port: u16) -> String {
    format!("{LOCAL_TOKEN_COOKIE_PREFIX}{port}")
}

fn invalid_token_file(data_root: &SecureDir, reason: &str) -> std::io::Error {
    let path = data_root.path().join(LOCAL_TOKEN_FILE);
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{} {reason}", path.display()),
    )
}

/// Read the local API token from the data root without creating it. Returns
/// `None` when the file is absent. Anything other than a private, singly
/// linked regular file holding a base64url token fails closed.
pub fn read_local_token(data_root: &SecureDir) -> std::io::Result<Option<SecretString>> {
    let (bytes, mode) =
        match data_root.read_leaf_limited(LOCAL_TOKEN_FILE, LOCAL_TOKEN_MAX_FILE_BYTES)? {
            None => return Ok(None),
            Some(SecureLeaf::Regular { bytes, mode }) => (bytes, mode),
            Some(SecureLeaf::Symlink { .. }) => {
                return Err(invalid_token_file(
                    data_root,
                    "is a symbolic link; delete it and restart Axocoatl to create a new token",
                ))
            }
            Some(SecureLeaf::Directory) => {
                return Err(invalid_token_file(
                    data_root,
                    "is a directory; remove it and restart Axocoatl to create a new token",
                ))
            }
        };
    if mode & 0o077 != 0 {
        return Err(invalid_token_file(
            data_root,
            "can be read or written by other users; run `chmod 600` on it, or delete it and restart Axocoatl to create a new token",
        ));
    }
    let token = std::str::from_utf8(&bytes)
        .ok()
        .map(|text| text.trim_end_matches(['\n', '\r']))
        .filter(|text| {
            text.len() >= LOCAL_TOKEN_MIN_LEN
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .ok_or_else(|| {
            invalid_token_file(
                data_root,
                "does not hold a valid token; delete it and restart Axocoatl to create a new one",
            )
        })?;
    Ok(Some(SecretString::from(token.to_string())))
}

/// Return the local API token, creating it on first use: 32 bytes from the
/// OS random number generator, base64url without padding, written atomically
/// with mode 0600. Only the daemon calls this, while it holds the data-root
/// lease. Delete the file and restart to rotate the token.
pub fn load_or_create_local_token(data_root: &SecureDir) -> std::io::Result<SecretString> {
    if let Some(token) = read_local_token(data_root)? {
        return Ok(token);
    }
    let mut bytes = [0_u8; LOCAL_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        std::io::Error::other(format!(
            "could not read the OS random number generator for the local API token: {error}"
        ))
    })?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    data_root.atomic_write(LOCAL_TOKEN_FILE, token.as_bytes())?;
    Ok(SecretString::from(token))
}

/// Read the local API token from a data directory path without creating
/// anything, for `axocoatl url`. The directory is opened without following
/// symlinks and must belong to this user with no group or world write access.
pub fn read_local_token_at(data_dir: &std::path::Path) -> std::io::Result<Option<SecretString>> {
    let data_root = match SecureDir::open_existing_all(data_dir) {
        Ok(data_root) => data_root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    data_root.require_owner_and_private_writes(effective_uid())?;
    read_local_token(&data_root)
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` takes no arguments and has no failure sentinel; uid_t
    // is an unsigned 32-bit integer on every supported Unix target.
    unsafe { geteuid() }
}

/// The browser sign-in link for a loopback daemon. It always names
/// `localhost`: the workbench redirects loopback IP literals there, and
/// using it directly keeps the secret out of that redirect.
pub fn sign_in_url(port: u16, secret: &SecretString) -> String {
    format!(
        "http://localhost:{port}/?{SIGN_IN_QUERY_KEY}={}",
        secret.expose_secret()
    )
}

/// Health/liveness probes stay open so orchestrators can reach them without a
/// credential. They expose no agent data or control surface.
pub fn is_public_path(path: &str) -> bool {
    matches!(path, "/health" | "/health/ready" | "/health/live")
}

/// Local token mode also serves the dashboard's static files without a
/// credential so the sign-in page and the shell can load. These prefixes match
/// the router's literal asset routes, which serve only embedded files.
fn is_local_public_path(path: &str) -> bool {
    is_public_path(path)
        || path == "/axo-tap.js"
        || ["/ui/", "/vendor/", "/lattice/", "/brand/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
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

/// Values of every cookie named `name`, across all `Cookie` headers.
fn cookie_values<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(move |pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name).then_some(value.trim())
        })
}

/// Constant-time comparison. Both sides are hashed first so the comparison
/// length never depends on the presented value, and an empty expected secret
/// (for example an unset `${ENV}` in `server.auth`) never matches.
fn secret_matches(expected: &str, presented: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let expected = Sha256::digest(expected.as_bytes());
    let presented = Sha256::digest(presented.as_bytes());
    let difference = expected
        .iter()
        .zip(presented.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        });
    std::hint::black_box(difference) == 0
}

/// Compare against every accepted secret without stopping at the first match.
fn any_secret_matches<'a>(
    expected: impl IntoIterator<Item = &'a SecretString>,
    presented: &str,
) -> bool {
    expected.into_iter().fold(false, |matched, secret| {
        secret_matches(secret.expose_secret(), presented) | matched
    })
}

/// A browser declared that another site (or another `localhost` port, which
/// is the same site) started this request.
fn fetch_site_is_foreign(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value, "cross-site" | "same-site"))
}

/// Whether the request carries a credential that this config accepts.
fn is_authorized(config: &AuthConfig, headers: &HeaderMap) -> bool {
    let local = config.local_token.as_ref().map(|token| &token.secret);
    let mut authorized = false;
    if let Some(key) = extract_api_key(headers) {
        authorized |= any_secret_matches(config.api_keys.iter().chain(local), &key);
    }
    if let Some(token) = extract_bearer_token(headers) {
        authorized |= any_secret_matches(config.bearer_tokens.iter().chain(local), &token);
    }
    if let Some(local) = &config.local_token {
        // A SameSite=Strict cookie still rides along on same-site requests,
        // and every `localhost:<port>` page is same-site with the workbench.
        // Only the workbench itself (same-origin), a typed URL (`none`) or a
        // request without Fetch Metadata (WebSocket upgrades, non-browser
        // clients) may use it. The Origin guard covers WebSocket handshakes.
        if !fetch_site_is_foreign(headers) {
            authorized |= cookie_values(headers, &local.cookie_name)
                .fold(false, |matched, value| {
                    secret_matches(local.secret.expose_secret(), value) | matched
                });
        }
    }
    authorized
}

fn query_key(pair: &str) -> &str {
    pair.split_once('=').map_or(pair, |(key, _)| key)
}

/// The raw sign-in value from `?token=…`. The key is compared without
/// percent-decoding, the same way [`loggable_uri`] finds it.
fn sign_in_token(uri: &Uri) -> Option<&str> {
    uri.query()?
        .split('&')
        .find(|pair| query_key(pair) == SIGN_IN_QUERY_KEY)
        .map(|pair| pair.split_once('=').map_or("", |(_, value)| value))
}

/// `/` plus every query pair except the sign-in token.
fn location_without_sign_in_token(uri: &Uri) -> String {
    let rest = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty() && query_key(pair) != SIGN_IN_QUERY_KEY)
        .collect::<Vec<_>>()
        .join("&");
    if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/?{rest}")
    }
}

/// The request URI with every `token=` query value replaced, for logs.
pub fn loggable_uri(uri: &Uri) -> String {
    let rendered = uri.to_string();
    let Some(query) = uri.query() else {
        return rendered;
    };
    if !query
        .split('&')
        .any(|pair| query_key(pair) == SIGN_IN_QUERY_KEY)
    {
        return rendered;
    }
    let redacted = query
        .split('&')
        .map(|pair| {
            if query_key(pair) == SIGN_IN_QUERY_KEY {
                "token=REDACTED"
            } else {
                pair
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    match rendered.split_once('?') {
        Some((before, _)) => format!("{before}?{redacted}"),
        None => rendered,
    }
}

fn is_local_token_cookie_pair(pair: &[u8]) -> bool {
    let name = pair
        .split(|byte| *byte == b'=')
        .next()
        .unwrap_or_default()
        .trim_ascii();
    name.len() >= LOCAL_TOKEN_COOKIE_PREFIX.len()
        && name[..LOCAL_TOKEN_COOKIE_PREFIX.len()]
            .eq_ignore_ascii_case(LOCAL_TOKEN_COOKIE_PREFIX.as_bytes())
}

/// A forwarded `Cookie` header without any Axocoatl token cookie, or `None`
/// when nothing else remains. Preview proxies use this so Session code never
/// receives the workbench credential.
pub fn without_local_token_cookies(value: &HeaderValue) -> Option<HeaderValue> {
    let kept = value
        .as_bytes()
        .split(|byte| *byte == b';')
        .map(<[u8]>::trim_ascii)
        .filter(|pair| !pair.is_empty() && !is_local_token_cookie_pair(pair))
        .collect::<Vec<_>>()
        .join(&b"; "[..]);
    if kept.is_empty() {
        None
    } else {
        HeaderValue::from_bytes(&kept).ok()
    }
}

/// Whether an upstream `Set-Cookie` would set an Axocoatl token cookie.
pub fn sets_local_token_cookie(value: &HeaderValue) -> bool {
    let first = value
        .as_bytes()
        .split(|byte| *byte == b';')
        .next()
        .unwrap_or_default();
    is_local_token_cookie_pair(first)
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

/// Loopback modes (the local token, or no auth at all) answer only to the
/// canonical loopback names. Configured credentials and the explicit
/// unauthenticated escape hatch on a non-loopback listener keep the
/// operator's hostnames.
pub(crate) fn local_mode_rejects_host(config: &AuthConfig, headers: &HeaderMap) -> bool {
    !config.allow_unauthenticated_remote
        && (config.local_token.is_some() || !config.enabled)
        && !host_is_canonical_local(headers)
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
        return fetch_site_is_foreign(headers);
    };
    origin == "null"
        || (!origin_matches_request_host(origin, headers)
            && !origin_is_explicitly_allowed(origin, allowed_origins))
}

const SIGN_IN_PAGE_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Sign in to Axocoatl</title>
<style>
:root { color-scheme: light dark; }
body { font: 16px/1.5 system-ui, sans-serif; max-width: 36rem; margin: 12vh auto; padding: 0 16px; }
code { font: 14px ui-monospace, SFMono-Regular, Menlo, monospace; }
</style>
</head>
<body>
<h1>Sign in to Axocoatl</h1>
"#;

const SIGN_IN_PAGE_BODY: &str = r#"<p>Run <code>axocoatl url</code> in a terminal, with the same <code>--config</code> and <code>AXOCOATL_DATA_DIR</code> that started this daemon, and open the link it prints. You can also open the sign-in link that <code>axocoatl serve</code> or <code>axocoatl dev</code> printed when it started.</p>
<p>Already opened the link? <a href="/">Continue</a></p>
</body>
</html>
"#;

fn sign_in_page(invalid_link: bool) -> Response {
    let reason = if invalid_link {
        "<p>That sign-in link is not valid for this Axocoatl daemon.</p>\n"
    } else {
        "<p>This workbench needs the sign-in token for this Axocoatl daemon.</p>\n"
    };
    let mut response = (
        StatusCode::UNAUTHORIZED,
        format!("{SIGN_IN_PAGE_HEAD}{reason}{SIGN_IN_PAGE_BODY}"),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    response
}

/// Store the cookie and send the browser to the same page without the secret.
fn sign_in_redirect(local: &LocalToken, uri: &Uri) -> Response {
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={LOCAL_TOKEN_COOKIE_MAX_AGE_SECS}",
        local.cookie_name,
        local.secret.expose_secret()
    );
    let (Ok(location), Ok(cookie)) = (
        HeaderValue::from_str(&location_without_sign_in_token(uri)),
        HeaderValue::from_str(&cookie),
    ) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, location);
    headers.insert(header::SET_COOKIE, cookie);
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /` in local token mode: sign in from `?token=`, serve the shell to a
/// signed-in browser, or explain how to sign in.
async fn local_dashboard(
    config: &AuthConfig,
    local: &LocalToken,
    request: Request,
    next: Next,
) -> Response {
    // Loopback IP literals first move to `localhost` (query included), so the
    // cookie is set for the same host the workbench and its Previews use.
    if crate::routes::canonical_workbench_location(request.headers(), request.uri()).is_some() {
        return next.run(request).await;
    }
    if let Some(presented) = sign_in_token(request.uri()) {
        return if secret_matches(local.secret.expose_secret(), presented) {
            sign_in_redirect(local, request.uri())
        } else {
            sign_in_page(true)
        };
    }
    if is_authorized(config, request.headers()) {
        next.run(request).await
    } else {
        sign_in_page(false)
    }
}

/// Core auth check. Checks run in order: Host (421), browser write origin
/// (403), then credentials (401). Public paths and explicitly unauthenticated
/// configs skip only the credential check.
pub async fn enforce(
    config: &AuthConfig,
    allowed_origins: &[String],
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Loopback modes must see one of the canonical loopback names in Host.
    // Otherwise a DNS-rebinding page could become same-origin with the
    // daemon. Non-loopback serving requires configured auth and keeps those
    // operator hostnames.
    if local_mode_rejects_host(config, request.headers()) {
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
    if let Some(local) = &config.local_token {
        let path = request.uri().path();
        if path == "/" && matches!(*request.method(), Method::GET | Method::HEAD) {
            return Ok(local_dashboard(config, local, request, next).await);
        }
        if is_local_public_path(path) || is_authorized(config, request.headers()) {
            return Ok(next.run(request).await);
        }
        return Err(StatusCode::UNAUTHORIZED);
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
        assert!(local_mode_rejects_host(&AuthConfig::default(), &headers));
        assert!(local_mode_rejects_host(
            &AuthConfig::default().with_local_token("secret".to_string().into(), 8080),
            &headers,
        ));
        assert!(!local_mode_rejects_host(
            &AuthConfig::new(vec!["secret".into()], vec![]),
            &headers,
        ));
        assert!(!local_mode_rejects_host(
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
            assert!(!local_mode_rejects_host(&AuthConfig::default(), &headers));
        }
    }

    const TOKEN: &str = "Zm9yLXRlc3RzLW9ubHktbm90LWEtcmVhbC10b2tlbi0xMjM";
    const PORT: u16 = 18080;

    fn token_config() -> AuthConfig {
        AuthConfig::new(vec![], vec![]).with_local_token(TOKEN.to_string().into(), PORT)
    }

    fn stub_app(config: AuthConfig) -> axum::Router {
        use axum::routing::{get, post};
        let ok = || async { "ok" };
        axum::Router::new()
            .route("/", get(crate::routes::dashboard))
            .route("/health", get(ok))
            .route("/health/live", get(ok))
            .route("/health/ready", get(ok))
            .route("/api/agents", get(ok))
            .route("/api/sessions/{id}/rebuild", post(ok))
            .route("/api/sessions/{id}/terminals/{tid}/ws", get(ok))
            .route("/ws", get(ok))
            .route("/a2a/tasks", post(ok))
            .route("/.well-known/agent.json", get(ok))
            .route("/ui/{*file}", get(ok))
            .route("/vendor/{*file}", get(ok))
            .route("/lattice/{file}", get(ok))
            .route("/brand/{file}", get(ok))
            .route("/axo-tap.js", get(ok))
            .layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    let config = config.clone();
                    async move { enforce(&config, &[], request, next).await }
                },
            ))
    }

    async fn send(
        config: &AuthConfig,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> Response {
        use tower::ServiceExt;
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("host"))
        {
            builder = builder.header(header::HOST, format!("localhost:{PORT}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        stub_app(config.clone())
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn header_text(response: &Response, name: header::HeaderName) -> Option<&str> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    fn cookie(value: &str) -> String {
        format!("{LOCAL_TOKEN_COOKIE_PREFIX}{PORT}={value}")
    }

    fn bearer(value: &str) -> String {
        format!("Bearer {value}")
    }

    #[tokio::test]
    async fn local_token_mode_refuses_requests_without_a_credential() {
        let config = token_config();
        for (method, uri) in [
            (Method::GET, "/api/agents"),
            (Method::GET, "/ws"),
            (Method::GET, "/api/sessions/s1/terminals/t1/ws"),
            (Method::POST, "/a2a/tasks"),
            (Method::GET, "/.well-known/agent.json"),
            (Method::POST, "/"),
        ] {
            let response = send(&config, method.clone(), uri, &[]).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn local_token_mode_refuses_wrong_credentials() {
        let config = token_config();
        let wrong_bearer = bearer("wrong");
        let wrong_cookie = cookie("wrong");
        let other_port_cookie = format!("{LOCAL_TOKEN_COOKIE_PREFIX}18081={TOKEN}");
        let truncated = &TOKEN[..TOKEN.len() - 1];
        for headers in [
            vec![("authorization", wrong_bearer.as_str())],
            vec![("x-api-key", "wrong")],
            vec![("x-api-key", truncated)],
            vec![("x-api-key", "")],
            vec![("cookie", wrong_cookie.as_str())],
            vec![("cookie", other_port_cookie.as_str())],
            vec![("authorization", TOKEN)],
        ] {
            let response = send(&config, Method::GET, "/api/agents", &headers).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{headers:?}");
        }
    }

    #[tokio::test]
    async fn local_token_mode_accepts_header_and_cookie_credentials() {
        let config = token_config();
        let valid_bearer = bearer(TOKEN);
        let valid_cookie = cookie(TOKEN);
        let mixed_cookie = format!("theme=dark; {}; app=1", cookie(TOKEN));
        for headers in [
            vec![("authorization", valid_bearer.as_str())],
            vec![("x-api-key", TOKEN)],
            vec![("cookie", valid_cookie.as_str())],
            vec![("cookie", mixed_cookie.as_str())],
            vec![("cookie", "theme=dark"), ("cookie", valid_cookie.as_str())],
        ] {
            for uri in [
                "/api/agents",
                "/ws",
                "/api/sessions/s1/terminals/t1/ws",
                "/.well-known/agent.json",
            ] {
                let response = send(&config, Method::GET, uri, &headers).await;
                assert_eq!(response.status(), StatusCode::OK, "{uri} {headers:?}");
            }
        }
        let response = send(
            &config,
            Method::POST,
            "/a2a/tasks",
            &[("authorization", valid_bearer.as_str())],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_cookie_is_ignored_on_cross_site_and_same_site_requests() {
        let config = token_config();
        let valid_cookie = cookie(TOKEN);
        for site in ["cross-site", "same-site"] {
            let response = send(
                &config,
                Method::GET,
                "/api/agents",
                &[("cookie", valid_cookie.as_str()), ("sec-fetch-site", site)],
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{site}");
        }
        for site in ["same-origin", "none"] {
            let response = send(
                &config,
                Method::GET,
                "/api/agents",
                &[("cookie", valid_cookie.as_str()), ("sec-fetch-site", site)],
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{site}");
        }
        let valid_bearer = bearer(TOKEN);
        let response = send(
            &config,
            Method::GET,
            "/api/agents",
            &[
                ("authorization", valid_bearer.as_str()),
                ("sec-fetch-site", "cross-site"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn local_token_mode_keeps_health_and_static_assets_public() {
        let config = token_config();
        for uri in [
            "/health",
            "/health/live",
            "/health/ready",
            "/ui/app.js",
            "/vendor/xterm/xterm.js",
            "/lattice/index.js",
            "/brand/mark.svg",
            "/axo-tap.js",
        ] {
            let response = send(&config, Method::GET, uri, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }

        let configured = AuthConfig::new(vec!["configured-key".into()], vec![]);
        for uri in ["/ui/app.js", "/axo-tap.js", "/"] {
            let response = send(&configured, Method::GET, uri, &[]).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
        let response = send(&configured, Method::GET, "/health", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn dashboard_without_a_credential_explains_how_to_sign_in() {
        let response = send(&token_config(), Method::GET, "/", &[]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            header_text(&response, header::CACHE_CONTROL),
            Some("no-store")
        );
        assert_eq!(
            header_text(&response, header::REFERRER_POLICY),
            Some("no-referrer")
        );
        assert!(header_text(&response, header::CONTENT_TYPE)
            .is_some_and(|value| value.starts_with("text/html")));
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let body = body_text(response).await;
        assert!(body.contains("axocoatl url"));
        assert!(body.contains(r#"href="/""#));
        assert!(!body.contains("ax-rail"));
    }

    #[tokio::test]
    async fn dashboard_with_a_valid_cookie_serves_the_workbench() {
        let valid_cookie = cookie(TOKEN);
        let response = send(
            &token_config(),
            Method::GET,
            "/",
            &[
                ("cookie", valid_cookie.as_str()),
                ("sec-fetch-site", "none"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_text(response).await,
            include_str!("../static/index.html")
        );
    }

    #[tokio::test]
    async fn sign_in_link_sets_the_cookie_and_redirects_without_the_secret() {
        let uri = format!("/?{SIGN_IN_QUERY_KEY}={TOKEN}&session=s1");
        let response = send(&token_config(), Method::GET, &uri, &[]).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = header_text(&response, header::LOCATION).unwrap();
        assert_eq!(location, "/?session=s1");
        assert!(!location.contains(TOKEN));
        assert_eq!(
            header_text(&response, header::SET_COOKIE),
            Some(
                format!(
                    "{LOCAL_TOKEN_COOKIE_PREFIX}{PORT}={TOKEN}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000"
                )
                .as_str()
            )
        );
        assert_eq!(
            header_text(&response, header::REFERRER_POLICY),
            Some("no-referrer")
        );
        assert_eq!(
            header_text(&response, header::CACHE_CONTROL),
            Some("no-store")
        );

        let response = send(
            &token_config(),
            Method::GET,
            &format!("/?{SIGN_IN_QUERY_KEY}={TOKEN}"),
            &[],
        )
        .await;
        assert_eq!(header_text(&response, header::LOCATION), Some("/"));
    }

    #[tokio::test]
    async fn invalid_sign_in_link_sets_no_cookie() {
        let response = send(&token_config(), Method::GET, "/?token=bad", &[]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        assert!(body_text(response).await.contains("not valid"));
    }

    #[tokio::test]
    async fn loopback_ip_sign_in_moves_to_localhost_first() {
        let uri = format!("/?{SIGN_IN_QUERY_KEY}={TOKEN}");
        let response = send(
            &token_config(),
            Method::GET,
            &uri,
            &[("host", "127.0.0.1:18080")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            header_text(&response, header::LOCATION),
            Some(format!("http://localhost:18080{uri}").as_str())
        );
        assert_eq!(
            header_text(&response, header::REFERRER_POLICY),
            Some("no-referrer")
        );
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn local_token_mode_keeps_host_and_origin_protections() {
        let config = token_config();
        let valid_bearer = bearer(TOKEN);
        let response = send(
            &config,
            Method::GET,
            "/api/agents",
            &[
                ("host", "attacker.example:18080"),
                ("authorization", valid_bearer.as_str()),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);

        let valid_cookie = cookie(TOKEN);
        for origin in ["null", "http://ses-123-p5173.localhost:18080"] {
            let response = send(
                &config,
                Method::POST,
                "/api/sessions/s1/rebuild",
                &[("cookie", valid_cookie.as_str()), ("origin", origin)],
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
        }
        let response = send(
            &config,
            Method::POST,
            "/api/sessions/s1/rebuild",
            &[
                ("cookie", valid_cookie.as_str()),
                ("origin", "http://localhost:18080"),
                ("sec-fetch-site", "same-origin"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn configured_credentials_ignore_cookies_and_sign_in_links() {
        let config = AuthConfig::new(vec!["configured-key".into()], vec![]);
        let response = send(
            &config,
            Method::GET,
            "/api/agents",
            &[("cookie", "axocoatl-token-18080=configured-key")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = send(&config, Method::GET, "/?token=configured-key", &[]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let response = send(
            &config,
            Method::GET,
            "/api/agents",
            &[("x-api-key", "configured-key")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // Configured operator hostnames keep working.
        let response = send(
            &config,
            Method::GET,
            "/api/agents",
            &[
                ("host", "axocoatl.internal"),
                ("x-api-key", "configured-key"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_configured_credentials_never_match() {
        let config = AuthConfig::new(vec!["".into()], vec!["".into()]);
        for headers in [vec![("x-api-key", "")], vec![("authorization", "Bearer ")]] {
            let response = send(&config, Method::GET, "/api/agents", &headers).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{headers:?}");
        }
    }

    #[test]
    fn secret_comparison_handles_lengths_and_empty_values() {
        assert!(secret_matches("abc", "abc"));
        assert!(!secret_matches("abc", "abd"));
        assert!(!secret_matches("abc", "abcd"));
        assert!(!secret_matches("abc", ""));
        assert!(!secret_matches("", ""));
    }

    #[test]
    fn loggable_uri_redacts_sign_in_tokens() {
        let redact = |uri: &str| loggable_uri(&uri.parse::<Uri>().unwrap());
        assert_eq!(redact("/?token=abc&s=1"), "/?token=REDACTED&s=1");
        assert_eq!(
            redact("/?a=1&token=abc&token=def"),
            "/?a=1&token=REDACTED&token=REDACTED"
        );
        assert_eq!(redact("/api/x?path=y"), "/api/x?path=y");
        assert_eq!(redact("/ui/app.js"), "/ui/app.js");
        assert_eq!(redact("/api/x?other_token=keep"), "/api/x?other_token=keep");
    }

    #[test]
    fn proxy_cookie_filter_removes_only_axocoatl_tokens() {
        let filter = |value: &str| {
            without_local_token_cookies(&HeaderValue::from_str(value).unwrap())
                .map(|value| value.to_str().unwrap().to_string())
        };
        assert_eq!(
            filter("axocoatl-token-18080=secret; app=1").as_deref(),
            Some("app=1")
        );
        assert_eq!(
            filter("a=1;AXOCOATL-TOKEN-8081=x; b=2").as_deref(),
            Some("a=1; b=2")
        );
        assert_eq!(filter("axocoatl-token-18080=secret"), None);
        assert_eq!(filter("app=1").as_deref(), Some("app=1"));

        assert!(sets_local_token_cookie(&HeaderValue::from_static(
            "axocoatl-token-18080=x; Path=/"
        )));
        assert!(!sets_local_token_cookie(&HeaderValue::from_static(
            "app=axocoatl-token-18080; Path=/"
        )));
    }

    #[test]
    fn sign_in_url_names_localhost_and_the_port() {
        assert_eq!(
            sign_in_url(8080, &SecretString::from("abc".to_string())),
            "http://localhost:8080/?token=abc"
        );
    }

    #[cfg(unix)]
    mod token_file {
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;

        fn data_root() -> (tempfile::TempDir, SecureDir) {
            let dir = tempfile::tempdir().unwrap();
            let root = SecureDir::open(dir.path()).unwrap();
            (dir, root)
        }

        #[test]
        fn token_is_created_once_private_and_reused() {
            let (dir, root) = data_root();
            assert!(read_local_token(&root).unwrap().is_none());
            assert!(!dir.path().join(LOCAL_TOKEN_FILE).exists());

            let first = load_or_create_local_token(&root).unwrap();
            let path = dir.path().join(LOCAL_TOKEN_FILE);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert_eq!(first.expose_secret().len(), LOCAL_TOKEN_MIN_LEN);
            assert_eq!(
                URL_SAFE_NO_PAD.decode(first.expose_secret()).unwrap().len(),
                LOCAL_TOKEN_BYTES
            );

            let second = load_or_create_local_token(&root).unwrap();
            assert_eq!(first.expose_secret(), second.expose_secret());
            let reopened = SecureDir::open(dir.path()).unwrap();
            assert_eq!(
                read_local_token(&reopened)
                    .unwrap()
                    .unwrap()
                    .expose_secret(),
                first.expose_secret()
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                first.expose_secret()
            );
        }

        #[test]
        fn shared_token_file_is_refused() {
            let (dir, root) = data_root();
            let token = load_or_create_local_token(&root).unwrap();
            let path = dir.path().join(LOCAL_TOKEN_FILE);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = read_local_token(&root).unwrap_err();
            assert!(error.to_string().contains("chmod 600"), "{error}");
            assert!(!error.to_string().contains(token.expose_secret()));
            assert!(load_or_create_local_token(&root).is_err());
        }

        #[test]
        fn symlinked_or_invalid_token_file_is_refused() {
            let (dir, root) = data_root();
            let target = dir.path().join("elsewhere");
            std::fs::write(&target, "A".repeat(LOCAL_TOKEN_MIN_LEN)).unwrap();
            std::os::unix::fs::symlink(&target, dir.path().join(LOCAL_TOKEN_FILE)).unwrap();
            assert!(read_local_token(&root).is_err());
            assert!(load_or_create_local_token(&root).is_err());

            let (dir, root) = data_root();
            let path = dir.path().join(LOCAL_TOKEN_FILE);
            for content in ["short", "not base64url!not base64url!not base64url!x"] {
                std::fs::write(&path, content).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                assert!(read_local_token(&root).is_err(), "{content}");
            }

            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(read_local_token(&root).is_err());
        }
    }
}
