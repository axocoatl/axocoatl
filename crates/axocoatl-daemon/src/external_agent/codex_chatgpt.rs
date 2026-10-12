//! Codex with a ChatGPT sign-in: the sign-in `axocoatl connect codex` stores,
//! and the daemon's broker that keeps its access token current. Owner:
//! workstream `agents`.
//!
//! `axocoatl connect codex` runs the official `codex login` on this computer
//! with a private, temporary `CODEX_HOME` and reads the tokens Codex writes
//! to its `auth.json` there ([`ChatgptSignIn::from_codex_auth_json`]). They
//! are stored as one secret, [`CODEX_CHATGPT_SECRET`], in the format of
//! [`ChatgptSignIn::encode`]: one line of JSON holding the access token, the
//! refresh token, the ChatGPT account id and when the tokens were last
//! obtained. Nothing else of `auth.json` is kept (its id token, and an API
//! key Codex may have exchanged for, are deleted with the temporary home).
//!
//! The tokens never enter a Session container. A Codex writer with
//! `credential: codex-chatgpt` runs Codex with its own HTTP provider aimed at
//! the ChatGPT backend (`codex::chatgpt_argv`): Codex then holds no ChatGPT
//! sign-in at all (no `auth.json`, a placeholder in the provider's `env_key`
//! variable), so it has no token to refresh or revoke (in the pinned Podman
//! run it made no request but its two model calls; nothing routes
//! `auth.openai.com`, so one would be refused and recorded). The route
//! Axocoatl adds for `chatgpt.com` asks
//! the daemon's [`ChatgptBroker`] for the value to send with each request it
//! allows: the current access token as `Authorization: Bearer`, and the
//! account id as `ChatGPT-Account-ID`.
//!
//! The broker refreshes on this computer, the way Codex 0.160.1 does
//! (`codex-rs/login/src/auth/manager.rs` at `rust-v0.160.1`): before a
//! request, when the access token's JWT `exp` is at most five minutes away
//! (or, for a token without `exp`, when it was obtained more than eight days
//! ago), and after the upstream answered `401` to a request sent with the
//! current token. A refresh is one JSON `POST https://auth.openai.com/oauth/
//! token` with `{"client_id", "grant_type": "refresh_token",
//! "refresh_token"}`; the answer's `access_token`, `refresh_token` (OpenAI
//! rotates them, and a used one is refused as reused) and `id_token` replace
//! the stored ones, and the new sign-in is written back atomically (a
//! temporary file in the secrets directory, renamed over the old one, `0600`).
//! A refusal OpenAI gives as final (`401`, `invalid_grant`, or the
//! `refresh_token_expired`, `refresh_token_reused` and
//! `refresh_token_invalidated` codes) is remembered for that refresh token,
//! so it is not sent again; the sign-in must then be connected again. When a
//! refresh fails, a token that has not expired yet is still sent, as Codex
//! does. Refreshes are serialized. The new sign-in is written under the
//! secret store's lock (`secret_store::SecretsLock`, which `axocoatl connect
//! codex` and `axocoatl secret remove` also take), after reading the file again:
//! a sign-in someone else stored meanwhile is kept and sent instead, and a
//! sign-in removed meanwhile (or replaced by a value that is not one) stays
//! removed, the refreshed tokens being sent with that one request only. When
//! the new sign-in cannot be written, the broker keeps it in memory and uses
//! it (trying to store it again on each request) for as long as the file
//! still holds the refresh token it replaced, so the used refresh token is
//! not sent again.
//!
//! Only a route to [`codex::CHATGPT_HOST`](super::codex::CHATGPT_HOST) over
//! TLS asks the broker for a value; a route to any other host naming
//! `codex-chatgpt` reads the file as a plain credential, which refuses it.
//!
//! No value is ever formatted into an error, a log line or a `Debug` string;
//! buffers that held one are zeroized when they are dropped.
//!
//! A build with the verification-only `live-verify` feature also has the
//! stale-access switch ([`verify`]): it is never in a release build.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::egress_broker::rules::{ManagedCredential, ManagedValue};

