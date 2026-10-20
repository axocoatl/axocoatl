//! OpenRouter's API for `axocoatl connect openrouter`: the PKCE exchange
//! that turns the sign-in's code into a key (`POST /auth/keys`), the key's
//! own description (`GET /key`: its label, credit limit, usage and whether
//! the account has bought credits) and the public model catalog
//! (`GET /models`).
//!
//! The key is held in a zeroizing buffer ([`ApiKey`], which has no `Debug`
//! that shows it) and leaves this process only in the `Authorization`
//! header of `GET /key`, built as `connect claude-code`'s check builds its
//! own ([`super::verify`]). The exchange's body (code and verifier) and its
//! answer are zeroizing too. Errors carry OpenRouter's own message only
//! when it holds none of the request's secret values; nothing here formats
//! a key into a message. Production requests are HTTPS only, follow no
//! redirect and keep no idle connection.

use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use zeroize::Zeroizing;

/// OpenRouter's API base.
pub(crate) const API_BASE: &str = "https://openrouter.ai/api/v1";
/// Each request's time limit.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The longest answer read from `/auth/keys` or `/key`.
const MAX_SMALL_ANSWER: usize = 64 * 1024;
/// The longest catalog read (the daemon's admission reads it with the same
/// limit).
const MAX_CATALOG: usize = 32 * 1024 * 1024;
/// The longest key accepted (the secret store's limit).
const MAX_KEY_BYTES: usize = axocoatl_daemon::secret_store::MAX_SECRET_BYTES;

/// An OpenRouter API key. Its `Debug` never shows it.
pub(crate) struct ApiKey(Zeroizing<String>);

impl ApiKey {
    /// The key, for storing it and for the one header that sends it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// A key read from the secret store.
    pub(crate) fn from_stored(key: String) -> Self {
        Self(Zeroizing::new(key))
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey(<hidden>)")
    }
}

/// Whether `key` has the form of a key: 16 to [`MAX_KEY_BYTES`] printable
/// ASCII characters without spaces (OpenRouter's start with `sk-or-`).
pub(crate) fn has_key_form(key: &str) -> bool {
    (16..=MAX_KEY_BYTES).contains(&key.len()) && key.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Why a request to OpenRouter did not give what was asked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// OpenRouter refused it (`400`, `401`, `402`, `403`), with its message
    /// when it can be shown.
    Refused {
        status: u16,
        message: Option<String>,
    },
    /// No answer: a connection failure or the time limit.
    Unreachable(String),
    /// An answer that is neither what was asked nor a refusal.
    Unexpected(String),
}

impl ApiError {
    /// One clause for a message: what happened, with OpenRouter's words.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Refused { status, message } => match message {
                Some(message) => format!("OpenRouter answered HTTP {status}: {message}"),
                None => format!("OpenRouter answered HTTP {status}"),
            },
            Self::Unreachable(reason) | Self::Unexpected(reason) => reason.clone(),
        }
    }
}

/// What `GET /key` says about a key, without the key.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct KeyInfo {
    /// The key's label in the account.
    pub(crate) label: Option<String>,
    /// The key's credit limit in US dollars; `None` for no limit.
    pub(crate) limit: Option<f64>,
    /// What is left of that limit.
    pub(crate) limit_remaining: Option<f64>,
    /// How often the limit resets (`daily`, `weekly`, `monthly`); `None`
    /// when it never does.
    pub(crate) limit_reset: Option<String>,
    /// Credits the key has used, all time and today (UTC).
    pub(crate) usage: Option<f64>,
    pub(crate) usage_daily: Option<f64>,
    /// Whether the account has never bought credits.
    pub(crate) is_free_tier: Option<bool>,
    /// Requests to `:free` models today, and the daily limit.
    pub(crate) free_requests_used: Option<u64>,
    pub(crate) free_requests_limit: Option<u64>,
}

/// Text from OpenRouter, for a terminal: control characters dropped, at
/// most `limit` characters.
fn printable(text: &str, limit: usize) -> String {
    let mut shown: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect();
    if text.chars().filter(|c| !c.is_control()).count() > limit {
        shown.push('…');
    }
    shown
}