/// Verification only: the stale-access switch (`live-verify` builds, and
/// tests).
#[cfg(any(test, feature = "live-verify"))]
#[path = "codex_chatgpt_verify.rs"]
pub mod verify;

/// The secret `axocoatl connect codex` stores the sign-in as, and the
/// credential the ChatGPT route names.
pub const CODEX_CHATGPT_SECRET: &str = "codex-chatgpt";
/// The first bytes of every stored sign-in: what marks a secret as one that
/// only its broker sends (`egress_broker::rules::MANAGED_VALUE_PREFIX`).
pub const FORMAT: &str = "axocoatl.codex-chatgpt/1";
/// The OAuth token endpoint Codex 0.160.1 refreshes at.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The OAuth client Codex 0.160.1 signs in with (`CLIENT_ID` in
/// `codex-rs/login/src/auth/manager.rs`): a refresh token is refreshed by
/// the client it was issued to.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The header that names the ChatGPT account (workspace) of a request.
pub const ACCOUNT_HEADER: &str = "chatgpt-account-id";
/// Refresh when the access token expires within this.
pub const REFRESH_WINDOW_MS: u64 = 5 * 60 * 1000;
/// Refresh a token without `exp` when it is older than this.
pub const REFRESH_INTERVAL_MS: u64 = 8 * 24 * 60 * 60 * 1000;
/// The longest token kept (a ChatGPT access token is a JWT of about 2 KiB).
const MAX_TOKEN_BYTES: usize = 6 * 1024;
/// The longest account id kept.
const MAX_ACCOUNT_ID_BYTES: usize = 256;
/// The largest `auth.json`, and the largest token endpoint answer, read.
pub const MAX_AUTH_FILE_BYTES: usize = 64 * 1024;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A token as stored: visible ASCII, no spaces, bounded. The reason never
/// holds it.
fn check_token(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("its {what} is empty"));
    }
    if value.len() > MAX_TOKEN_BYTES {
        return Err(format!("its {what} is longer than {MAX_TOKEN_BYTES} bytes"));
    }
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "its {what} holds a character a token does not have"
        ));
    }
    Ok(())
}

fn check_account_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_ACCOUNT_ID_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err("its ChatGPT account id is not one".into());
    }
    Ok(())
}

/// The claims of a JWT, when `token` is one (three parts, the middle one
/// base64url JSON). The signature is not checked: OpenAI's servers check it;
/// Axocoatl only reads when the token expires and which account it is for.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    let decoded = Zeroizing::new(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .ok()?,
    );
    serde_json::from_slice(&decoded).ok()
}

/// When a JWT expires (`exp`), in milliseconds.
fn jwt_expiry_ms(token: &str) -> Option<u64> {
    jwt_claims(token)?
        .get("exp")?
        .as_u64()
        .map(|seconds| seconds.saturating_mul(1000))
}

/// The ChatGPT account id a JWT names (`https://api.openai.com/auth`
/// `chatgpt_account_id`).
fn jwt_account_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| check_account_id(id).is_ok())
        .map(str::to_string)
}

/// `auth.json` as Codex 0.160.1 writes it (`codex-rs/login/src/auth/
/// storage.rs`), only the fields read here.
#[derive(Deserialize)]
struct AuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

/// The stored form ([`ChatgptSignIn::encode`]).
#[derive(Serialize)]
struct StoredRef<'a> {
    format: &'a str,
    access_token: &'a str,
    refresh_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<&'a str>,
    refreshed_at_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    format: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    account_id: Option<String>,
    refreshed_at_ms: u64,
}

/// A ChatGPT sign-in: the tokens Codex obtained and the account they are for.
#[derive(Clone)]
pub struct ChatgptSignIn {
    access_token: Zeroizing<String>,
    refresh_token: Zeroizing<String>,
    account_id: Option<String>,
    /// When the tokens were obtained (signed in or refreshed).
    refreshed_at_ms: u64,
}

impl std::fmt::Debug for ChatgptSignIn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatgptSignIn")
            .field("account_id", &self.account_id.as_ref().map(|_| "<set>"))
            .field("refreshed_at_ms", &self.refreshed_at_ms)
            .finish_non_exhaustive()
    }
}

impl ChatgptSignIn {
    /// A sign-in from its parts, each checked.
    pub fn new(
        access_token: Zeroizing<String>,
        refresh_token: Zeroizing<String>,
        account_id: Option<String>,
        refreshed_at_ms: u64,
    ) -> Result<Self, String> {
        check_token("access token", &access_token)?;
        check_token("refresh token", &refresh_token)?;
        if let Some(account_id) = &account_id {
            check_account_id(account_id)?;
        }
        Ok(Self {
            access_token,
            refresh_token,
            account_id,
            refreshed_at_ms,
        })
    }

    /// The ChatGPT sign-in in the `auth.json` Codex wrote after
    /// `codex login` (`now_ms` is when it was read: the tokens were just
    /// obtained). Refused, without naming any value, when the file holds no
    /// ChatGPT sign-in (an API key, a personal access token, or no tokens).
    /// The account id is the file's `tokens.account_id`, else the id or
    /// access token's `chatgpt_account_id` claim.
    pub fn from_codex_auth_json(bytes: &[u8], now_ms: u64) -> Result<Self, String> {
        if bytes.len() > MAX_AUTH_FILE_BYTES {
            return Err(format!(
                "Codex's auth.json is larger than {MAX_AUTH_FILE_BYTES} bytes"
            ));
        }
        let mut file: AuthFile = serde_json::from_slice(bytes)
            .map_err(|_| "Codex's auth.json is not the JSON Codex writes".to_string())?;
        match file.auth_mode.as_deref() {
            None | Some("chatgpt") => {}
            Some(mode) => {
                let mode = if mode.len() <= 32 && mode.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    mode
                } else {
                    "another kind"
                };
                return Err(format!(
                    "Codex stored a sign-in of kind {mode}, not a ChatGPT sign-in"
                ));
            }
        }
        let Some(tokens) = file.tokens.as_mut() else {
            return Err("Codex's auth.json holds no ChatGPT tokens".into());
        };
        let take = |value: &mut Option<String>| Zeroizing::new(value.take().unwrap_or_default());
        let access_token = take(&mut tokens.access_token);
        let refresh_token = take(&mut tokens.refresh_token);
        let id_token = take(&mut tokens.id_token);
        if access_token.is_empty() || refresh_token.is_empty() {
            return Err("Codex's auth.json holds no ChatGPT access and refresh token".to_string());
        }
        let account_id = tokens
            .account_id
            .take()
            .filter(|id| !id.is_empty())
            .or_else(|| jwt_account_id(&id_token))
            .or_else(|| jwt_account_id(&access_token));
        Self::new(access_token, refresh_token, account_id, now_ms)
    }

    /// The stored form: one line of JSON starting with `{"format":` and
    /// [`FORMAT`], at most [`crate::secret_store::MAX_SECRET_BYTES`].
    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut out = Zeroizing::new(Vec::with_capacity(4 * MAX_TOKEN_BYTES));
        serde_json::to_writer(
            &mut *out,
            &StoredRef {
                format: FORMAT,
                access_token: &self.access_token,
                refresh_token: &self.refresh_token,
                account_id: self.account_id.as_deref(),
                refreshed_at_ms: self.refreshed_at_ms,
            },
        )
        .map_err(|_| "the sign-in could not be written".to_string())?;
        if out.len() > crate::secret_store::MAX_SECRET_BYTES {
            return Err(format!(
                "the sign-in is longer than the {} bytes a secret holds",
                crate::secret_store::MAX_SECRET_BYTES
            ));
        }
        Ok(out)
    }

    /// A stored sign-in ([`Self::encode`]).
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let trimmed = bytes
            .strip_suffix(b"\r\n")
            .or_else(|| bytes.strip_suffix(b"\n"))
            .unwrap_or(bytes);
        let mut stored: Stored = serde_json::from_slice(trimmed).map_err(|_| {
            "it is not a ChatGPT sign-in stored by `axocoatl connect codex`".to_string()
        })?;
        let access_token = Zeroizing::new(std::mem::take(&mut stored.access_token));
        let refresh_token = Zeroizing::new(std::mem::take(&mut stored.refresh_token));
        if stored.format != FORMAT {
            return Err("it is not a ChatGPT sign-in stored by `axocoatl connect codex`".into());
        }
        Self::new(
            access_token,
            refresh_token,
            stored.account_id.take(),
            stored.refreshed_at_ms,
        )
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub fn refreshed_at_ms(&self) -> u64 {
        self.refreshed_at_ms
    }

    /// When the access token expires, when it is a JWT with `exp`.
    pub fn expires_at_ms(&self) -> Option<u64> {
        jwt_expiry_ms(&self.access_token)
    }

    /// Whether the access token is to be refreshed at `now_ms`: it expires
    /// within [`REFRESH_WINDOW_MS`], or, without `exp`, it was obtained more
    /// than [`REFRESH_INTERVAL_MS`] ago (Codex 0.160.1's rule).
    pub fn refresh_due(&self, now_ms: u64) -> bool {
        match self.expires_at_ms() {
            Some(expires) => expires <= now_ms.saturating_add(REFRESH_WINDOW_MS),
            None => self.refreshed_at_ms.saturating_add(REFRESH_INTERVAL_MS) < now_ms,
        }
    }

    /// Whether the access token has expired at `now_ms` (never, as far as
    /// Axocoatl can tell, for one without `exp`).
    pub fn expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms()
            .is_some_and(|expires| expires <= now_ms)
    }
}