impl KeyInfo {
    /// Read `GET /key`'s answer; fields of an unexpected type are absent.
    pub(crate) fn from_answer(answer: &Value) -> Option<Self> {
        let data = answer.get("data")?.as_object()?;
        let number = |name: &str| data.get(name).and_then(Value::as_f64);
        let text = |name: &str| {
            data.get(name)
                .and_then(Value::as_str)
                .map(|text| printable(text, 100))
        };
        let free = data.get("free_model_daily_requests");
        let free_count = |name: &str| free.and_then(|free| free.get(name)).and_then(Value::as_u64);
        Some(Self {
            label: text("label"),
            limit: number("limit"),
            limit_remaining: number("limit_remaining"),
            limit_reset: text("limit_reset"),
            usage: number("usage"),
            usage_daily: number("usage_daily"),
            is_free_tier: data.get("is_free_tier").and_then(Value::as_bool),
            free_requests_used: free_count("used"),
            free_requests_limit: free_count("limit"),
        })
    }
}

/// OpenRouter's API at a base address.
pub(crate) struct Api {
    client: reqwest::Client,
    base: String,
    timeout: Duration,
}

impl Api {
    /// <https://openrouter.ai/api/v1>, HTTPS only, no redirects.
    pub(crate) fn openrouter() -> Result<Self, String> {
        Self::at(API_BASE, true, TIMEOUT)
    }