/// What a refresh came to.
enum Refreshed {
    /// New tokens (or a sign-in someone else stored meanwhile).
    SignIn(ChatgptSignIn),
    /// OpenAI refused the refresh token for good.
    Refused(String),
    /// No answer, or one that does not say.
    Failed(String),
}

/// The token endpoint and the client that calls it.
pub struct TokenEndpoint {
    url: String,
    client: reqwest::Client,
}

impl TokenEndpoint {
    /// OpenAI's ([`TOKEN_URL`]): HTTPS only, no redirects, no idle
    /// connection kept.
    pub fn openai() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .timeout(REFRESH_TIMEOUT)
            .user_agent(concat!("axocoatl/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("could not set up HTTPS: {error}"))?;
        Ok(Self {
            url: TOKEN_URL.into(),
            client,
        })
    }

    /// A local test server's endpoint (plain HTTP allowed). Tests only.
    #[cfg(test)]
    pub(crate) fn for_test(url: &str) -> Self {
        Self {
            url: url.into(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(0)
                .timeout(REFRESH_TIMEOUT)
                .build()
                .unwrap(),
        }
    }
}

/// The token endpoint's answer, only the fields read.
#[derive(Deserialize)]
struct TokenAnswer {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

/// The error code of a refusal: `error.code`, `error` as a string, or
/// `code`, when it is a short identifier.
fn error_code(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = value
        .get("error")
        .and_then(|error| error.get("code").and_then(|code| code.as_str()))
        .or_else(|| value.get("error").and_then(|error| error.as_str()))
        .or_else(|| value.get("code").and_then(|code| code.as_str()))?;
    (code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
    .then(|| code.to_string())
}

/// Codes with which OpenAI refuses a refresh token for good (Codex
/// 0.160.1's `REFRESH_TOKEN_*` codes).
const FINAL_CODES: [&str; 4] = [
    "invalid_grant",
    "refresh_token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
];

/// What a broker remembers between requests.
#[derive(Default)]
struct BrokerState {
    /// SHA-256 of the refresh token OpenAI refused for good, and why.
    refused: Option<([u8; 32], String)>,
    /// A refreshed sign-in that could not be stored.
    pending: Option<Pending>,
}

/// A refreshed sign-in the broker could not store: used in place of the
/// stored one, and stored again, while the file still holds the refresh
/// token it replaced (OpenAI refuses that one as reused).
struct Pending {
    /// SHA-256 of the refresh token it replaced.
    replaced: [u8; 32],
    sign_in: ChatgptSignIn,
}

/// What writing a refreshed sign-in came to.
enum Settled {
    /// Stored; or a sign-in someone else stored meanwhile, used instead.
    Current(ChatgptSignIn),
    /// The secret was removed, or replaced by a value that is not a
    /// sign-in, meanwhile: the refreshed sign-in goes with this request
    /// only and is not stored.
    Removed(ChatgptSignIn),
    /// The refreshed sign-in could not be written: it is sent, and kept.
    Unstored(ChatgptSignIn),
}

/// The stored sign-in in `file`, read now (owner-only file, no link
/// followed). The error names no value.
fn load_sign_in(file: &Path) -> Result<ChatgptSignIn, String> {
    let text = crate::egress_broker::rules::read_owner_only(
        file,
        axocoatl_config::egress_routes::MAX_SECRET_FILE_BYTES,
    )
    .map_err(|reason| {
        format!("the ChatGPT sign-in {CODEX_CHATGPT_SECRET} cannot be read: {reason}")
    })?;
    ChatgptSignIn::decode(text.as_bytes()).map_err(|reason| {
        format!("the secret {CODEX_CHATGPT_SECRET} cannot be used: {reason}; {CONNECT_AGAIN}")
    })
}

/// Write `refreshed`, which replaces the sign-in whose refresh token has
/// digest `used`, holding the secret store's lock: only when the file still
/// holds that sign-in. `fail` makes the write fail (tests).
fn settle_refreshed(
    file: &Path,
    data_dir: &Path,
    used: [u8; 32],
    refreshed: ChatgptSignIn,
    fail: bool,
) -> Settled {
    let lock = match crate::secret_store::lock_secrets(data_dir, false) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Settled::Removed(refreshed),
        Err(error) => {
            tracing::warn!(%error, "could not lock the secret store to store the refreshed ChatGPT sign-in");
            return Settled::Unstored(refreshed);
        }
    };
    match load_sign_in(file) {
        Ok(stored) if digest(stored.refresh_token()) != used => {
            // Someone else (a new `axocoatl connect codex`) stored a
            // sign-in while this one was refreshed: theirs is kept.
            return Settled::Current(stored);
        }
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(
                "the ChatGPT sign-in was removed or replaced while it was refreshed; the \
                 refreshed sign-in is not stored"
            );
            return Settled::Removed(refreshed);
        }
    }
    let written = if fail {
        Err("the write failed (test)".to_string())
    } else {
        refreshed.encode().and_then(|encoded| {
            lock.set(CODEX_CHATGPT_SECRET, &encoded)
                .map_err(|error| error.to_string())
        })
    };
    match written {
        Ok(()) => {
            tracing::info!("refreshed the ChatGPT sign-in");
            Settled::Current(refreshed)
        }
        Err(reason) => {
            // The refresh token just used may no longer be accepted, and the
            // new one is only in memory: kept there until it is stored.
            tracing::warn!(%reason, "could not store the refreshed ChatGPT sign-in; kept in memory");
            Settled::Unstored(refreshed)
        }
    }
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// What to do when a sign-in is refused or cannot be read.
pub const CONNECT_AGAIN: &str = "connect again with `axocoatl connect codex`";

/// The daemon's broker for the ChatGPT sign-in stored as
/// [`CODEX_CHATGPT_SECRET`]: see the module documentation.
pub struct ChatgptBroker {
    /// `{data root}/secrets/codex-chatgpt`.
    file: PathBuf,
    /// The data root, where the secret store writes.
    data_dir: PathBuf,
    endpoint: TokenEndpoint,
    state: tokio::sync::Mutex<BrokerState>,
    /// How many of the next writes fail (tests).
    #[cfg(test)]
    pub(crate) fail_stores: std::sync::atomic::AtomicUsize,
    /// Verification only: the stale-access switch, when it is on.
    #[cfg(any(test, feature = "live-verify"))]
    stale_access: Option<Arc<verify::StaleAccessOnce>>,
}

impl ChatgptBroker {
    /// The broker of the sign-in stored under `data_dir`, refreshing at
    /// `endpoint`.
    pub fn new(data_dir: &Path, endpoint: TokenEndpoint) -> Self {
        Self {
            file: crate::secret_store::secret_path(data_dir, CODEX_CHATGPT_SECRET),
            data_dir: data_dir.to_path_buf(),
            endpoint,
            state: tokio::sync::Mutex::new(BrokerState::default()),
            #[cfg(test)]
            fail_stores: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "live-verify"))]
            stale_access: None,
        }
    }

    /// Verification only: the broker with the stale-access `switch`.
    #[cfg(any(test, feature = "live-verify"))]
    pub(crate) fn with_stale_access(
        mut self,
        switch: Option<Arc<verify::StaleAccessOnce>>,
    ) -> Self {
        self.stale_access = switch;
        self
    }

    /// Verification only: [`verify::STALE_ACCESS_TOKEN`] in place of
    /// `value`'s access token, when this is the switch's request.
    fn stale_access_first(&self, value: ManagedValue) -> ManagedValue {
        #[cfg(any(test, feature = "live-verify"))]
        if self
            .stale_access
            .as_ref()
            .is_some_and(|switch| switch.take_first())
        {
            tracing::warn!(
                "VERIFICATION SWITCH: this request to chatgpt.com carries an invalid access \
                 token in place of the stored ChatGPT sign-in's"
            );
            return ManagedValue {
                secret: SecretString::from(verify::STALE_ACCESS_TOKEN),
                headers: value.headers,
            };
        }
        value
    }

    /// Verification only: whether `sent` is the switch's invalid token, its
    /// `401` not handled yet.
    fn stale_access_rejected(&self, sent: &SecretString) -> bool {
        #[cfg(any(test, feature = "live-verify"))]
        if self
            .stale_access
            .as_ref()
            .is_some_and(|switch| switch.take_rejection(sent.expose_secret()))
        {
            tracing::warn!(
                "VERIFICATION SWITCH: the upstream answered 401 to the invalid access token; \
                 refreshing the ChatGPT sign-in"
            );
            return true;
        }
        let _ = sent;
        false
    }