    /// The API at `base` with each request's time limit; plain HTTP only
    /// when `https_only` is off (tests).
    pub(crate) fn at(base: &str, https_only: bool, timeout: Duration) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .timeout(timeout)
            .user_agent(concat!("axocoatl/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("could not set up HTTPS: {error}"))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            timeout,
        })
    }

    /// A test server's API, over plain HTTP.
    #[cfg(test)]
    pub(crate) fn for_test(base: &str, timeout: Duration) -> Self {
        Self::at(base, false, timeout).unwrap()
    }

    /// The host requests go to, for messages.
    pub(crate) fn host(&self) -> &str {
        self.base
            .split_once("://")
            .map_or(self.base.as_str(), |(_, rest)| rest)
            .split('/')
            .next()
            .unwrap_or_default()
    }

    fn unreachable(&self, error: &reqwest::Error) -> ApiError {
        let what = if error.is_timeout() {
            format!("did not answer within {} seconds", self.timeout.as_secs())
        } else if error.is_connect() {
            "could not be reached".to_string()
        } else {
            "failed".to_string()
        };
        let mut detail = String::new();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            detail = cause.to_string();
            source = cause.source();
        }
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", printable(&detail, 200))
        };
        ApiError::Unreachable(format!("the request to {} {what}{detail}", self.host()))
    }

    /// Read at most `limit` bytes of `response`'s body, into a zeroizing
    /// buffer.
    async fn body(
        &self,
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<Zeroizing<Vec<u8>>, ApiError> {
        let mut body = Zeroizing::new(Vec::new());
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.unreachable(&error))?
        {
            if body.len() + chunk.len() > limit {
                return Err(ApiError::Unexpected(format!(
                    "{} answered with more than {limit} bytes",
                    self.host()
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// A refusal's status and OpenRouter's message (`{"error": {"message":
    /// …}}`), left out when it holds any of `secrets`.
    async fn refusal(&self, response: reqwest::Response, secrets: &[&str]) -> ApiError {
        let status = response.status().as_u16();
        let message = self
            .body(response, MAX_SMALL_ANSWER)
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
            .and_then(|answer| {
                answer
                    .pointer("/error/message")
                    .or_else(|| answer.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|message| {
                !message.is_empty()
                    && !secrets
                        .iter()
                        .any(|secret| !secret.is_empty() && message.contains(secret))
            })
            .map(|message| printable(&message, 200));
        if (400..=403).contains(&status) {
            ApiError::Refused { status, message }
        } else {
            ApiError::Unexpected(match message {
                Some(message) => format!("{} answered HTTP {status}: {message}", self.host()),
                None => format!("{} answered HTTP {status}", self.host()),
            })
        }
    }

    /// `POST /auth/keys`: exchange the sign-in's `code` and this sign-in's
    /// `verifier` for a key.
    pub(crate) async fn exchange(&self, code: &str, verifier: &str) -> Result<ApiKey, ApiError> {
        let body = Zeroizing::new(
            serde_json::to_vec(&serde_json::json!({
                "code": code,
                "code_verifier": verifier,
                "code_challenge_method": super::openrouter_pkce::CHALLENGE_METHOD,
            }))
            .map_err(|error| ApiError::Unexpected(error.to_string()))?,
        );
        let response = self
            .client
            .post(format!("{}/auth/keys", self.base))
            .header(CONTENT_TYPE, "application/json")
            .body(Bytes::from_owner(body))
            .send()
            .await
            .map_err(|error| self.unreachable(&error))?;
        if !response.status().is_success() {
            return Err(self.refusal(response, &[code, verifier]).await);
        }
        let body = self.body(response, MAX_SMALL_ANSWER).await?;
        #[derive(serde::Deserialize)]
        struct Exchanged {
            key: Option<String>,
        }
        let key = serde_json::from_slice::<Exchanged>(&body)
            .ok()
            .and_then(|answer| answer.key)
            .map(Zeroizing::new)
            .ok_or_else(|| {
                ApiError::Unexpected(format!(
                    "{} answered the exchange without a key",
                    self.host()
                ))
            })?;
        if !has_key_form(&key) {
            return Err(ApiError::Unexpected(format!(
                "{} answered the exchange with a value that is not in the form of a key (it is \
                 not shown)",
                self.host()
            )));
        }
        Ok(ApiKey(key))
    }

    /// `GET /key`: what OpenRouter says about `key`. `401` or `403` is a
    /// refusal of the key.
    pub(crate) async fn key_info(&self, key: &ApiKey) -> Result<KeyInfo, ApiError> {
        let mut bearer = Zeroizing::new(Vec::with_capacity(7 + key.expose().len()));
        bearer.extend_from_slice(b"Bearer ");
        bearer.extend_from_slice(key.expose().as_bytes());
        // The header holds `bearer` itself; it is zeroized when the
        // request's headers are dropped.
        let mut authorization = HeaderValue::from_maybe_shared(Bytes::from_owner(bearer))
            .map_err(|_| ApiError::Unexpected("the key is not a valid header value".into()))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let response = self
            .client
            .get(format!("{}/key", self.base))
            .headers(headers)
            .send()
            .await
            .map_err(|error| self.unreachable(&error))?;
        if !response.status().is_success() {
            return Err(self.refusal(response, &[key.expose()]).await);
        }
        let body = self.body(response, MAX_SMALL_ANSWER).await?;
        serde_json::from_slice::<Value>(&body)
            .ok()
            .as_ref()
            .and_then(KeyInfo::from_answer)
            .ok_or_else(|| {
                ApiError::Unexpected(format!(
                    "{} answered GET /key with something other than the key's description",
                    self.host()
                ))
            })
    }

    /// `GET /models`: OpenRouter's public catalog (no key is sent).
    pub(crate) async fn models(&self) -> Result<Value, ApiError> {
        let response = self
            .client
            .get(format!("{}/models", self.base))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| self.unreachable(&error))?;
        if !response.status().is_success() {
            return Err(self.refusal(response, &[]).await);
        }
        let body = self.body(response, MAX_CATALOG).await?;
        serde_json::from_slice(&body).map_err(|_| {
            ApiError::Unexpected(format!(
                "{} answered GET /models with something other than JSON",
                self.host()
            ))
        })
    }
}

/// An OpenRouter model id from `--writer` or `--reviewer`
/// (`vendor/model`, or `openrouter:vendor/model`), as native Sessions take
/// it. The refusal never repeats the value, in case it is a key.
pub(crate) fn model_id(flag: &str, given: &str) -> Result<String, String> {
    let id = given.strip_prefix("openrouter:").unwrap_or(given);
    let not_a_model = || {
        format!(
            "the value given to {flag} is not an OpenRouter model id (vendor/model, as \
             https://openrouter.ai/models lists them); it is not repeated here, in case it is a \
             key"
        )
    };
    let form = !id.is_empty()
        && id.len() <= 256
        && id.split('/').count() == 2
        && !id.starts_with('/')
        && !id.ends_with('/')
        && !id.starts_with("sk-")
        && !id.starts_with("openrouter/")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.:".contains(&byte));
    if !form {
        return Err(not_a_model());
    }
    let (vendor, _) = id.split_once('/').unwrap_or_default();
    if vendor.contains(':') {
        // `ollama:qwen3/…`, `anthropic:…`: another provider's model.
        return Err(format!(
            "{flag} takes an OpenRouter model id (vendor/model, optionally written \
             openrouter:vendor/model); {id} names another provider"
        ));
    }
    for (suffix, why) in [
        (
            ":online",
            "turns on OpenRouter web search, which native Sessions never request",
        ),
        (
            ":nitro",
            "opts into an OpenRouter service tier; native Sessions use the standard tier",
        ),
        (
            ":floor",
            "opts into an OpenRouter service tier; native Sessions use the standard tier",
        ),
    ] {
        if id.ends_with(suffix) {
            return Err(format!(
                "{flag}: {id} {why}, so use the model id without {suffix}"
            ));
        }
    }
    Ok(id.to_string())
}

/// What the catalog says about one model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelRow {
    pub(crate) id: String,
    /// The model's context window.
    pub(crate) context: Option<u64>,
    /// US dollars per million prompt and completion tokens.
    pub(crate) prompt_per_million: Option<f64>,
    pub(crate) completion_per_million: Option<f64>,
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// A per-token price (`"0.00000022"`) per million tokens.
fn per_million(value: &Value) -> Option<f64> {
    let per_token = value
        .as_str()
        .and_then(|text| text.parse::<f64>().ok())
        .or_else(|| value.as_f64())?;
    (per_token.is_finite() && per_token >= 0.0).then_some(per_token * 1_000_000.0)
}

/// `id`'s row in `catalog`, when a native Agent can use it: listed, with
/// tool calls, reading and writing text only. The reason otherwise.
pub(crate) fn catalog_model(catalog: &Value, id: &str) -> Result<ModelRow, String> {
    let rows = catalog
        .get("data")
        .and_then(Value::as_array)
        .ok_or("OpenRouter's model catalog has no list of models")?;
    let row = rows
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| {
            format!(
                "{id} is not in OpenRouter's model catalog (check the spelling at \
                 https://openrouter.ai/models)"
            )
        })?;
    if !strings(&row["supported_parameters"]).contains(&"tools") {
        return Err(format!(
            "{id} does not support tool calls on OpenRouter, which Axocoatl's Agents need"
        ));
    }
    let inputs = strings(&row["architecture"]["input_modalities"]);
    let outputs = strings(&row["architecture"]["output_modalities"]);
    if !inputs.contains(&"text") || outputs != ["text"] {
        return Err(format!(
            "{id} does not read and write text only on OpenRouter; native Sessions run text \
             models"
        ));
    }
    let context = row["context_length"]
        .as_u64()
        .or_else(|| row["top_provider"]["context_length"].as_u64());
    Ok(ModelRow {
        id: id.to_string(),
        context,
        prompt_per_million: per_million(&row["pricing"]["prompt"]),
        completion_per_million: per_million(&row["pricing"]["completion"]),
    })
}

impl ModelRow {
    /// One line: context and prices.
    pub(crate) fn describe(&self) -> String {
        let context = self
            .context
            .map(|tokens| format!("{} tokens of context", thousands(tokens)))
            .unwrap_or_else(|| "context not listed".to_string());
        let price = match (self.prompt_per_million, self.completion_per_million) {
            (Some(prompt), Some(completion)) if prompt == 0.0 && completion == 0.0 => {
                "free".to_string()
            }
            (Some(prompt), Some(completion)) => format!(
                "${} in / ${} out per million tokens",
                dollars(prompt),
                dollars(completion)
            ),
            _ => "price not listed".to_string(),
        };
        format!("{}: {context}, {price}", self.id)
    }
}

/// `1234567` as `1,234,567`.
pub(crate) fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// US dollars with two decimals, or up to six for amounts under a dollar
/// that two would hide.
pub(crate) fn dollars(amount: f64) -> String {
    if amount == 0.0 || amount.abs() >= 1.0 {
        format!("{amount:.2}")
    } else {
        let text = format!("{amount:.6}");
        let trimmed = text.trim_end_matches('0');
        if trimmed.len() < "0.00".len() {
            format!("{amount:.2}")
        } else {
            trimmed.to_string()
        }
    }
}