    /// The broker of the sign-in stored under `data_dir`, refreshing at
    /// OpenAI ([`TOKEN_URL`]).
    pub fn for_data_root(data_dir: &Path) -> Result<Self, String> {
        Ok(Self::new(data_dir, TokenEndpoint::openai()?))
    }

    /// Whether the next write is to fail (tests).
    fn fail_store(&self) -> bool {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            self.fail_stores
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// Write `refreshed` (see [`settle_refreshed`]) and remember it when it
    /// could not be written. The sign-in to send.
    async fn settle(
        &self,
        state: &mut BrokerState,
        used: [u8; 32],
        refreshed: ChatgptSignIn,
    ) -> ChatgptSignIn {
        let (file, data_dir, fail) = (self.file.clone(), self.data_dir.clone(), self.fail_store());
        let fallback = refreshed.clone();
        let settled = tokio::task::spawn_blocking(move || {
            settle_refreshed(&file, &data_dir, used, refreshed, fail)
        })
        .await
        .unwrap_or(Settled::Unstored(fallback));
        state.pending = None;
        match settled {
            Settled::Current(sign_in) | Settled::Removed(sign_in) => sign_in,
            Settled::Unstored(sign_in) => {
                state.pending = Some(Pending {
                    replaced: used,
                    sign_in: sign_in.clone(),
                });
                sign_in
            }
        }
    }

    /// The sign-in in force: the stored one, or a refreshed one not stored
    /// yet ([`Pending`]) while the file still holds the sign-in it replaced,
    /// which is then stored again.
    async fn signed_in(&self, state: &mut BrokerState) -> Result<ChatgptSignIn, String> {
        let stored = load_sign_in(&self.file);
        let Some(pending) = state.pending.take() else {
            return stored;
        };
        // Removed, or replaced by something else: the pending one is dropped.
        let stored = stored?;
        if digest(stored.refresh_token()) != pending.replaced {
            return Ok(stored);
        }
        Ok(self.settle(state, pending.replaced, pending.sign_in).await)
    }

    /// The JSON body of a refresh, in a zeroizing buffer.
    fn refresh_body(refresh_token: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        #[derive(Serialize)]
        struct Body<'a> {
            client_id: &'a str,
            grant_type: &'a str,
            refresh_token: &'a str,
        }
        let mut body = Zeroizing::new(Vec::with_capacity(refresh_token.len() + 256));
        serde_json::to_writer(
            &mut *body,
            &Body {
                client_id: CLIENT_ID,
                grant_type: "refresh_token",
                refresh_token,
            },
        )
        .map_err(|_| "the refresh could not be written".to_string())?;
        Ok(body)
    }

    /// Refresh `current` at the token endpoint and store the result. Holds
    /// `state` (refreshes are serialized).
    async fn refresh(&self, state: &mut BrokerState, current: &ChatgptSignIn) -> Refreshed {
        let used = digest(current.refresh_token());
        if let Some((refused, reason)) = &state.refused {
            if *refused == used {
                return Refreshed::Refused(reason.clone());
            }
        }
        let body = match Self::refresh_body(current.refresh_token()) {
            Ok(body) => body,
            Err(reason) => return Refreshed::Failed(reason),
        };
        let response = self
            .endpoint
            .client
            .post(&self.endpoint.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(Bytes::from_owner(body))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let reason = if error.is_timeout() {
                    "the token endpoint did not answer in time".to_string()
                } else {
                    "the token endpoint could not be reached".to_string()
                };
                tracing::warn!(%reason, "refreshing the ChatGPT sign-in failed");
                return Refreshed::Failed(reason);
            }
        };
        let status = response.status();
        let answer = match read_bounded(response).await {
            Ok(answer) => answer,
            Err(reason) => return Refreshed::Failed(reason),
        };
        if !status.is_success() {
            let code = error_code(&answer);
            let refused = status == reqwest::StatusCode::UNAUTHORIZED
                || code
                    .as_deref()
                    .is_some_and(|code| FINAL_CODES.contains(&code));
            let said = code
                .as_deref()
                .map_or_else(String::new, |code| format!(", {code}"));
            tracing::warn!(
                status = status.as_u16(),
                code = code.as_deref().unwrap_or(""),
                "OpenAI refused to refresh the ChatGPT sign-in"
            );
            if refused {
                let reason = format!(
                    "OpenAI refused to refresh the ChatGPT sign-in (HTTP {}{said}); \
                     {CONNECT_AGAIN}",
                    status.as_u16()
                );
                state.refused = Some((used, reason.clone()));
                return Refreshed::Refused(reason);
            }
            return Refreshed::Failed(format!(
                "the token endpoint answered HTTP {}{said}",
                status.as_u16()
            ));
        }
        let mut parsed: TokenAnswer = match serde_json::from_slice(&answer) {
            Ok(parsed) => parsed,
            Err(_) => return Refreshed::Failed("the token endpoint's answer is not JSON".into()),
        };
        let access_token = Zeroizing::new(parsed.access_token.take().unwrap_or_default());
        let rotated = Zeroizing::new(parsed.refresh_token.take().unwrap_or_default());
        let id_token = Zeroizing::new(parsed.id_token.take().unwrap_or_default());
        if access_token.is_empty() {
            return Refreshed::Failed("the token endpoint's answer has no access token".into());
        }
        let refresh_token = if rotated.is_empty() {
            Zeroizing::new(current.refresh_token().to_string())
        } else {
            rotated
        };
        let account_id = jwt_account_id(&id_token)
            .or_else(|| jwt_account_id(&access_token))
            .or_else(|| current.account_id().map(str::to_string));
        let refreshed = match ChatgptSignIn::new(access_token, refresh_token, account_id, now_ms())
        {
            Ok(refreshed) => refreshed,
            Err(reason) => {
                return Refreshed::Failed(format!("the token endpoint's answer: {reason}"))
            }
        };
        Refreshed::SignIn(self.settle(state, used, refreshed).await)
    }

    fn value(sign_in: &ChatgptSignIn) -> Result<ManagedValue, String> {
        let mut headers = Vec::new();
        if let Some(account_id) = sign_in.account_id() {
            let value = HeaderValue::from_str(account_id)
                .map_err(|_| "the ChatGPT account id cannot be sent in a header".to_string())?;
            headers.push((HeaderName::from_static(ACCOUNT_HEADER), value));
        }
        Ok(ManagedValue {
            secret: SecretString::from(sign_in.access_token().to_string()),
            headers,
        })
    }
}

/// A response body of at most [`MAX_AUTH_FILE_BYTES`], in a zeroizing
/// buffer.
async fn read_bounded(mut response: reqwest::Response) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut body = Zeroizing::new(Vec::with_capacity(8 * 1024));
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_AUTH_FILE_BYTES {
                    return Err("the token endpoint's answer is too large".into());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(_) => return Err("the token endpoint's answer broke off".into()),
        }
    }
}

#[async_trait]
impl ManagedCredential for ChatgptBroker {
    fn file(&self) -> &Path {
        &self.file
    }

    fn host(&self) -> &str {
        super::codex::CHATGPT_HOST
    }

    async fn current(&self) -> Result<ManagedValue, String> {
        let mut state = self.state.lock().await;
        let sign_in = self.signed_in(&mut state).await?;
        let now = now_ms();
        if !sign_in.refresh_due(now) {
            return Self::value(&sign_in).map(|value| self.stale_access_first(value));
        }
        match self.refresh(&mut state, &sign_in).await {
            Refreshed::SignIn(refreshed) => {
                Self::value(&refreshed).map(|value| self.stale_access_first(value))
            }
            Refreshed::Refused(reason) | Refreshed::Failed(reason) => {
                if sign_in.expired(now) {
                    Err(format!("the ChatGPT access token expired and {reason}"))
                } else {
                    // Still valid: sent, as Codex sends it after a failed
                    // early refresh.
                    Self::value(&sign_in).map(|value| self.stale_access_first(value))
                }
            }
        }
    }

    async fn rejected(&self, sent: &SecretString) {
        let mut state = self.state.lock().await;
        let Ok(sign_in) = self.signed_in(&mut state).await else {
            return;
        };
        // Refreshed since that request was sent: nothing to do. (A `401`
        // to the verification switch's invalid token is one to the current
        // sign-in: that token stood in for it.)
        if sign_in.access_token() != sent.expose_secret() && !self.stale_access_rejected(sent) {
            return;
        }
        match self.refresh(&mut state, &sign_in).await {
            Refreshed::SignIn(_) => {}
            Refreshed::Refused(reason) | Refreshed::Failed(reason) => {
                tracing::warn!(%reason, "the ChatGPT sign-in was rejected and not refreshed");
            }
        }
    }
}

/// The daemon's managed credentials: the ChatGPT sign-in's broker for the
/// secret store under `data_dir`. Without HTTPS (which cannot happen on a
/// supported platform) there is none, and the route refuses its requests.
///
/// In a build with the verification-only `live-verify` feature, the broker
/// also has the process's stale-access switch ([`verify`]), which is on when
/// `AXOCOATL_VERIFY_STALE_ACCESS_ONCE=1` as the daemon starts. Without the
/// feature, that variable is never read.
pub fn managed_credentials(data_dir: &Path) -> crate::egress_broker::rules::ManagedCredentials {
    let mut managed = crate::egress_broker::rules::ManagedCredentials::default();
    match ChatgptBroker::for_data_root(data_dir) {
        #[cfg(feature = "live-verify")]
        Ok(broker) => managed.add(Arc::new(broker.with_stale_access(verify::for_process()))),
        #[cfg(not(feature = "live-verify"))]
        Ok(broker) => managed.add(Arc::new(broker)),
        Err(error) => {
            tracing::warn!(%error, "the ChatGPT sign-in broker is unavailable");
        }
    }
    managed
}

#[cfg(test)]
#[path = "codex_chatgpt_tests.rs"]
pub(crate) mod tests;
