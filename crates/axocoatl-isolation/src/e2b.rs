//! E2B Cloud backend — run Session tools inside a remote microVM instead of a
//! local Podman container.
//!
//! Two protocols, every shape verified live against E2B Cloud:
//! - **Control plane** — REST at `api_url`: `POST /sandboxes` (`{templateID, timeout, envVars}`
//!   → `{sandboxID, …}`), `POST /sandboxes/{id}/connect` (exact-id resume),
//!   `POST /sandboxes/{id}/pause` (durable suspension), `POST /sandboxes/{id}/timeout`
//!   (keep-alive), `DELETE /sandboxes/{id}`; `X-API-KEY` auth.
//! - **Data plane** — the sandbox's `envd` at `https://49983-{id}.{domain}`, ConnectRPC:
//!   - `process.Process/Start` — server-streaming. POST `application/connect+json` with a
//!     5-byte-framed JSON `{process:{cmd,args,cwd,envs}, tag, stdin, pty?}`. The response is framed
//!     `ProcessEvent`s — `start{pid}` → `data{stdout|stderr|pty}` (base64) → `end{exitCode?}` —
//!     terminated by a `0x02` end-of-stream frame. (`exitCode` is omitted when it is 0.)
//!   - Unary (`application/json`, by `tag`): `SendInput` (`stdin` or `pty`), `CloseStdin`,
//!     `Update` (PTY resize via `pty:{size}`), `SendSignal` (`SIGNAL_SIGKILL`).
//!
//! Command execution, background tasks, git-native clone, **and interactive PTY
//! terminals** all run over this client. Third-party E2B API implementations are
//! outside Axocoatl 1.0's support claim.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;
use tokio::sync::{oneshot, Notify};
use tokio_stream::StreamExt;

use crate::error::IsolationError;
use crate::session_sandbox::{
    captured_output_text, BgTask, ExecResult, Sandbox, COMMAND_OUTPUT_MAX_BYTES,
};

/// The port `envd` listens on inside every sandbox.
const ENVD_PORT: u32 = 49983;

/// Control-plane calls must finish independently of the request task that
/// initiated them. In particular, an abandoned create request may still have
/// created a billable remote VM, so the owned start task needs a finite point at
/// which it can either install lifecycle ownership or fail.
const CONTROL_PLANE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONTROL_PLANE_DELETE_TIMEOUT: Duration = Duration::from_secs(30);
/// An envd Connect message contains JSON plus base64 output. This accepts a
/// comfortably larger message than one complete retained command stream while
/// rejecting a corrupt/malicious length prefix before the decoder allocates it.
const CONNECT_FRAME_MAX_BYTES: usize = 4 * 1024 * 1024;
const CONNECT_BUFFER_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const AXOCOATL_CREATION_TOKEN_METADATA_KEY: &str = "axocoatl_creation_token";

/// Accept only one conservative URL-path component for a provider sandbox id.
/// Ids are interpolated into control- and data-plane URLs, so percent escapes,
/// separators, query syntax, dot components, and control characters are never
/// permitted even if a provider or persisted record supplies them.
fn validate_sandbox_id(sandbox_id: &str) -> Result<&str, IsolationError> {
    let valid = !sandbox_id.is_empty()
        && sandbox_id.len() <= 128
        && sandbox_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !valid {
        return Err(IsolationError::E2b(format!(
            "sandbox id '{sandbox_id}' is not a safe URL-path component"
        )));
    }
    Ok(sandbox_id)
}

/// Validate the DNS hostname used for authenticated envd traffic. This is
/// intentionally hostname-only: ports, userinfo, paths, query strings, IP
/// literals, whitespace, and control characters are not valid authorities.
pub fn validate_data_plane_domain(domain: &str) -> Result<&str, IsolationError> {
    let trimmed = domain.trim();
    let valid = domain == trimmed
        && !domain.is_empty()
        && domain.len() <= 253
        && domain.as_bytes().iter().all(u8::is_ascii)
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        });
    if !valid {
        return Err(IsolationError::E2b(format!(
            "E2B data-plane domain '{domain}' is not a DNS hostname"
        )));
    }
    Ok(domain)
}

/// Configuration for the remote backend validated with E2B Cloud.
///
/// Third-party E2B API implementations are outside Axocoatl 1.0's support claim.
#[derive(Debug, Clone)]
pub struct E2bConfig {
    /// Control-plane base URL, e.g. `https://api.e2b.dev`.
    pub api_url: String,
    /// E2B API key sent as `X-API-KEY`.
    pub api_key: String,
    /// Sandbox template id or alias, e.g. `base`.
    pub template: String,
    /// Domain the sandbox's `envd` traffic is served from, e.g. `e2b.app`.
    pub domain: String,
}

/// A checked operation against an exact, durably persisted E2B runtime id.
///
/// `NotFound` is deliberately distinct from transport/auth/control-plane
/// failures. A daemon restoring a Ready Session must durably mark that runtime
/// as lost when the exact id is gone; it must never turn a 404 into a fresh
/// sandbox and clone.
#[derive(Debug, thiserror::Error)]
pub enum E2bRuntimeError {
    #[error("persisted E2B sandbox '{sandbox_id}' was not found")]
    NotFound { sandbox_id: String },
    #[error(transparent)]
    ControlPlane(#[from] IsolationError),
}

/// Outcome of revalidating one fsynced ambiguous-create candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2bCreationCandidateCleanup {
    /// The provider object still carried the exact token and was deleted.
    Deleted,
    /// The exact hinted id no longer existed. This is not by itself proof that
    /// an ambiguous create left no other billable sandbox.
    AlreadyAbsent,
}

/// Exact remote identity learned by create or metadata reconciliation before
/// the daemon could durably publish it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2bUnconfirmedRuntime {
    pub sandbox_id: String,
    pub data_plane_domain: String,
}

/// Owned-start failure that retains any exact provider identity and whether
/// checked rollback proved that identity deleted. Callers must durably retain
/// an unconfirmed identity instead of reducing this error to text.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct E2bStartError {
    #[source]
    pub error: IsolationError,
    pub runtime: Option<E2bUnconfirmedRuntime>,
    pub cleanup_confirmed: bool,
    /// True when a provider create may have committed without returning an id.
    /// False means the request was never dispatched or was authoritatively
    /// rejected before allocation, so its precreate marker may be cleared.
    pub creation_ambiguous: bool,
}

impl E2bStartError {
    fn unresolved(error: IsolationError) -> Self {
        Self {
            error,
            runtime: None,
            cleanup_confirmed: false,
            creation_ambiguous: true,
        }
    }

    fn pre_dispatch(error: IsolationError) -> Self {
        Self {
            error,
            runtime: None,
            cleanup_confirmed: false,
            creation_ambiguous: false,
        }
    }

    fn with_runtime(
        error: IsolationError,
        sandbox_id: impl Into<String>,
        data_plane_domain: impl Into<String>,
        cleanup_confirmed: bool,
    ) -> Self {
        Self {
            error,
            runtime: Some(E2bUnconfirmedRuntime {
                sandbox_id: sandbox_id.into(),
                data_plane_domain: data_plane_domain.into(),
            }),
            cleanup_confirmed,
            creation_ambiguous: false,
        }
    }
}

enum E2bCreateRequestError {
    Rejected(IsolationError),
    Ambiguous(IsolationError),
}

impl E2bCreateRequestError {
    fn into_inner(self) -> IsolationError {
        match self {
            Self::Rejected(error) | Self::Ambiguous(error) => error,
        }
    }
}

/// A client for the E2B control-plane API shape validated with E2B Cloud and
/// the sandboxes it creates. Third-party implementations are outside the 1.0
/// support boundary.
/// Cloning is cheap (`reqwest::Client` is internally reference-counted).
#[derive(Clone)]
pub struct E2bClient {
    http: reqwest::Client,
    cfg: E2bConfig,
}

/// A live sandbox: its id, its `envd` base URL, and (secure mode only) an access token.
#[derive(Clone)]
pub struct E2bSession {
    pub sandbox_id: String,
    data_plane_domain: String,
    envd_base: String,
    access_token: Option<String>,
}

impl E2bClient {
    pub fn new(cfg: E2bConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            cfg,
        }
    }

    /// Create a sandbox with a maximum lifetime (seconds) and a set of
    /// environment variables. The vars are set at create time and persist into
    /// **every** later `envd` process (verified live) — that's how a git token is
    /// made available to the agent's own later `git push`, not just the clone.
    pub async fn create(
        &self,
        timeout_secs: u64,
        env: &BTreeMap<String, String>,
    ) -> Result<E2bSession, IsolationError> {
        self.create_request(timeout_secs, env, None)
            .await
            .map_err(E2bCreateRequestError::into_inner)
    }

    async fn create_request(
        &self,
        timeout_secs: u64,
        env: &BTreeMap<String, String>,
        creation_token: Option<&str>,
    ) -> Result<E2bSession, E2bCreateRequestError> {
        validate_data_plane_domain(&self.cfg.domain).map_err(E2bCreateRequestError::Rejected)?;
        // A hard daemon/process crash cannot run the explicit pause boundary.
        // The validated E2B Cloud API exposes the top-level `autoPause` create
        // field, so TTL expiry preserves the exact runtime instead of deleting
        // possible unpushed work. Full-memory auto-pause is the default;
        // explicit connect resumes the same id.
        let mut body = json!({
            "templateID": self.cfg.template,
            "timeout": timeout_secs,
            "autoPause": true,
        });
        if !env.is_empty() {
            body["envVars"] = json!(env);
        }
        if let Some(token) = creation_token {
            body["metadata"] = json!({ AXOCOATL_CREATION_TOKEN_METADATA_KEY: token });
        }
        let resp = self
            .http
            .post(format!("{}/sandboxes", self.cfg.api_url))
            .header("X-API-KEY", &self.cfg.api_key)
            .json(&body)
            .timeout(CONTROL_PLANE_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                E2bCreateRequestError::Ambiguous(IsolationError::E2b(format!(
                    "create request: {e}"
                )))
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| {
            E2bCreateRequestError::Ambiguous(IsolationError::E2b(format!("create body: {e}")))
        })?;
        if !status.is_success() {
            let error = IsolationError::E2b(format!("create sandbox failed ({status}): {body}"));
            return Err(
                if matches!(
                    status,
                    reqwest::StatusCode::BAD_REQUEST
                        | reqwest::StatusCode::UNAUTHORIZED
                        | reqwest::StatusCode::FORBIDDEN
                        | reqwest::StatusCode::NOT_FOUND
                        | reqwest::StatusCode::UNPROCESSABLE_ENTITY
                ) {
                    E2bCreateRequestError::Rejected(error)
                } else {
                    E2bCreateRequestError::Ambiguous(error)
                },
            );
        }
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| E2bCreateRequestError::Ambiguous(IsolationError::from(error)))?;
        let sandbox_id = v["sandboxID"].as_str().ok_or_else(|| {
            E2bCreateRequestError::Ambiguous(IsolationError::E2b(
                "create response missing sandboxID".into(),
            ))
        })?;
        let sandbox_id = validate_sandbox_id(sandbox_id)
            .map_err(E2bCreateRequestError::Ambiguous)?
            .to_string();
        if let Some(returned_domain) = v["domain"]
            .as_str()
            .map(str::trim)
            .filter(|domain| !domain.is_empty())
        {
            validate_data_plane_domain(returned_domain)
                .map_err(E2bCreateRequestError::Ambiguous)?;
            if returned_domain != self.cfg.domain {
                return Err(E2bCreateRequestError::Ambiguous(IsolationError::E2b(
                    format!(
                        "create response returned data-plane domain '{returned_domain}', expected configured domain '{}'",
                        self.cfg.domain
                    ),
                )));
            }
        }
        let data_plane_domain = self.cfg.domain.clone();
        let access_token = v["envdAccessToken"].as_str().map(str::to_string);
        let envd_base = format!("https://{ENVD_PORT}-{sandbox_id}.{data_plane_domain}");
        Ok(E2bSession {
            sandbox_id,
            data_plane_domain,
            envd_base,
            access_token,
        })
    }

    /// Discover every live sandbox carrying one exact Axocoatl creation
    /// token. The provider-side metadata filter is always rechecked locally so
    /// a compatibility implementation that ignores the query cannot cause an
    /// unrelated sandbox to be adopted or deleted.
    pub async fn discover_creation_token(
        &self,
        creation_token: &str,
    ) -> Result<Vec<String>, IsolationError> {
        let metadata_filter = format!("{AXOCOATL_CREATION_TOKEN_METADATA_KEY}={creation_token}");
        let mut next_token: Option<String> = None;
        let mut seen_pages = HashSet::new();
        let mut sandbox_ids = HashSet::new();
        loop {
            let mut request = self
                .http
                .get(format!("{}/v2/sandboxes", self.cfg.api_url))
                .header("X-API-KEY", &self.cfg.api_key)
                .query(&[
                    ("metadata", metadata_filter.as_str()),
                    ("state", "running,paused"),
                    ("limit", "100"),
                ])
                .timeout(CONTROL_PLANE_REQUEST_TIMEOUT);
            if let Some(cursor) = next_token.as_deref() {
                request = request.query(&[("nextToken", cursor)]);
            }
            let response = request.send().await.map_err(|error| {
                IsolationError::E2b(format!("discover sandbox creation token request: {error}"))
            })?;
            let status = response.status();
            let page_token = response
                .headers()
                .get("x-next-token")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let body = response.text().await.map_err(|error| {
                IsolationError::E2b(format!(
                    "discover sandbox creation token response body: {error}"
                ))
            })?;
            if !status.is_success() {
                return Err(IsolationError::E2b(format!(
                    "discover sandbox creation token failed ({status}): {body}"
                )));
            }
            let listed: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|error| {
                IsolationError::E2b(format!(
                    "discover sandbox creation token returned invalid JSON: {error}"
                ))
            })?;
            for sandbox in listed {
                let exact_token = sandbox
                    .get("metadata")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|metadata| metadata.get(AXOCOATL_CREATION_TOKEN_METADATA_KEY))
                    .and_then(serde_json::Value::as_str);
                let live_state = sandbox
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|state| matches!(state, "running" | "paused"));
                if exact_token == Some(creation_token) && live_state {
                    let sandbox_id = sandbox
                        .get("sandboxID")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            IsolationError::E2b(
                                "metadata-matched sandbox response missing sandboxID".to_string(),
                            )
                        })?;
                    sandbox_ids.insert(validate_sandbox_id(sandbox_id)?.to_string());
                }
            }
            let Some(cursor) = page_token else {
                break;
            };
            if !seen_pages.insert(cursor.clone()) {
                return Err(IsolationError::E2b(
                    "sandbox discovery returned a repeated pagination token".to_string(),
                ));
            }
            next_token = Some(cursor);
        }
        let mut sandbox_ids: Vec<String> = sandbox_ids.into_iter().collect();
        sandbox_ids.sort();
        Ok(sandbox_ids)
    }

    async fn create_owned(
        &self,
        timeout_secs: u64,
        env: &BTreeMap<String, String>,
        creation_token: &str,
    ) -> Result<E2bSession, E2bStartError> {
        let create_error = match self
            .create_request(timeout_secs, env, Some(creation_token))
            .await
        {
            Ok(session) => return Ok(session),
            Err(E2bCreateRequestError::Rejected(error)) => {
                return Err(E2bStartError::pre_dispatch(error))
            }
            Err(E2bCreateRequestError::Ambiguous(error)) => error,
        };

        let discovered = self
            .discover_creation_token(creation_token)
            .await
            .map_err(|discovery_error| {
                E2bStartError::unresolved(IsolationError::E2b(format!(
                    "{create_error}; reconciling the committed create by metadata token also failed: {discovery_error}"
                )))
            })?;
        let [sandbox_id] = discovered.as_slice() else {
            let detail = if discovered.is_empty() {
                "no live matching sandbox was visible"
            } else {
                "multiple live matching sandboxes were returned"
            };
            return Err(E2bStartError::unresolved(IsolationError::E2b(format!(
                "{create_error}; {detail} for durable creation token '{creation_token}', so Axocoatl retained the token and did not create or adopt a replacement"
            ))));
        };
        match self.connect_exact(sandbox_id, timeout_secs).await {
            Ok(session) => Ok(session),
            Err(E2bRuntimeError::NotFound { .. }) => Err(E2bStartError::with_runtime(
                IsolationError::E2b(format!(
                    "{create_error}; discovered exact sandbox '{sandbox_id}' by creation token, but it disappeared before reconnect"
                )),
                sandbox_id,
                &self.cfg.domain,
                true,
            )),
            Err(E2bRuntimeError::ControlPlane(connect_error)) => {
                Err(E2bStartError::with_runtime(
                    IsolationError::E2b(format!(
                        "{create_error}; discovered exact sandbox '{sandbox_id}' by creation token, but reconnect failed: {connect_error}"
                    )),
                    sandbox_id,
                    &self.cfg.domain,
                    false,
                ))
            }
        }
    }

    /// Resume or reconnect to one exact persisted sandbox id and extend its
    /// TTL. A 404 remains typed so callers cannot accidentally recover by
    /// creating a different sandbox.
    pub async fn connect_exact(
        &self,
        sandbox_id: &str,
        timeout_secs: u64,
    ) -> Result<E2bSession, E2bRuntimeError> {
        validate_sandbox_id(sandbox_id)?;
        validate_data_plane_domain(&self.cfg.domain)?;
        let resp = self
            .http
            .post(format!(
                "{}/sandboxes/{sandbox_id}/connect",
                self.cfg.api_url
            ))
            .header("X-API-KEY", &self.cfg.api_key)
            .json(&json!({ "timeout": timeout_secs }))
            .timeout(CONTROL_PLANE_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| {
                IsolationError::E2b(format!("connect exact sandbox request: {error}"))
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|error| {
            IsolationError::E2b(format!("connect exact sandbox response body: {error}"))
        })?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(E2bRuntimeError::NotFound {
                sandbox_id: sandbox_id.to_string(),
            });
        }
        if !status.is_success() {
            return Err(IsolationError::E2b(format!(
                "connect exact sandbox failed ({status}): {body}"
            ))
            .into());
        }

        let value: serde_json::Value = serde_json::from_str(&body).map_err(IsolationError::from)?;
        let returned_id = value["sandboxID"]
            .as_str()
            .ok_or_else(|| IsolationError::E2b("connect response missing sandboxID".to_string()))?;
        validate_sandbox_id(returned_id)?;
        if returned_id != sandbox_id {
            return Err(IsolationError::E2b(format!(
                "connect response returned sandbox id '{returned_id}', expected exact persisted id '{sandbox_id}'"
            ))
            .into());
        }
        let returned_domain = value["domain"]
            .as_str()
            .map(str::trim)
            .filter(|domain| !domain.is_empty());
        if let Some(returned_domain) = returned_domain {
            if returned_domain != self.cfg.domain {
                return Err(IsolationError::E2b(format!(
                    "connect response returned data-plane domain '{returned_domain}', expected persisted domain '{}'",
                    self.cfg.domain
                ))
                .into());
            }
        }
        let data_plane_domain = returned_domain.unwrap_or(&self.cfg.domain).to_string();
        let access_token = value["envdAccessToken"].as_str().map(str::to_string);
        Ok(E2bSession {
            sandbox_id: returned_id.to_string(),
            envd_base: format!("https://{ENVD_PORT}-{returned_id}.{data_plane_domain}"),
            data_plane_domain,
            access_token,
        })
    }

    /// Prove that one exact provider id still carries the durable metadata
    /// token installed by its owning Session before any persisted lifecycle
    /// action is sent. The provider response is checked locally; an endpoint
    /// that omits or rewrites metadata cannot authorize connect, pause, or
    /// delete.
    async fn verify_ownership_exact(
        &self,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<(), E2bRuntimeError> {
        validate_sandbox_id(sandbox_id)?;
        let discovered = self.discover_creation_token(ownership_token).await?;
        if !discovered.is_empty()
            && (discovered.len() != 1 || discovered.first().map(String::as_str) != Some(sandbox_id))
        {
            return Err(IsolationError::E2b(format!(
                "persisted E2B ownership token resolves to provider sandbox ids [{}], not only expected id '{sandbox_id}'",
                discovered.join(", ")
            ))
            .into());
        }
        self.verify_metadata_exact(sandbox_id, ownership_token)
            .await
    }

    /// Recheck the exact provider object rather than trusting discovery
    /// results or durable local hints. Ambiguous-create recovery deliberately
    /// permits more than one sandbox to carry a token, so its caller first
    /// discovers and fsyncs the whole set, then uses this per-id proof before
    /// each destructive request.
    async fn verify_metadata_exact(
        &self,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<(), E2bRuntimeError> {
        let value = self.sandbox_exact(sandbox_id).await?;
        let returned_token = value
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .and_then(|metadata| metadata.get(AXOCOATL_CREATION_TOKEN_METADATA_KEY))
            .and_then(serde_json::Value::as_str);
        if returned_token != Some(ownership_token) {
            return Err(IsolationError::E2b(format!(
                "sandbox '{sandbox_id}' does not carry the exact persisted Axocoatl ownership token"
            ))
            .into());
        }
        Ok(())
    }

    async fn sandbox_exact(&self, sandbox_id: &str) -> Result<serde_json::Value, E2bRuntimeError> {
        validate_sandbox_id(sandbox_id)?;
        let resp = self
            .http
            .get(format!("{}/sandboxes/{sandbox_id}", self.cfg.api_url))
            .header("X-API-KEY", &self.cfg.api_key)
            .timeout(CONTROL_PLANE_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| IsolationError::E2b(format!("get sandbox request: {error}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|error| IsolationError::E2b(format!("get sandbox response body: {error}")))?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(E2bRuntimeError::NotFound {
                sandbox_id: sandbox_id.to_string(),
            });
        }
        if !status.is_success() {
            return Err(IsolationError::E2b(format!(
                "get exact sandbox failed ({status}): {body}"
            ))
            .into());
        }
        let value: serde_json::Value = serde_json::from_str(&body).map_err(IsolationError::from)?;
        let returned_id = value["sandboxID"].as_str().ok_or_else(|| {
            IsolationError::E2b("get sandbox response missing sandboxID".to_string())
        })?;
        validate_sandbox_id(returned_id)?;
        if returned_id != sandbox_id {
            return Err(IsolationError::E2b(format!(
                "get sandbox response returned id '{returned_id}', expected exact persisted id '{sandbox_id}'"
            ))
            .into());
        }
        Ok(value)
    }

    async fn connect_owned_exact(
        &self,
        sandbox_id: &str,
        ownership_token: &str,
        timeout_secs: u64,
    ) -> Result<E2bSession, E2bRuntimeError> {
        self.verify_ownership_exact(sandbox_id, ownership_token)
            .await?;
        self.connect_exact(sandbox_id, timeout_secs).await
    }

    /// Pause one exact sandbox while preserving its filesystem, processes, and
    /// memory. A successful pause is the non-destructive daemon-shutdown
    /// and Close boundary; explicit Delete/Rebuild still uses [`Self::kill`].
    pub async fn pause_exact(&self, sandbox_id: &str) -> Result<(), E2bRuntimeError> {
        validate_sandbox_id(sandbox_id)?;
        let resp = self
            .http
            .post(format!("{}/sandboxes/{sandbox_id}/pause", self.cfg.api_url))
            .header("X-API-KEY", &self.cfg.api_key)
            .json(&json!({ "memory": true }))
            .timeout(CONTROL_PLANE_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| IsolationError::E2b(format!("pause sandbox request: {error}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(E2bRuntimeError::NotFound {
                sandbox_id: sandbox_id.to_string(),
            });
        }
        if status == reqwest::StatusCode::CONFLICT {
            // Pause is an at-least-once shutdown boundary. If the daemon died
            // after the control plane committed pause but before local state
            // advanced, retry may return 409. Reconcile the exact id rather
            // than treating every conflict as success.
            let state = self.sandbox_state_exact(sandbox_id).await?;
            if state == "paused" {
                return Ok(());
            }
            return Err(IsolationError::E2b(format!(
                "pause sandbox failed ({status}): {body}; exact runtime state is '{state}', not 'paused'"
            ))
            .into());
        }
        Err(IsolationError::E2b(format!("pause sandbox failed ({status}): {body}")).into())
    }

    async fn pause_owned_exact(
        &self,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<(), E2bRuntimeError> {
        self.verify_ownership_exact(sandbox_id, ownership_token)
            .await?;
        self.pause_exact(sandbox_id).await
    }

    async fn sandbox_state_exact(&self, sandbox_id: &str) -> Result<String, E2bRuntimeError> {
        let value = self.sandbox_exact(sandbox_id).await?;
        value["state"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| IsolationError::E2b("get sandbox response missing state".to_string()))
            .map_err(E2bRuntimeError::from)
    }

    /// Kill (delete) a sandbox. Callers may choose best-effort semantics, but
    /// this method itself reports transport and non-success responses so
    /// lifecycle cleanup failures are observable.
    pub async fn kill(&self, sandbox_id: &str) -> Result<(), IsolationError> {
        validate_sandbox_id(sandbox_id)?;
        let resp = self
            .http
            .delete(format!("{}/sandboxes/{sandbox_id}", self.cfg.api_url))
            .header("X-API-KEY", &self.cfg.api_key)
            .timeout(CONTROL_PLANE_DELETE_TIMEOUT)
            .send()
            .await
            .map_err(|error| IsolationError::E2b(format!("kill request: {error}")))?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(IsolationError::E2b(format!(
            "kill sandbox failed ({status}): {body}"
        )))
    }

    async fn kill_owned(
        &self,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<(), IsolationError> {
        match self
            .verify_ownership_exact(sandbox_id, ownership_token)
            .await
        {
            Ok(()) => self.kill(sandbox_id).await,
            Err(E2bRuntimeError::NotFound { .. }) => Ok(()),
            Err(E2bRuntimeError::ControlPlane(error)) => Err(error),
        }
    }

    async fn kill_creation_candidate(
        &self,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<E2bCreationCandidateCleanup, IsolationError> {
        validate_sandbox_id(sandbox_id)?;
        match self
            .verify_metadata_exact(sandbox_id, ownership_token)
            .await
        {
            Ok(()) => {
                self.kill(sandbox_id).await?;
                Ok(E2bCreationCandidateCleanup::Deleted)
            }
            // Durable discovered ids are crash-recovery hints. A prior attempt
            // may have deleted one immediately before the daemon stopped, so
            // absence of that exact id is an idempotent success. The caller
            // must rediscover the token before selecting hints, ensuring a
            // different still-live token-owned id cannot be hidden by a 404.
            Err(E2bRuntimeError::NotFound { .. }) => Ok(E2bCreationCandidateCleanup::AlreadyAbsent),
            Err(E2bRuntimeError::ControlPlane(error)) => Err(error),
        }
    }

    /// (Re)set a sandbox's remaining lifetime to `timeout_secs` from now. Called
    /// periodically by the keep-alive loop so a live session's VM doesn't
    /// self-terminate at its original TTL.
    pub async fn set_timeout(
        &self,
        sandbox_id: &str,
        timeout_secs: u64,
    ) -> Result<(), IsolationError> {
        validate_sandbox_id(sandbox_id)?;
        let resp = self
            .http
            .post(format!(
                "{}/sandboxes/{sandbox_id}/timeout",
                self.cfg.api_url
            ))
            .header("X-API-KEY", &self.cfg.api_key)
            .json(&json!({ "timeout": timeout_secs }))
            .timeout(CONTROL_PLANE_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("set_timeout request: {e}")))?;
        if !resp.status().is_success() {
            return Err(IsolationError::E2b(format!(
                "set_timeout failed: HTTP {}",
                resp.status()
            )));
        }
        Ok(())
    }

    // ── Interactive PTY terminals ────────────────────────────────────────
    // Protocol verified live against envd: Start carries `pty:{size:{cols,rows}}`
    // and streams output as `event.data.pty` (base64 vt100); SendInput takes
    // `input:{pty}`; Update resizes via `pty:{size}`; SendSignal SIGNAL_SIGKILL
    // ends it. The process is addressed by `tag` (we use the terminal id).

    /// Open a PTY-backed `sh -c <command>` and return the streaming response to
    /// pump. `TERM` is set so vt100 features are on.
    async fn pty_start(
        &self,
        session: &E2bSession,
        tag: &str,
        command: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) -> Result<reqwest::Response, IsolationError> {
        let payload = serde_json::to_vec(&json!({
            "process": {
                "cmd": "/bin/sh",
                "args": ["-c", command],
                "cwd": cwd,
                "envs": { "TERM": "xterm-256color" },
            },
            "pty": { "size": { "cols": cols, "rows": rows } },
            "tag": tag,
        }))?;
        let resp = self
            .envd(session, "Start")
            .header("Content-Type", "application/connect+json")
            .body(connect_frame(&payload))
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("PTY Start request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(IsolationError::E2b(format!(
                "PTY Start failed ({status}): {body}"
            )));
        }
        Ok(resp)
    }

    /// Send keystrokes to a PTY process (by tag).
    async fn pty_input(
        &self,
        session: &E2bSession,
        tag: &str,
        data: &[u8],
    ) -> Result<(), IsolationError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        self.envd_unary(
            session,
            "SendInput",
            &json!({ "process": { "tag": tag }, "input": { "pty": encoded } }),
        )
        .await
    }

    /// Resize a PTY (by tag) so the inner program reflows.
    async fn pty_resize(
        &self,
        session: &E2bSession,
        tag: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), IsolationError> {
        self.envd_unary(
            session,
            "Update",
            &json!({ "process": { "tag": tag }, "pty": { "size": { "cols": cols, "rows": rows } } }),
        )
        .await
    }

    /// Kill a PTY process (by tag) with SIGKILL. Best-effort.
    async fn pty_kill(&self, session: &E2bSession, tag: &str) -> Result<(), IsolationError> {
        self.envd_unary(
            session,
            "SendSignal",
            &json!({ "process": { "tag": tag }, "signal": "SIGNAL_SIGKILL" }),
        )
        .await
    }

    /// Run a command in the sandbox and collect `{stdout, stderr, exit}`.
    pub async fn exec(
        &self,
        session: &E2bSession,
        argv: &[&str],
        cwd: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.run(session, argv, cwd, None, timeout).await
    }

    /// Run a command with `stdin` piped in (the write path — `SendInput` + `CloseStdin`).
    pub async fn exec_stdin(
        &self,
        session: &E2bSession,
        argv: &[&str],
        stdin: &str,
        cwd: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.run(session, argv, cwd, Some(stdin.as_bytes()), timeout)
            .await
    }

    /// Start a command via `envd`, stream the response, and — when `stdin` is
    /// given — send it (by `tag`) once the process has started, then close stdin.
    async fn run(
        &self,
        session: &E2bSession,
        argv: &[&str],
        cwd: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        match tokio::time::timeout(timeout, self.run_inner(session, argv, cwd, stdin)).await {
            Ok(result) => result,
            Err(_) => Err(IsolationError::Timeout(timeout)),
        }
    }

    async fn run_inner(
        &self,
        session: &E2bSession,
        argv: &[&str],
        cwd: &str,
        stdin: Option<&[u8]>,
    ) -> Result<ExecResult, IsolationError> {
        let (cmd, args) = argv
            .split_first()
            .ok_or_else(|| IsolationError::E2b("empty argv".into()))?;
        let tag = format!("axo-{}", uuid::Uuid::new_v4());
        let payload = serde_json::to_vec(&json!({
            "process": { "cmd": cmd, "args": args, "envs": {}, "cwd": cwd },
            "tag": tag,
            "stdin": stdin.is_some(),
        }))?;

        let resp = self
            .envd(session, "Start")
            .header("Content-Type", "application/connect+json")
            .body(connect_frame(&payload))
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("Start request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(IsolationError::E2b(format!(
                "Start failed ({status}): {body}"
            )));
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = FrameDecoder::new();
        let mut out = ExecOutput::default();
        let mut input_sent = stdin.is_none();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| IsolationError::E2b(format!("Start stream: {e}")))?;
            decoder.push(&chunk)?;
            while let Some(frame) = decoder.next_frame()? {
                match frame {
                    Frame::End(v) => {
                        if let Some(err) = v.get("error") {
                            return Err(IsolationError::E2b(format!("envd stream error: {err}")));
                        }
                    }
                    Frame::Event(v) => {
                        out.apply(&v);
                        // The process is now live — safe to send stdin by tag.
                        if !input_sent && out.saw_start {
                            if let Some(data) = stdin {
                                self.send_input(session, &tag, data).await?;
                                self.close_stdin(session, &tag).await?;
                            }
                            input_sent = true;
                        }
                    }
                }
            }
        }
        Ok(out.into_result())
    }

    /// Write to a running process's stdin (unary Connect), selecting it by `tag`.
    async fn send_input(
        &self,
        session: &E2bSession,
        tag: &str,
        data: &[u8],
    ) -> Result<(), IsolationError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        self.envd_unary(
            session,
            "SendInput",
            &json!({ "process": { "tag": tag }, "input": { "stdin": encoded } }),
        )
        .await
    }

    /// Close a running process's stdin (unary Connect), selecting it by `tag`.
    async fn close_stdin(&self, session: &E2bSession, tag: &str) -> Result<(), IsolationError> {
        self.envd_unary(session, "CloseStdin", &json!({ "process": { "tag": tag } }))
            .await
    }

    /// A unary `envd` Connect call (`application/json`).
    async fn envd_unary(
        &self,
        session: &E2bSession,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<(), IsolationError> {
        let resp = self
            .envd(session, method)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| IsolationError::E2b(format!("{method}: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(IsolationError::E2b(format!(
                "{method} failed ({status}): {text}"
            )));
        }
        Ok(())
    }

    /// A request builder pointed at a `process.Process/{method}` envd endpoint,
    /// with the Connect version header and (secure mode) the access token.
    fn envd(&self, session: &E2bSession, method: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("{}/process.Process/{method}", session.envd_base))
            .header("Connect-Protocol-Version", "1");
        if let Some(token) = &session.access_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req
    }

    /// Stream a background command's combined output into `log` (tail-trimmed to
    /// 64 KiB) and set `status` when it exits. Best-effort — used by
    /// `spawn_background`, which returns before this completes.
    async fn run_to_log(
        &self,
        session: &E2bSession,
        command: &str,
        cwd: &str,
        log: Arc<Mutex<String>>,
        status: Arc<Mutex<String>>,
    ) {
        let payload = match serde_json::to_vec(&json!({
            "process": { "cmd": "sh", "args": ["-c", command], "envs": {}, "cwd": cwd },
            "stdin": false,
        })) {
            Ok(p) => p,
            Err(e) => return set(&status, format!("failed: {e}")),
        };
        let resp = match self
            .envd(session, "Start")
            .header("Content-Type", "application/connect+json")
            .body(connect_frame(&payload))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return set(&status, format!("failed: {e}")),
        };
        if !resp.status().is_success() {
            return set(&status, format!("failed: HTTP {}", resp.status()));
        }

        let b64 = base64::engine::general_purpose::STANDARD;
        let mut stream = resp.bytes_stream();
        let mut decoder = FrameDecoder::new();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            if decoder.push(&chunk).is_err() {
                break;
            }
            while let Ok(Some(frame)) = decoder.next_frame() {
                let Frame::Event(v) = frame else { continue };
                let event = &v["event"];
                if let Some(data) = event.get("data") {
                    for key in ["stdout", "stderr"] {
                        if let Some(s) = data.get(key).and_then(|x| x.as_str()) {
                            if let Ok(bytes) = b64.decode(s) {
                                append_tail(&log, &String::from_utf8_lossy(&bytes));
                            }
                        }
                    }
                } else if let Some(end) = event.get("end") {
                    let code = end.get("exitCode").and_then(|x| x.as_i64()).unwrap_or(0);
                    set(&status, format!("exited ({code})"));
                }
            }
        }
    }
}

/// Wrap a message in a Connect envelope: `[flag=0x00][len: u32 big-endian][payload]`.
fn connect_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0x00);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Set a shared status/log cell, ignoring a poisoned lock (best-effort).
fn set(cell: &Arc<Mutex<String>>, value: String) {
    if let Ok(mut guard) = cell.lock() {
        *guard = value;
    }
}

/// Append to a shared log, keeping only the last 64 KiB (background tasks are chatty).
fn append_tail(cell: &Arc<Mutex<String>>, text: &str) {
    if let Ok(mut guard) = cell.lock() {
        guard.push_str(text);
        if guard.len() > 64 * 1024 {
            let cut = guard.len() - 64 * 1024;
            guard.drain(..cut);
        }
    }
}

/// Pump a PTY `Start` stream: decode each `event.data.pty` (base64 vt100) into
/// the scrollback (tail-trimmed to 64 KiB) and the live output broadcast, and
/// flip `alive` to false when the stream ends. Runs until the process exits or
/// the stream drops.
async fn pump_pty_output(
    resp: reqwest::Response,
    output: crate::pty::PtyOutput,
    alive: Arc<Mutex<bool>>,
) {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut stream = resp.bytes_stream();
    let mut decoder = FrameDecoder::new();
    'pump: while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        if decoder.push(&chunk).is_err() {
            break;
        }
        while let Ok(Some(frame)) = decoder.next_frame() {
            let Frame::Event(v) = frame else { continue };
            let event = &v["event"];
            if let Some(s) = event
                .get("data")
                .and_then(|d| d.get("pty"))
                .and_then(|x| x.as_str())
            {
                if let Ok(bytes) = b64.decode(s) {
                    output.append(bytes);
                }
            } else if event.get("end").is_some() {
                break 'pump;
            }
        }
    }
    if let Ok(mut a) = alive.lock() {
        *a = false;
    }
}

/// A [`Sandbox`] backed by a remote E2B Cloud microVM. Session tools run
/// against it exactly as they do against the local Podman `SessionSandbox`.
///
/// Command execution, background tasks, and interactive PTY terminals are
/// implemented through envd; provider compatibility remains subject to the live
/// remote-backend gate.
///
/// One shared lifecycle owner follows every re-rooted view of a remote VM.
/// Freshly-created handles delete on final drop until the daemon durably
/// publishes Ready. Persisted/reattached handles preserve the exact VM on final
/// drop; only an explicit checked stop deletes them.
pub struct E2bSandbox {
    client: E2bClient,
    session: E2bSession,
    /// Exact durable provider metadata token for Session-owned handles.
    /// Generic compatibility handles have no ownership token and retain their
    /// legacy raw lifecycle behavior.
    ownership_token: Option<String>,
    /// The working directory inside the sandbox (the confinement root for file tools).
    root: PathBuf,
    tasks: Mutex<Vec<E2bBgHandle>>,
    /// Live interactive PTY terminals opened on this handle.
    terminals: Mutex<Vec<Arc<crate::pty::PtyTerminal>>>,
    /// Shared by the primary handle and every `with_root` view. Before durable
    /// Ready, final drop owns deletion; afterwards it preserves. An explicit
    /// stop always requests deletion and waits for the control-plane result.
    lifecycle: Arc<E2bLifecycle>,
}

/// Shared final-handle ownership for one remote sandbox.
///
/// The cleanup task deliberately does not hold an `Arc<E2bLifecycle>`: doing so
/// would keep the owner alive forever. It owns only the remote client/id and
/// waits for the final lifecycle value to drop its oneshot sender (or for an
/// explicit `stop` to send it).
struct E2bLifecycle {
    stop: Mutex<Option<oneshot::Sender<()>>>,
    keepalive: tokio::task::AbortHandle,
    delete_on_drop: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    stopped_notify: Arc<Notify>,
    cleanup_error: Arc<Mutex<Option<String>>>,
}

impl E2bLifecycle {
    fn supervise<F>(keepalive: tokio::task::AbortHandle, cleanup: F) -> Self
    where
        F: Future<Output = Result<(), IsolationError>> + Send + 'static,
    {
        Self::supervise_with_drop_cleanup(keepalive, cleanup, true)
    }

    fn supervise_preserving_on_drop<F>(keepalive: tokio::task::AbortHandle, cleanup: F) -> Self
    where
        F: Future<Output = Result<(), IsolationError>> + Send + 'static,
    {
        Self::supervise_with_drop_cleanup(keepalive, cleanup, false)
    }

    fn supervise_with_drop_cleanup<F>(
        keepalive: tokio::task::AbortHandle,
        cleanup: F,
        delete_on_drop: bool,
    ) -> Self
    where
        F: Future<Output = Result<(), IsolationError>> + Send + 'static,
    {
        let (stop, stop_requested) = oneshot::channel::<()>();
        let delete_on_drop = Arc::new(AtomicBool::new(delete_on_drop));
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_notify = Arc::new(Notify::new());
        let cleanup_error = Arc::new(Mutex::new(None));
        let supervisor_delete_on_drop = delete_on_drop.clone();
        let supervisor_stopped = stopped.clone();
        let supervisor_notify = stopped_notify.clone();
        let supervisor_error = cleanup_error.clone();
        tokio::spawn(async move {
            // Sending requests an explicit delete. A dropped sender is a final
            // handle drop and follows the current durable-publication
            // disposition instead.
            let explicit_delete = stop_requested.await.is_ok();
            if explicit_delete || supervisor_delete_on_drop.load(Ordering::Acquire) {
                match tokio::spawn(cleanup).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        *supervisor_error
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.to_string());
                        tracing::warn!(error = %error, "failed to delete E2B sandbox");
                    }
                    Err(error) => {
                        *supervisor_error
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(format!("E2B cleanup task failed: {error}"));
                        tracing::warn!(error = %error, "E2B cleanup task failed");
                    }
                }
            }
            supervisor_stopped.store(true, Ordering::Release);
            supervisor_notify.notify_waiters();
        });
        Self {
            stop: Mutex::new(Some(stop)),
            keepalive,
            delete_on_drop,
            stopped,
            stopped_notify,
            cleanup_error,
        }
    }

    /// Switch final-drop behavior from delete to preserve. The daemon calls
    /// this only after the Session's Ready state, exact runtime id, and exact
    /// remote root/data-plane authority are durably published.
    fn preserve_on_drop(&self) {
        self.delete_on_drop.store(false, Ordering::Release);
    }

    /// A paused runtime no longer needs keep-alive, but remains explicitly
    /// deletable if the user later chooses Delete/Rebuild.
    fn preserve_and_abort_keepalive(&self) {
        self.preserve_on_drop();
        self.keepalive.abort();
    }

    fn request_stop(&self) {
        self.keepalive.abort();
        let mut stop = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(stop) = stop.take() {
            let _ = stop.send(());
        }
    }

    async fn stop(&self) {
        let _ = self.stop_checked().await;
    }

    async fn stop_checked(&self) -> Result<(), IsolationError> {
        self.request_stop();
        loop {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            // Register before the second check so a completion between the
            // check and await cannot become a lost notification.
            let notified = self.stopped_notify.notified();
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
        match self
            .cleanup_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            Some(error) => Err(IsolationError::E2b(error)),
            None => Ok(()),
        }
    }
}

impl Drop for E2bLifecycle {
    fn drop(&mut self) {
        self.keepalive.abort();
        if self.delete_on_drop.load(Ordering::Acquire) {
            self.request_stop();
        }
        // In preserve mode, dropping the sender wakes the supervisor with a
        // cancelled receive. It records completion without running DELETE.
    }
}

struct E2bBgHandle {
    id: String,
    command: String,
    status: Arc<Mutex<String>>,
    log: Arc<Mutex<String>>,
}

/// Await provisioning in a detached owned task. If the awaiting request is
/// cancelled, Tokio keeps this task running; its eventual output is then
/// dropped, which activates `E2bLifecycle` cleanup as soon as a remote id has
/// been obtained.
async fn await_owned_start<T, F>(start: F) -> Result<T, IsolationError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, IsolationError>> + Send + 'static,
{
    tokio::spawn(start)
        .await
        .map_err(|error| IsolationError::E2b(format!("E2B sandbox start task failed: {error}")))?
}

async fn await_session_owned_start<T, F>(start: F) -> Result<T, E2bStartError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, E2bStartError>> + Send + 'static,
{
    tokio::spawn(start).await.map_err(|error| {
        E2bStartError::unresolved(IsolationError::E2b(format!(
            "E2B sandbox start task failed: {error}"
        )))
    })?
}

impl E2bSandbox {
    fn owner_start_lock(owner_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        static START_LOCKS: std::sync::OnceLock<
            std::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
        > = std::sync::OnceLock::new();
        let locks = START_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut locks = locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(owner_key).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(owner_key.to_string(), Arc::downgrade(&lock));
        lock
    }

    /// Create a fresh remote sandbox for a session, rooted at `root` (a path
    /// inside the sandbox, e.g. a cloned repo or `/home/user`). `env` is set on
    /// the VM at create time and visible to every later command (e.g. a git
    /// token read by an in-VM credential helper).
    pub async fn start(
        cfg: E2bConfig,
        timeout_secs: u64,
        root: impl Into<PathBuf>,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, IsolationError> {
        Self::start_with_identity_persisted(cfg, timeout_secs, root, env, |_, _| async { Ok(()) })
            .await
    }

    /// Create a sandbox inside an owned task and durably hand its exact id and
    /// data-plane domain to the caller-supplied persistence boundary before
    /// returning a live handle or executing any provisioning command.
    /// Cancelling the awaiting request cannot interrupt this callback halfway
    /// through a successful create.
    pub async fn start_with_identity_persisted<F, Fut>(
        cfg: E2bConfig,
        timeout_secs: u64,
        root: impl Into<PathBuf>,
        env: &BTreeMap<String, String>,
        persist_identity: F,
    ) -> Result<Self, IsolationError>
    where
        F: FnOnce(String, String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), IsolationError>> + Send + 'static,
    {
        let root = root.into();
        let env = env.clone();

        // The owned task is intentional: cancelling the caller detaches this
        // work instead of cancelling it halfway through a successful remote
        // create. Once an id exists, the returned value either reaches the
        // caller or is dropped and its lifecycle supervisor deletes that VM.
        await_owned_start(async move {
            Self::start_owned(cfg, timeout_secs, root, env, None, persist_identity)
                .await
                .map_err(|error| error.error)
        })
        .await
    }

    /// Session-owned start variant. The exact owner lock is acquired before
    /// the detached create task is spawned and remains held until the task has
    /// either persisted an id or reached terminal cleanup. A concurrent
    /// Close/Delete can wait on [`Self::wait_for_owned_start`] and therefore
    /// cannot return while a cancelled request may still create a remote VM.
    pub async fn start_with_identity_persisted_for_owner<C, CFut, F, Fut>(
        owner_key: impl Into<String>,
        cfg: E2bConfig,
        timeout_secs: u64,
        root: impl Into<PathBuf>,
        env: &BTreeMap<String, String>,
        persist_creation: C,
        persist_identity: F,
    ) -> Result<Self, E2bStartError>
    where
        C: FnOnce() -> CFut + Send + 'static,
        CFut: Future<Output = Result<String, IsolationError>> + Send + 'static,
        F: FnOnce(String, String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), IsolationError>> + Send + 'static,
    {
        let owner_key = owner_key.into();
        let owner = Self::owner_start_lock(&owner_key).lock_owned().await;
        let root = root.into();
        let env = env.clone();
        await_session_owned_start(async move {
            let _owner = owner;
            // The owner lock and detached task exist before the durable marker
            // is written. Cancellation before owner registration therefore
            // leaves no forever-ambiguous pre-dispatch token; cancellation
            // after this callback cannot stop the subsequent provider POST.
            let creation_token = persist_creation()
                .await
                .map_err(E2bStartError::pre_dispatch)?;
            Self::start_owned(
                cfg,
                timeout_secs,
                root,
                env,
                Some(creation_token),
                persist_identity,
            )
            .await
        })
        .await
    }

    /// Wait until any detached create task for this Session owner reaches a
    /// terminal identity/cleanup boundary.
    pub async fn wait_for_owned_start(owner_key: &str) {
        let _owner = Self::owner_start_lock(owner_key).lock_owned().await;
    }

    async fn start_owned<F, Fut>(
        cfg: E2bConfig,
        timeout_secs: u64,
        root: PathBuf,
        env: BTreeMap<String, String>,
        creation_token: Option<String>,
        persist_identity: F,
    ) -> Result<Self, E2bStartError>
    where
        F: FnOnce(String, String) -> Fut,
        Fut: Future<Output = Result<(), IsolationError>>,
    {
        let client = E2bClient::new(cfg);
        let session = match creation_token.as_deref() {
            Some(token) => client.create_owned(timeout_secs, &env, token).await?,
            None => client
                .create(timeout_secs, &env)
                .await
                .map_err(E2bStartError::unresolved)?,
        };
        if let Some(token) = creation_token.as_deref() {
            match client
                .verify_metadata_exact(&session.sandbox_id, token)
                .await
            {
                Ok(()) => {}
                Err(E2bRuntimeError::NotFound { .. }) => {
                    return Err(E2bStartError::with_runtime(
                        IsolationError::E2b(format!(
                            "newly created sandbox '{}' disappeared before provider ownership could be verified",
                            session.sandbox_id
                        )),
                        session.sandbox_id,
                        session.data_plane_domain,
                        true,
                    ));
                }
                Err(E2bRuntimeError::ControlPlane(error)) => {
                    return Err(E2bStartError::with_runtime(
                        IsolationError::E2b(format!(
                            "provider ownership of newly created sandbox '{}' could not be verified: {error}",
                            session.sandbox_id
                        )),
                        session.sandbox_id,
                        session.data_plane_domain,
                        false,
                    ));
                }
            }
        }
        if let Err(persist_error) = persist_identity(
            session.sandbox_id.clone(),
            session.data_plane_domain.clone(),
        )
        .await
        {
            let cleanup = match creation_token.as_deref() {
                Some(token) => client
                    .kill_creation_candidate(&session.sandbox_id, token)
                    .await
                    .map(|_| ()),
                None => client.kill(&session.sandbox_id).await,
            };
            return match cleanup {
                Ok(()) => Err(E2bStartError::with_runtime(
                    persist_error,
                    session.sandbox_id,
                    session.data_plane_domain,
                    true,
                )),
                Err(cleanup_error) => Err(E2bStartError::with_runtime(
                    IsolationError::E2b(format!(
                        "{persist_error}; deleting sandbox '{}' after identity persistence failed also failed: {cleanup_error}",
                        session.sandbox_id
                    )),
                    session.sandbox_id,
                    session.data_plane_domain,
                    false,
                )),
            };
        }

        Ok(Self::from_live_session(
            client,
            session,
            timeout_secs,
            root,
            true,
            creation_token,
        ))
    }

    fn from_live_session(
        client: E2bClient,
        session: E2bSession,
        timeout_secs: u64,
        root: PathBuf,
        delete_on_drop: bool,
        ownership_token: Option<String>,
    ) -> Self {
        // Keep the VM alive while this handle lives: re-extend the TTL to
        // `timeout_secs` every half-life (min 30s) so it never lapses under an
        // active session. Best-effort; a failed extension just retries next tick.
        let ka_client = client.clone();
        let ka_id = session.sandbox_id.clone();
        let period = Duration::from_secs((timeout_secs / 2).max(30));
        let keepalive = tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let _ = ka_client.set_timeout(&ka_id, timeout_secs).await;
            }
        })
        .abort_handle();
        let cleanup_client = client.clone();
        let cleanup_id = session.sandbox_id.clone();
        let cleanup_token = ownership_token.clone();
        let cleanup = async move {
            match cleanup_token.as_deref() {
                Some(token) => cleanup_client
                    .kill_creation_candidate(&cleanup_id, token)
                    .await
                    .map(|_| ()),
                None => cleanup_client.kill(&cleanup_id).await,
            }
        };
        let lifecycle = Arc::new(if delete_on_drop {
            E2bLifecycle::supervise(keepalive, cleanup)
        } else {
            E2bLifecycle::supervise_preserving_on_drop(keepalive, cleanup)
        });

        Self {
            client,
            session,
            ownership_token,
            root,
            tasks: Mutex::new(Vec::new()),
            terminals: Mutex::new(Vec::new()),
            lifecycle,
        }
    }

    /// The underlying sandbox id.
    pub fn sandbox_id(&self) -> &str {
        &self.session.sandbox_id
    }

    /// Exact data-plane authority returned by the provider (or the configured
    /// compatibility fallback when the provider omits it).
    pub fn data_plane_domain(&self) -> &str {
        &self.session.data_plane_domain
    }

    /// Reattach to one exact durably persisted remote runtime and root. This
    /// never creates or clones anything. Reattached handles preserve the VM on
    /// final drop from their first moment of ownership.
    pub async fn reattach_exact(
        cfg: E2bConfig,
        timeout_secs: u64,
        root: impl Into<PathBuf>,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<Self, E2bRuntimeError> {
        let client = E2bClient::new(cfg);
        let session = client
            .connect_owned_exact(sandbox_id, ownership_token, timeout_secs)
            .await?;
        Ok(Self::from_live_session(
            client,
            session,
            timeout_secs,
            root.into(),
            false,
            Some(ownership_token.to_string()),
        ))
    }

    /// Mark a newly-created handle as durably Ready. Final handle drop will
    /// abort keep-alive but preserve the remote runtime for exact reattachment;
    /// an explicit checked stop still deletes it.
    pub fn preserve_on_drop(&self) {
        self.lifecycle.preserve_on_drop();
    }

    /// Pause this exact runtime without deleting it. The preserve disposition
    /// is installed only after the control plane confirms pause, so a failed
    /// request leaves the existing keep-alive and lifecycle ownership intact.
    pub async fn pause_checked(&self) -> Result<(), E2bRuntimeError> {
        match self.ownership_token.as_deref() {
            Some(token) => {
                self.client
                    .pause_owned_exact(&self.session.sandbox_id, token)
                    .await?
            }
            None => self.client.pause_exact(&self.session.sandbox_id).await?,
        }
        self.lifecycle.preserve_and_abort_keepalive();
        Ok(())
    }

    /// Pause an exact persisted runtime when no executable handle is present.
    pub async fn pause_persisted(
        cfg: E2bConfig,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<(), E2bRuntimeError> {
        E2bClient::new(cfg)
            .pause_owned_exact(sandbox_id, ownership_token)
            .await
    }

    /// Delete an exact persisted remote sandbox identity without first
    /// reconstructing an executable handle. Interrupted/Failed preparation and
    /// explicit Delete/Rebuild use this path; a missing sandbox is idempotent.
    pub async fn delete_persisted(
        cfg: E2bConfig,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<(), IsolationError> {
        E2bClient::new(cfg)
            .kill_owned(sandbox_id, ownership_token)
            .await
    }

    /// Delete one id selected by a freshly repeated creation-token discovery,
    /// or one previously fsynced hint when that discovery returned no live
    /// matches. The exact provider object must still carry the token. Unlike
    /// [`Self::delete_persisted`], this supports deleting every member of a
    /// genuinely ambiguous multi-sandbox create rather than requiring the
    /// token to resolve to a singleton.
    pub async fn delete_persisted_creation_candidate(
        cfg: E2bConfig,
        sandbox_id: &str,
        ownership_token: &str,
    ) -> Result<E2bCreationCandidateCleanup, IsolationError> {
        E2bClient::new(cfg)
            .kill_creation_candidate(sandbox_id, ownership_token)
            .await
    }

    /// List live remote ids carrying one exact durable pre-create token.
    pub async fn discover_persisted_creation(
        cfg: E2bConfig,
        creation_token: &str,
    ) -> Result<Vec<String>, IsolationError> {
        E2bClient::new(cfg)
            .discover_creation_token(creation_token)
            .await
    }
}

#[async_trait::async_trait]
impl Sandbox for E2bSandbox {
    fn root(&self) -> &Path {
        &self.root
    }

    fn runtime_id(&self) -> Option<&str> {
        Some(&self.session.sandbox_id)
    }

    fn preserve_on_drop(&self) {
        E2bSandbox::preserve_on_drop(self);
    }

    async fn exec(&self, argv: &[&str], timeout: Duration) -> Result<ExecResult, IsolationError> {
        self.client
            .exec(&self.session, argv, &self.root.to_string_lossy(), timeout)
            .await
    }

    async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.client
            .exec_stdin(
                &self.session,
                argv,
                stdin,
                &self.root.to_string_lossy(),
                timeout,
            )
            .await
    }

    fn spawn_background(&self, command: &str) -> String {
        let id = format!("task-{}", uuid::Uuid::new_v4());
        let status = Arc::new(Mutex::new("running".to_string()));
        let log = Arc::new(Mutex::new(String::new()));
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(E2bBgHandle {
                id: id.clone(),
                command: command.to_string(),
                status: status.clone(),
                log: log.clone(),
            });
        }
        let client = self.client.clone();
        let session = self.session.clone();
        let cwd = self.root.to_string_lossy().to_string();
        let command = command.to_string();
        tokio::spawn(async move {
            client
                .run_to_log(&session, &command, &cwd, log, status)
                .await;
        });
        id
    }

    fn spawn_pty(
        &self,
        command: &str,
        rows: u16,
        cols: u16,
    ) -> Result<std::sync::Arc<crate::pty::PtyTerminal>, String> {
        use crate::pty::{PtyOutput, PtyTerminal};
        // The terminal id doubles as the envd process `tag` for input/resize/kill.
        let id = format!("term-{}", uuid::Uuid::new_v4());
        let output = PtyOutput::new(256);
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(Mutex::new(true));
        let cwd = self.root.to_string_lossy().to_string();

        // Output pump: open the PTY stream and feed scrollback + broadcast.
        {
            let client = self.client.clone();
            let session = self.session.clone();
            let (tag, command) = (id.clone(), command.to_string());
            let (output, al) = (output.clone(), alive.clone());
            tokio::spawn(async move {
                match client
                    .pty_start(&session, &tag, &command, &cwd, cols, rows)
                    .await
                {
                    Ok(resp) => pump_pty_output(resp, output, al).await,
                    Err(_) => {
                        if let Ok(mut a) = al.lock() {
                            *a = false;
                        }
                    }
                }
            });
        }

        // Input drain: the WS bridge writes to a std mpsc; bridge it to async and
        // forward each chunk to SendInput.
        {
            let client = self.client.clone();
            let session = self.session.clone();
            let tag = id.clone();
            let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            std::thread::spawn(move || {
                while let Ok(bytes) = input_rx.recv() {
                    if async_tx.send(bytes).is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                while let Some(bytes) = async_rx.recv().await {
                    let _ = client.pty_input(&session, &tag, &bytes).await;
                }
            });
        }

        // Resize hook: fire an async Update (xterm.js sends rows, cols).
        let resize_hook: Box<dyn Fn(u16, u16) + Send + Sync> = {
            let client = self.client.clone();
            let session = self.session.clone();
            let tag = id.clone();
            Box::new(move |rows, cols| {
                let (client, session, tag) = (client.clone(), session.clone(), tag.clone());
                tokio::spawn(async move {
                    let _ = client.pty_resize(&session, &tag, cols, rows).await;
                });
            })
        };

        let term = Arc::new(PtyTerminal::from_parts(
            id,
            command.to_string(),
            output,
            input_tx,
            alive,
            resize_hook,
        ));
        if let Ok(mut t) = self.terminals.lock() {
            t.push(term.clone());
        }
        Ok(term)
    }

    fn get_terminal(&self, id: &str) -> Option<std::sync::Arc<crate::pty::PtyTerminal>> {
        self.terminals
            .lock()
            .ok()?
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }

    fn kill_terminal(&self, id: &str) -> bool {
        let mut terms = match self.terminals.lock() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let Some(pos) = terms.iter().position(|t| t.id == id) else {
            return false;
        };
        let term = terms.remove(pos);
        drop(terms);
        if let Ok(mut a) = term.alive.lock() {
            *a = false;
        }
        // Best-effort remote kill (tag == id). Dropping the terminal also drops
        // the output/input tasks' senders, closing them.
        let client = self.client.clone();
        let session = self.session.clone();
        let tag = id.to_string();
        tokio::spawn(async move {
            let _ = client.pty_kill(&session, &tag).await;
        });
        true
    }

    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        self.terminals
            .lock()
            .map(|terms| {
                terms
                    .iter()
                    .map(|t| (t.id.clone(), t.command.clone(), t.is_alive()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list_tasks(&self) -> Vec<BgTask> {
        self.tasks
            .lock()
            .map(|tasks| {
                tasks
                    .iter()
                    .map(|h| BgTask {
                        id: h.id.clone(),
                        command: h.command.clone(),
                        status: h.status.lock().map(|s| s.clone()).unwrap_or_default(),
                        log: h.log.lock().map(|l| l.clone()).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn with_root(&self, root: &Path) -> Arc<dyn Sandbox> {
        // Same remote microVM (shared client + session), re-rooted at a worktree.
        // Fresh task list, but shared lifecycle ownership: dropping either view
        // leaves the VM running while the other exists; final-drop behavior is
        // the shared create-owned or durably-preserved disposition.
        Arc::new(E2bSandbox {
            client: self.client.clone(),
            session: self.session.clone(),
            ownership_token: self.ownership_token.clone(),
            root: root.to_path_buf(),
            tasks: Mutex::new(Vec::new()),
            terminals: Mutex::new(Vec::new()),
            lifecycle: self.lifecycle.clone(),
        })
    }

    async fn stop(&self) {
        self.lifecycle.stop().await;
    }

    async fn stop_checked(&self) -> Result<(), IsolationError> {
        match self.lifecycle.stop_checked().await {
            Ok(()) => Ok(()),
            Err(first_error) => {
                let retry = match self.ownership_token.as_deref() {
                    Some(token) => self
                        .client
                        .kill_creation_candidate(&self.session.sandbox_id, token)
                        .await
                        .map(|_| ()),
                    None => self.client.kill(&self.session.sandbox_id).await,
                };
                retry.map_err(|retry_error| {
                    IsolationError::E2b(format!(
                        "initial E2B cleanup failed ({first_error}); retry failed ({retry_error})"
                    ))
                })
            }
        }
    }

    async fn pause_checked(&self) -> Result<(), IsolationError> {
        E2bSandbox::pause_checked(self)
            .await
            .map_err(|error| IsolationError::E2b(error.to_string()))
    }
}

/// A Connect stream frame: a normal message, or the end-of-stream frame.
enum Frame {
    Event(serde_json::Value),
    End(serde_json::Value),
}

/// Incremental decoder for Connect's `[flag][len][payload]` framed stream.
struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), IsolationError> {
        let buffered =
            self.buf.len().checked_add(bytes.len()).ok_or_else(|| {
                IsolationError::E2b("envd stream buffer length overflow".to_string())
            })?;
        if buffered > CONNECT_BUFFER_MAX_BYTES {
            return Err(IsolationError::E2b(format!(
                "envd stream buffer exceeded {CONNECT_BUFFER_MAX_BYTES} bytes"
            )));
        }
        self.buf.extend_from_slice(bytes);
        self.validate_declared_frame_length()
    }

    fn validate_declared_frame_length(&self) -> Result<(), IsolationError> {
        if self.buf.len() < 5 {
            return Ok(());
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > CONNECT_FRAME_MAX_BYTES {
            return Err(IsolationError::E2b(format!(
                "envd frame declared {len} bytes, exceeding the {CONNECT_FRAME_MAX_BYTES}-byte limit"
            )));
        }
        Ok(())
    }

    /// Pop the next complete frame, or `None` if the buffer holds a partial one.
    fn next_frame(&mut self) -> Result<Option<Frame>, IsolationError> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > CONNECT_FRAME_MAX_BYTES {
            return Err(IsolationError::E2b(format!(
                "envd frame declared {len} bytes, exceeding the {CONNECT_FRAME_MAX_BYTES}-byte limit"
            )));
        }
        let frame_len = 5_usize
            .checked_add(len)
            .ok_or_else(|| IsolationError::E2b("envd frame length overflow".to_string()))?;
        if self.buf.len() < frame_len {
            return Ok(None);
        }
        let flag = self.buf[0];
        let v: serde_json::Value = serde_json::from_slice(&self.buf[5..frame_len])
            .map_err(|e| IsolationError::E2b(format!("bad envd frame: {e}")))?;
        self.buf.drain(..frame_len);
        self.validate_declared_frame_length()?;
        Ok(Some(if flag & 0x02 != 0 {
            Frame::End(v)
        } else {
            Frame::Event(v)
        }))
    }
}

/// Accumulates `ProcessEvent`s into an [`ExecResult`].
#[derive(Default)]
struct ExecOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    /// `None` until an `end` event is seen; the exit code (0 when omitted).
    exit_code: Option<i32>,
    saw_start: bool,
}

impl ExecOutput {
    fn apply(&mut self, v: &serde_json::Value) {
        let event = &v["event"];
        if event.get("start").is_some() {
            self.saw_start = true;
        } else if let Some(data) = event.get("data") {
            if let Some(s) = data.get("stdout").and_then(|x| x.as_str()) {
                append_base64_output(&mut self.stdout, &mut self.stdout_truncated, s);
            }
            if let Some(s) = data.get("stderr").and_then(|x| x.as_str()) {
                append_base64_output(&mut self.stderr, &mut self.stderr_truncated, s);
            }
        } else if let Some(end) = event.get("end") {
            // envd omits `exitCode` when it is 0 (proto3 default-value omission).
            self.exit_code = Some(end.get("exitCode").and_then(|x| x.as_i64()).unwrap_or(0) as i32);
        }
    }

    fn into_result(self) -> ExecResult {
        ExecResult {
            stdout: captured_output_text(&self.stdout, self.stdout_truncated, "stdout"),
            stderr: captured_output_text(&self.stderr, self.stderr_truncated, "stderr"),
            // -1 signals no `end` event was seen (a truncated/incomplete stream).
            exit_code: self.exit_code.unwrap_or(-1),
        }
    }
}

fn append_base64_output(target: &mut Vec<u8>, truncated: &mut bool, encoded: &str) {
    let remaining = COMMAND_OUTPUT_MAX_BYTES.saturating_sub(target.len());
    // Decode through a fixed scratch buffer and continue to EOF even after the
    // retained prefix is full. This validates the whole event while limiting
    // temporary allocation to at most the remaining per-stream allowance.
    let engine = base64::engine::general_purpose::STANDARD;
    let mut decoder = base64::read::DecoderReader::new(encoded.as_bytes(), &engine);
    let mut retained = Vec::with_capacity(remaining.min(16 * 1024));
    let mut decoded_bytes = 0_usize;
    let mut scratch = [0_u8; 16 * 1024];
    loop {
        let read = match decoder.read(&mut scratch) {
            Ok(0) => break,
            Ok(read) => read,
            // Preserve the previous behavior: an invalid base64 event
            // contributes no bytes at all.
            Err(_) => return,
        };
        decoded_bytes = decoded_bytes.saturating_add(read);
        if retained.len() < remaining {
            let keep = (remaining - retained.len()).min(read);
            retained.extend_from_slice(&scratch[..keep]);
        }
    }
    target.extend_from_slice(&retained);
    *truncated |= decoded_bytes > remaining;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct FakeControlPlane {
        api_url: String,
        requests: tokio::sync::mpsc::UnboundedReceiver<String>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeControlPlane {
        async fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind fake E2B control plane");
            let address = listener.local_addr().expect("fake server address");
            let (requests_tx, requests) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                for response in responses {
                    let (mut socket, _) = listener.accept().await.expect("accept E2B request");
                    let request = read_http_request(&mut socket).await;
                    requests_tx.send(request).expect("record E2B request");
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write E2B response");
                    socket.shutdown().await.expect("close E2B response");
                }
            });
            Self {
                api_url: format!("http://{address}"),
                requests,
                task,
            }
        }

        fn config(&self) -> E2bConfig {
            E2bConfig {
                api_url: self.api_url.clone(),
                api_key: "test-key".to_string(),
                template: "base".to_string(),
                domain: "sandbox.test".to_string(),
            }
        }

        async fn next_request(&mut self) -> String {
            tokio::time::timeout(Duration::from_secs(1), self.requests.recv())
                .await
                .expect("fake E2B request should arrive")
                .expect("fake E2B server should remain available")
        }

        async fn assert_no_request(&mut self) {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), self.requests.recv())
                    .await
                    .is_err(),
                "no additional E2B control-plane request was expected"
            );
        }
    }

    impl Drop for FakeControlPlane {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("read E2B request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= header_end + content_length {
                break;
            }
        }
        String::from_utf8(request).expect("E2B test request is UTF-8")
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn session_response(sandbox_id: &str) -> String {
        http_response(
            "200 OK",
            &json!({
                "sandboxID": sandbox_id,
                "envdAccessToken": "envd-token",
                "domain": "sandbox.test"
            })
            .to_string(),
        )
    }

    fn listed_session_response(sandbox_id: &str, creation_token: &str) -> String {
        http_response(
            "200 OK",
            &json!([{
                "sandboxID": sandbox_id,
                "state": "running",
                "metadata": {
                    AXOCOATL_CREATION_TOKEN_METADATA_KEY: creation_token
                }
            }])
            .to_string(),
        )
    }

    fn owned_sandbox_response(sandbox_id: &str, ownership_token: &str, state: &str) -> String {
        http_response(
            "200 OK",
            &json!({
                "sandboxID": sandbox_id,
                "state": state,
                "metadata": {
                    AXOCOATL_CREATION_TOKEN_METADATA_KEY: ownership_token
                }
            })
            .to_string(),
        )
    }

    fn frame(v: serde_json::Value) -> Vec<u8> {
        connect_frame(&serde_json::to_vec(&v).unwrap())
    }

    fn end_frame() -> Vec<u8> {
        let mut f = vec![0x02];
        f.extend_from_slice(&2u32.to_be_bytes());
        f.extend_from_slice(b"{}");
        f
    }

    fn decode_all(bytes: &[u8]) -> ExecOutput {
        let mut dec = FrameDecoder::new();
        dec.push(bytes).unwrap();
        let mut out = ExecOutput::default();
        while let Some(f) = dec.next_frame().unwrap() {
            if let Frame::Event(v) = f {
                out.apply(&v);
            }
        }
        out
    }

    #[test]
    fn connect_frame_prefixes_length() {
        assert_eq!(connect_frame(b"hi"), vec![0x00, 0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn parses_the_verified_nonzero_exit_stream() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut body = Vec::new();
        body.extend(frame(json!({"event":{"start":{"pid":1319}}})));
        body.extend(frame(
            json!({"event":{"data":{"stderr": b64.encode("oops")}}}),
        ));
        body.extend(frame(
            json!({"event":{"data":{"stdout": b64.encode("hello")}}}),
        ));
        body.extend(frame(
            json!({"event":{"end":{"exitCode":7,"exited":true,"status":"exit status 7"}}}),
        ));
        body.extend(end_frame());
        let r = decode_all(&body).into_result();
        assert_eq!(r.stdout, "hello");
        assert_eq!(r.stderr, "oops");
        assert_eq!(r.exit_code, 7);
    }

    #[test]
    fn exit_zero_has_no_exit_code_field() {
        // The exact shape envd emits for a successful command — `exitCode` absent.
        let mut body = Vec::new();
        body.extend(frame(json!({"event":{"start":{"pid":1}}})));
        body.extend(frame(
            json!({"event":{"end":{"exited":true,"status":"exit status 0"}}}),
        ));
        body.extend(end_frame());
        assert_eq!(decode_all(&body).into_result().exit_code, 0);
    }

    #[test]
    fn exec_output_caps_each_stream_and_marks_utf8_safe_truncation() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut stdout = vec![b'a'; COMMAND_OUTPUT_MAX_BYTES - 1];
        stdout.extend_from_slice("🦎".as_bytes());
        let stderr = vec![b'e'; COMMAND_OUTPUT_MAX_BYTES + 17];
        let mut out = ExecOutput::default();
        out.apply(&json!({"event":{"data":{
            "stdout": b64.encode(stdout),
            "stderr": b64.encode(stderr),
        }}}));

        assert_eq!(out.stdout.len(), COMMAND_OUTPUT_MAX_BYTES);
        assert_eq!(out.stderr.len(), COMMAND_OUTPUT_MAX_BYTES);
        assert!(out.stdout_truncated);
        assert!(out.stderr_truncated);
        let result = out.into_result();
        assert!(result.stdout_truncated());
        assert!(result.stderr_truncated());
        assert!(!result.stdout.contains('\u{fffd}'));
    }

    #[test]
    fn invalid_base64_event_is_discarded_without_partial_output() {
        let mut out = ExecOutput::default();
        out.apply(&json!({"event":{"data":{"stdout":"aGVsbG8=!"}}}));
        assert!(out.stdout.is_empty());
        assert!(!out.stdout_truncated);
    }

    #[test]
    fn frame_decoder_rejects_oversized_declared_frame_immediately() {
        let declared = (CONNECT_FRAME_MAX_BYTES as u32 + 1).to_be_bytes();
        let mut header = vec![0];
        header.extend_from_slice(&declared);
        let mut decoder = FrameDecoder::new();
        let error = decoder.push(&header).unwrap_err();
        assert!(error.to_string().contains("exceeding"));
        assert_eq!(decoder.buf.len(), 5);
    }

    #[test]
    fn frame_decoder_handles_split_chunks() {
        // A frame delivered across two stream chunks must still decode.
        let full = frame(json!({"event":{"start":{"pid":9}}}));
        let (a, b) = full.split_at(3);
        let mut dec = FrameDecoder::new();
        dec.push(a).unwrap();
        assert!(dec.next_frame().unwrap().is_none());
        dec.push(b).unwrap();
        assert!(matches!(dec.next_frame().unwrap(), Some(Frame::Event(_))));
    }

    #[test]
    fn end_stream_error_surfaces() {
        let mut dec = FrameDecoder::new();
        dec.push(&frame(
            json!({"error":{"code":"internal","message":"boom"}}),
        ))
        .unwrap();
        // reclassify the last frame as an End frame by flipping the flag byte
        let mut raw = frame(json!({"error":{"code":"internal"}}));
        raw[0] = 0x02;
        dec.push(&raw).unwrap();
        // drain to the end frame and assert error handling in run_inner's logic
        let mut saw_err = false;
        while let Some(f) = dec.next_frame().unwrap() {
            if let Frame::End(v) = f {
                if v.get("error").is_some() {
                    saw_err = true;
                }
            }
        }
        assert!(saw_err);
    }

    fn lifecycle_test_sandbox(lifecycle: Arc<E2bLifecycle>, root: &str) -> E2bSandbox {
        E2bSandbox {
            client: E2bClient::new(E2bConfig {
                api_url: "http://unused.invalid".into(),
                api_key: "unused".into(),
                template: "unused".into(),
                domain: "unused.invalid".into(),
            }),
            session: E2bSession {
                sandbox_id: "sandbox-for-lifecycle-test".into(),
                data_plane_domain: "unused.invalid".into(),
                envd_base: "http://unused.invalid".into(),
                access_token: None,
            },
            ownership_token: None,
            root: root.into(),
            tasks: Mutex::new(Vec::new()),
            terminals: Mutex::new(Vec::new()),
            lifecycle,
        }
    }

    async fn wait_for_cleanup(cleanups: &AtomicUsize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanups.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle cleanup should finish");
    }

    #[tokio::test]
    async fn rooted_views_share_lifecycle_and_final_drop_cleans_up_once() {
        let keepalive = tokio::spawn(std::future::pending::<()>());
        let cleanups = Arc::new(AtomicUsize::new(0));
        let cleanup_count = cleanups.clone();
        let lifecycle = Arc::new(E2bLifecycle::supervise(
            keepalive.abort_handle(),
            async move {
                cleanup_count.fetch_add(1, Ordering::AcqRel);
                Ok(())
            },
        ));
        let primary = lifecycle_test_sandbox(lifecycle, "/home/user");
        let rooted = primary.with_root(Path::new("/home/user/repo"));

        drop(primary);
        tokio::task::yield_now().await;
        assert_eq!(
            cleanups.load(Ordering::Acquire),
            0,
            "a rooted view still owns the remote sandbox"
        );

        drop(rooted);
        wait_for_cleanup(&cleanups).await;
        assert_eq!(cleanups.load(Ordering::Acquire), 1);
        assert!(
            keepalive.await.unwrap_err().is_cancelled(),
            "final drop must abort keep-alive"
        );
    }

    #[tokio::test]
    async fn explicit_stop_from_any_view_waits_for_one_shared_cleanup() {
        let keepalive = tokio::spawn(std::future::pending::<()>());
        let cleanups = Arc::new(AtomicUsize::new(0));
        let cleanup_count = cleanups.clone();
        let lifecycle = Arc::new(E2bLifecycle::supervise(
            keepalive.abort_handle(),
            async move {
                tokio::task::yield_now().await;
                cleanup_count.fetch_add(1, Ordering::AcqRel);
                Ok(())
            },
        ));
        let primary = lifecycle_test_sandbox(lifecycle, "/home/user");
        let rooted = primary.with_root(Path::new("/home/user/repo"));

        rooted.stop().await;
        assert_eq!(cleanups.load(Ordering::Acquire), 1);
        primary.stop().await;
        drop(rooted);
        drop(primary);
        tokio::task::yield_now().await;
        assert_eq!(
            cleanups.load(Ordering::Acquire),
            1,
            "repeated stop and final drop must not issue duplicate cleanup"
        );
        assert!(keepalive.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn checked_stop_reports_one_shared_remote_cleanup_failure() {
        let keepalive = tokio::spawn(std::future::pending::<()>());
        let lifecycle = Arc::new(E2bLifecycle::supervise(keepalive.abort_handle(), async {
            Err(IsolationError::E2b("delete refused".to_string()))
        }));
        let first = lifecycle
            .stop_checked()
            .await
            .expect_err("delete must fail");
        assert!(first.to_string().contains("delete refused"));
        let retry = lifecycle
            .stop_checked()
            .await
            .expect_err("shared cleanup result must stay failed");
        assert!(retry.to_string().contains("delete refused"));
        assert!(keepalive.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn ready_created_handle_drop_preserves_remote_but_checked_stop_deletes_once() {
        let mut preserved_server = FakeControlPlane::start(vec![
            session_response("created-ready"),
            http_response("204 No Content", ""),
        ])
        .await;
        let (identity_tx, identity_rx) = oneshot::channel();
        let preserved = E2bSandbox::start_with_identity_persisted(
            preserved_server.config(),
            3600,
            "/home/user/repository",
            &BTreeMap::new(),
            move |sandbox_id, domain| async move {
                let _ = identity_tx.send((sandbox_id, domain));
                Ok(())
            },
        )
        .await
        .expect("create E2B sandbox");
        assert_eq!(
            identity_rx.await.expect("persisted identity callback"),
            ("created-ready".to_string(), "sandbox.test".to_string())
        );
        let create_request = preserved_server.next_request().await;
        assert!(create_request.starts_with("POST /sandboxes HTTP/1.1"));
        assert!(
            create_request.contains("\"autoPause\":true"),
            "created E2B runtimes must pause rather than delete at TTL"
        );
        preserved.preserve_on_drop();
        drop(preserved);
        preserved_server.assert_no_request().await;

        let mut stopped_server = FakeControlPlane::start(vec![
            session_response("created-stopped"),
            http_response("204 No Content", ""),
        ])
        .await;
        let stopped = E2bSandbox::start(
            stopped_server.config(),
            3600,
            "/home/user/repository",
            &BTreeMap::new(),
        )
        .await
        .expect("create E2B sandbox");
        let _ = stopped_server.next_request().await;
        stopped.preserve_on_drop();
        Sandbox::stop_checked(&stopped)
            .await
            .expect("explicit checked stop deletes a preserved runtime");
        let delete_request = stopped_server.next_request().await;
        assert!(delete_request.starts_with("DELETE /sandboxes/created-stopped HTTP/1.1"));
        drop(stopped);
    }

    #[tokio::test]
    async fn dropped_create_response_is_recovered_by_exact_metadata_token() {
        let creation_token = "session-7-generation-3-unique";
        let mut server = FakeControlPlane::start(vec![
            String::new(),
            listed_session_response("committed-id", creation_token),
            session_response("committed-id"),
            owned_sandbox_response("committed-id", creation_token, "running"),
            http_response("204 No Content", ""),
        ])
        .await;
        let (identity_tx, identity_rx) = oneshot::channel();
        let sandbox = E2bSandbox::start_with_identity_persisted_for_owner(
            "session-7",
            server.config(),
            3600,
            "/home/user",
            &BTreeMap::new(),
            || async { Ok(creation_token.to_string()) },
            move |sandbox_id, domain| async move {
                let _ = identity_tx.send((sandbox_id, domain));
                Ok(())
            },
        )
        .await
        .expect("metadata discovery should recover the committed sandbox");

        assert_eq!(sandbox.sandbox_id(), "committed-id");
        assert_eq!(
            identity_rx.await.expect("durable identity callback"),
            ("committed-id".to_string(), "sandbox.test".to_string())
        );
        let create = server.next_request().await;
        assert!(create.starts_with("POST /sandboxes HTTP/1.1"));
        assert!(create.contains(&format!(
            "\"{AXOCOATL_CREATION_TOKEN_METADATA_KEY}\":\"{creation_token}\""
        )));
        let discover = server.next_request().await;
        assert!(discover.starts_with("GET /v2/sandboxes?"));
        assert!(
            discover.contains("metadata=axocoatl_creation_token%3Dsession-7-generation-3-unique")
        );
        assert!(discover.contains("state=running%2Cpaused"));
        let connect = server.next_request().await;
        assert!(connect.starts_with("POST /sandboxes/committed-id/connect HTTP/1.1"));
        let ownership = server.next_request().await;
        assert!(ownership.starts_with("GET /sandboxes/committed-id HTTP/1.1"));
        sandbox.preserve_on_drop();
        drop(sandbox);
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn zero_first_discovery_does_not_prevent_later_exact_cleanup() {
        let creation_token = "session-late-generation-8-unique";
        let mut server = FakeControlPlane::start(vec![
            String::new(),
            http_response("200 OK", "[]"),
            listed_session_response("late-committed-id", creation_token),
            listed_session_response("late-committed-id", creation_token),
            owned_sandbox_response("late-committed-id", creation_token, "running"),
            http_response("204 No Content", ""),
        ])
        .await;
        let config = server.config();
        let error = match E2bSandbox::start_with_identity_persisted_for_owner(
            "session-late",
            config.clone(),
            3600,
            "/home/user",
            &BTreeMap::new(),
            || async { Ok(creation_token.to_string()) },
            |_, _| async { Ok(()) },
        )
        .await
        {
            Ok(_) => panic!("the first empty reconciliation must fail closed"),
            Err(error) => error,
        };
        assert!(error.runtime.is_none());
        assert!(error.to_string().contains("retained the token"));
        let _create = server.next_request().await;
        let _first_list = server.next_request().await;

        let discovered = E2bSandbox::discover_persisted_creation(config.clone(), creation_token)
            .await
            .expect("later reconciliation sees the committed sandbox");
        assert_eq!(discovered, vec!["late-committed-id"]);
        let second_list = server.next_request().await;
        assert!(second_list.starts_with("GET /v2/sandboxes?"));
        E2bSandbox::delete_persisted(config, &discovered[0], creation_token)
            .await
            .expect("delete the exact late sandbox");
        let rediscovery = server.next_request().await;
        assert!(rediscovery.starts_with("GET /v2/sandboxes?"));
        let proof = server.next_request().await;
        assert!(proof.starts_with("GET /sandboxes/late-committed-id HTTP/1.1"));
        let delete = server.next_request().await;
        assert!(delete.starts_with("DELETE /sandboxes/late-committed-id HTTP/1.1"));
    }

    #[tokio::test]
    async fn authoritative_create_rejection_is_not_treated_as_an_allocated_sandbox() {
        let creation_token = "session-rejected-generation-1";
        let mut server = FakeControlPlane::start(vec![
            http_response("400 Bad Request", "invalid template"),
            http_response("200 OK", "[]"),
        ])
        .await;
        let error = match E2bSandbox::start_with_identity_persisted_for_owner(
            "session-rejected",
            server.config(),
            3600,
            "/home/user",
            &BTreeMap::new(),
            || async { Ok(creation_token.to_string()) },
            |_, _| async { Ok(()) },
        )
        .await
        {
            Ok(_) => panic!("an invalid create request must fail"),
            Err(error) => error,
        };
        assert!(!error.creation_ambiguous);
        assert!(error.runtime.is_none());
        assert!(error.to_string().contains("400 Bad Request"));
        let create = server.next_request().await;
        assert!(create.starts_with("POST /sandboxes HTTP/1.1"));
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn cancellation_before_owner_registration_persists_no_marker_and_sends_no_post() {
        let owner_key = "session-cancel-before-owner";
        let owner = E2bSandbox::owner_start_lock(owner_key).lock_owned().await;
        let mut server = FakeControlPlane::start(vec![session_response("must-not-create")]).await;
        let config = server.config();
        let marker_persisted = Arc::new(AtomicBool::new(false));
        let marker_probe = marker_persisted.clone();
        let caller = tokio::spawn(async move {
            E2bSandbox::start_with_identity_persisted_for_owner(
                owner_key,
                config,
                3600,
                "/home/user",
                &BTreeMap::new(),
                move || async move {
                    marker_probe.store(true, Ordering::Release);
                    Ok("unused-token".to_string())
                },
                |_, _| async { Ok(()) },
            )
            .await
        });
        tokio::task::yield_now().await;
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        drop(owner);
        tokio::task::yield_now().await;
        assert!(!marker_persisted.load(Ordering::Acquire));
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn persistence_and_rollback_failure_returns_structured_exact_identity() {
        let creation_token = "session-9-generation-4-unique";
        let mut server = FakeControlPlane::start(vec![
            session_response("unpublishable-id"),
            owned_sandbox_response("unpublishable-id", creation_token, "running"),
            owned_sandbox_response("unpublishable-id", creation_token, "running"),
            http_response("503 Service Unavailable", "delete unavailable"),
        ])
        .await;
        let error = match E2bSandbox::start_with_identity_persisted_for_owner(
            "session-9",
            server.config(),
            3600,
            "/home/user",
            &BTreeMap::new(),
            || async { Ok(creation_token.to_string()) },
            |_, _| async {
                Err(IsolationError::E2b(
                    "durable identity write failed".to_string(),
                ))
            },
        )
        .await
        {
            Ok(_) => panic!("persistence plus checked rollback must fail"),
            Err(error) => error,
        };

        let runtime = error
            .runtime
            .as_ref()
            .expect("exact identity must escape as data");
        assert_eq!(runtime.sandbox_id, "unpublishable-id");
        assert_eq!(runtime.data_plane_domain, "sandbox.test");
        assert!(!error.cleanup_confirmed);
        assert!(error.to_string().contains("durable identity write failed"));
        assert!(error.to_string().contains("delete unavailable"));
        let create = server.next_request().await;
        assert!(create.contains(&format!(
            "\"{AXOCOATL_CREATION_TOKEN_METADATA_KEY}\":\"{creation_token}\""
        )));
        let verify = server.next_request().await;
        assert!(verify.starts_with("GET /sandboxes/unpublishable-id HTTP/1.1"));
        let rollback_proof = server.next_request().await;
        assert!(rollback_proof.starts_with("GET /sandboxes/unpublishable-id HTTP/1.1"));
        let delete = server.next_request().await;
        assert!(delete.starts_with("DELETE /sandboxes/unpublishable-id HTTP/1.1"));
    }

    #[tokio::test]
    async fn exact_reattach_uses_persisted_id_and_root_and_never_deletes_on_drop() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut server = FakeControlPlane::start(vec![
            listed_session_response("persisted-id", ownership_token),
            owned_sandbox_response("persisted-id", ownership_token, "paused"),
            session_response("persisted-id"),
            http_response("204 No Content", ""),
        ])
        .await;
        let sandbox = E2bSandbox::reattach_exact(
            server.config(),
            3600,
            "/home/user/exact-repository-root",
            "persisted-id",
            ownership_token,
        )
        .await
        .expect("reattach exact E2B sandbox");
        assert_eq!(sandbox.sandbox_id(), "persisted-id");
        assert_eq!(sandbox.data_plane_domain(), "sandbox.test");
        assert_eq!(
            sandbox.root(),
            Path::new("/home/user/exact-repository-root")
        );
        let discovery_request = server.next_request().await;
        assert!(discovery_request.starts_with("GET /v2/sandboxes?"));
        let ownership_request = server.next_request().await;
        assert!(ownership_request.starts_with("GET /sandboxes/persisted-id HTTP/1.1"));
        let connect_request = server.next_request().await;
        assert!(connect_request.starts_with("POST /sandboxes/persisted-id/connect HTTP/1.1"));
        assert!(connect_request.contains("\"timeout\":3600"));
        drop(sandbox);
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn exact_reattach_classifies_404_separately_from_other_control_plane_failures() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut missing_server = FakeControlPlane::start(vec![
            http_response("200 OK", "[]"),
            http_response("404 Not Found", "gone"),
        ])
        .await;
        let missing = match E2bSandbox::reattach_exact(
            missing_server.config(),
            3600,
            "/home/user/repository",
            "missing-id",
            ownership_token,
        )
        .await
        {
            Ok(_) => panic!("missing persisted sandbox must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            missing,
            E2bRuntimeError::NotFound { sandbox_id } if sandbox_id == "missing-id"
        ));
        let _ = missing_server.next_request().await;
        let _ = missing_server.next_request().await;

        let mut failed_server = FakeControlPlane::start(vec![
            http_response("200 OK", "[]"),
            http_response("503 Service Unavailable", "temporarily unavailable"),
        ])
        .await;
        let failure = match E2bSandbox::reattach_exact(
            failed_server.config(),
            3600,
            "/home/user/repository",
            "still-present-id",
            ownership_token,
        )
        .await
        {
            Ok(_) => panic!("control-plane failure must not look like absence"),
            Err(error) => error,
        };
        assert!(matches!(failure, E2bRuntimeError::ControlPlane(_)));
        let _ = failed_server.next_request().await;
        let _ = failed_server.next_request().await;
    }

    #[tokio::test]
    async fn unsafe_persisted_sandbox_ids_issue_no_provider_request() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut server =
            FakeControlPlane::start(vec![http_response("500 Internal Error", "")]).await;
        for sandbox_id in [
            "../admin",
            ".",
            "..",
            "id/child",
            "id%2fchild",
            "id?query",
            "id#fragment",
            "id\nheader",
        ] {
            let error = E2bSandbox::delete_persisted(server.config(), sandbox_id, ownership_token)
                .await
                .expect_err("unsafe ids must fail locally");
            assert!(error.to_string().contains("safe URL-path component"));
        }
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn exact_runtime_404_does_not_hide_another_token_owned_sandbox() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut server = FakeControlPlane::start(vec![
            listed_session_response("actual-owned-id", ownership_token),
            http_response("500 Internal Error", "unexpected request"),
        ])
        .await;
        let error =
            E2bSandbox::delete_persisted(server.config(), "forged-missing-id", ownership_token)
                .await
                .expect_err("a different token-owned id must block exact cleanup");
        assert!(error.to_string().contains("actual-owned-id"));
        let discovery = server.next_request().await;
        assert!(discovery.starts_with("GET /v2/sandboxes?"));
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn ambiguous_creation_candidate_requires_exact_metadata_before_delete() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut server = FakeControlPlane::start(vec![
            owned_sandbox_response(
                "candidate-id",
                "ses-other:1:00000000-0000-4000-8000-000000000002",
                "running",
            ),
            http_response("500 Internal Error", "unexpected request"),
        ])
        .await;
        let error = E2bSandbox::delete_persisted_creation_candidate(
            server.config(),
            "candidate-id",
            ownership_token,
        )
        .await
        .expect_err("foreign metadata must block deletion");
        assert!(error.to_string().contains("exact persisted"));
        let proof = server.next_request().await;
        assert!(proof.starts_with("GET /sandboxes/candidate-id HTTP/1.1"));
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn connect_uses_configured_domain_when_provider_omits_it() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut server = FakeControlPlane::start(vec![
            listed_session_response("persisted-id", ownership_token),
            owned_sandbox_response("persisted-id", ownership_token, "paused"),
            http_response(
                "200 OK",
                &json!({
                    "sandboxID": "persisted-id",
                    "envdAccessToken": "secret-access-token"
                })
                .to_string(),
            ),
            http_response("500 Internal Error", "unexpected request"),
        ])
        .await;
        let sandbox = E2bSandbox::reattach_exact(
            server.config(),
            3600,
            "/home/user/repository",
            "persisted-id",
            ownership_token,
        )
        .await
        .expect("configured provider domain is the fallback authority");
        assert_eq!(sandbox.data_plane_domain(), "sandbox.test");
        let _ = server.next_request().await;
        let _ = server.next_request().await;
        let _ = server.next_request().await;
        sandbox.preserve_on_drop();
        drop(sandbox);
        server.assert_no_request().await;
    }

    #[test]
    fn data_plane_domain_is_strictly_dns_hostname_only() {
        for valid in ["e2b.app", "sandbox.test", "localhost", "a-b.example"] {
            assert_eq!(validate_data_plane_domain(valid).unwrap(), valid);
        }
        for invalid in [
            "",
            "https://e2b.app",
            "user@e2b.app",
            "e2b.app:443",
            "e2b.app/path",
            "e2b.app?token=x",
            "e2b.app#fragment",
            " e2b.app",
            "e2b.app\n",
            "-bad.example",
            "bad-.example",
        ] {
            assert!(validate_data_plane_domain(invalid).is_err(), "{invalid:?}");
        }
    }

    #[tokio::test]
    async fn token_owned_stop_never_falls_back_to_raw_delete_on_metadata_mismatch() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let foreign_token = "ses-other:1:00000000-0000-4000-8000-000000000002";
        let mut server = FakeControlPlane::start(vec![
            listed_session_response("persisted-id", ownership_token),
            owned_sandbox_response("persisted-id", ownership_token, "paused"),
            session_response("persisted-id"),
            owned_sandbox_response("persisted-id", foreign_token, "running"),
            owned_sandbox_response("persisted-id", foreign_token, "running"),
            http_response("500 Internal Error", "unexpected request"),
        ])
        .await;
        let sandbox = E2bSandbox::reattach_exact(
            server.config(),
            3600,
            "/home/user/repository",
            "persisted-id",
            ownership_token,
        )
        .await
        .unwrap();
        let _ = server.next_request().await;
        let _ = server.next_request().await;
        let _ = server.next_request().await;
        let error = Sandbox::stop_checked(&sandbox)
            .await
            .expect_err("ownership mismatch must remain actionable");
        assert!(error.to_string().contains("exact persisted"));
        let first_proof = server.next_request().await;
        let retry_proof = server.next_request().await;
        assert!(first_proof.starts_with("GET /sandboxes/persisted-id HTTP/1.1"));
        assert!(retry_proof.starts_with("GET /sandboxes/persisted-id HTTP/1.1"));
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn checked_pause_is_idempotent_and_drop_never_deletes() {
        let ownership_token = "ses-owner:1:00000000-0000-4000-8000-000000000001";
        let mut server = FakeControlPlane::start(vec![
            listed_session_response("pause-me", ownership_token),
            owned_sandbox_response("pause-me", ownership_token, "paused"),
            session_response("pause-me"),
            listed_session_response("pause-me", ownership_token),
            owned_sandbox_response("pause-me", ownership_token, "paused"),
            http_response("204 No Content", ""),
            listed_session_response("pause-me", ownership_token),
            owned_sandbox_response("pause-me", ownership_token, "paused"),
            http_response("409 Conflict", "already paused"),
            http_response(
                "200 OK",
                &json!({ "sandboxID": "pause-me", "state": "paused" }).to_string(),
            ),
            http_response("500 Internal Error", "unexpected request"),
        ])
        .await;
        let sandbox = E2bSandbox::reattach_exact(
            server.config(),
            3600,
            "/home/user/repository",
            "pause-me",
            ownership_token,
        )
        .await
        .expect("reattach sandbox before pause");
        let _ = server.next_request().await;
        let _ = server.next_request().await;
        let _ = server.next_request().await;
        sandbox.pause_checked().await.expect("pause exact sandbox");
        let pause_discovery = server.next_request().await;
        assert!(pause_discovery.starts_with("GET /v2/sandboxes?"));
        let pause_proof = server.next_request().await;
        assert!(pause_proof.starts_with("GET /sandboxes/pause-me HTTP/1.1"));
        let pause_request = server.next_request().await;
        assert!(pause_request.starts_with("POST /sandboxes/pause-me/pause HTTP/1.1"));
        assert!(
            pause_request.contains("\"memory\":true"),
            "the E2B pause endpoint must take a full-memory snapshot"
        );
        sandbox
            .pause_checked()
            .await
            .expect("a retried pause reconciles exact paused state");
        let retry_discovery = server.next_request().await;
        assert!(retry_discovery.starts_with("GET /v2/sandboxes?"));
        let retry_proof = server.next_request().await;
        assert!(retry_proof.starts_with("GET /sandboxes/pause-me HTTP/1.1"));
        let retried_pause = server.next_request().await;
        assert!(retried_pause.starts_with("POST /sandboxes/pause-me/pause HTTP/1.1"));
        let state_lookup = server.next_request().await;
        assert!(state_lookup.starts_with("GET /sandboxes/pause-me HTTP/1.1"));
        drop(sandbox);
        server.assert_no_request().await;
    }

    #[tokio::test]
    async fn caller_cancellation_does_not_cancel_owned_remote_start() {
        #[derive(Debug)]
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let output_dropped = Arc::new(AtomicBool::new(false));
        let owned_output = output_dropped.clone();
        let caller = tokio::spawn(async move {
            await_owned_start(async move {
                let _ = entered.send(());
                let _ = release_rx.await;
                Ok(DropProbe(owned_output))
            })
            .await
        });

        entered_rx.await.expect("owned start entered");
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release.send(()).expect("release detached start");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !output_dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached start output should be dropped");
    }

    /// Live end-to-end test against real E2B. Ignored by default; run with:
    /// `E2B_API_KEY=... cargo test -p axocoatl-isolation -- --ignored live_`
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_exec_and_stdin_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let client = E2bClient::new(E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        });
        let session = client
            .create(120, &BTreeMap::new())
            .await
            .expect("create sandbox");

        let run = async {
            // exec: non-zero exit, stdout + stderr split.
            let r = client
                .exec(
                    &session,
                    &["sh", "-c", "printf hi; printf oops 1>&2; exit 3"],
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!(
                (r.stdout.as_str(), r.stderr.as_str(), r.exit_code),
                ("hi", "oops", 3)
            );

            // exec: success (exit 0, the omitted-exitCode case).
            let ok = client
                .exec(
                    &session,
                    &["sh", "-c", "echo done"],
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!((ok.stdout.trim(), ok.exit_code), ("done", 0));

            // exec_stdin: write a file via `cat > file`, then read it back.
            client
                .exec_stdin(
                    &session,
                    &["sh", "-c", "cat > \"$1\"", "sh", "/tmp/axo.txt"],
                    "written-via-stdin",
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            let back = client
                .exec(
                    &session,
                    &["cat", "/tmp/axo.txt"],
                    "/",
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!(back.stdout, "written-via-stdin");
            Ok::<_, IsolationError>(())
        }
        .await;

        client.kill(&session.sandbox_id).await.ok();
        run.expect("live E2B exec/stdin flow");
    }

    /// Drive `E2bSandbox` purely through the `Sandbox` trait object — the same
    /// path the session tools take — against real E2B.
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_sandbox_trait_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let cfg = E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        };
        let sandbox = E2bSandbox::start(cfg, 120, "/tmp", &BTreeMap::new())
            .await
            .expect("start sandbox");
        let sb: &dyn Sandbox = &sandbox;

        let run = async {
            // cwd is honored (the file tools + bash rely on `root` as the cwd).
            let pwd = sb.exec(&["pwd"], Duration::from_secs(30)).await?;
            assert_eq!(pwd.stdout.trim(), "/tmp");

            // write via exec_stdin (relative path, resolved against root), read back.
            sb.exec_stdin(
                &["sh", "-c", "cat > \"$1\"", "sh", "note.txt"],
                "trait-write",
                Duration::from_secs(30),
            )
            .await?;
            let back = sb
                .exec(&["cat", "note.txt"], Duration::from_secs(30))
                .await?;
            assert_eq!(back.stdout, "trait-write");

            // background task captures its output into the tracked log.
            let id = sb.spawn_background("echo bg-line");
            assert!(id.starts_with("task-"));
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let tasks = sb.list_tasks();
            assert_eq!(tasks.len(), 1);
            assert!(
                tasks[0].log.contains("bg-line"),
                "bg log should capture output, got: {:?}",
                tasks[0].log
            );

            // interactive terminals now work on this backend (see the dedicated
            // live_pty_terminal_against_e2b test for the full input/output flow).
            let term = sb.spawn_pty("sh", 24, 80).expect("spawn_pty");
            assert_eq!(sb.list_terminals().len(), 1);
            assert!(sb.kill_terminal(&term.id));
            Ok::<_, IsolationError>(())
        }
        .await;

        sb.stop().await;
        run.expect("live E2bSandbox trait flow");
    }

    /// End-to-end git-native flow against real E2B, running the same commands the
    /// daemon's `start_e2b_sandbox` uses: inject a token as a sandbox env var,
    /// configure the in-VM credential helper, prove it yields Basic creds from the
    /// env (the private clone/push auth path — without needing a real token),
    /// clone a public repo, re-root at it, and `git worktree add` inside (the
    /// variant-lane mechanism).
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_git_native_flow_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let cfg = E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        };
        // Exactly the daemon's create-time env: no prompts + a (fake) git token.
        let mut env = BTreeMap::new();
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
        env.insert("AXO_GIT_TOKEN".to_string(), "fake-token-xyz789".to_string());
        let base = E2bSandbox::start(cfg, 180, "/home/user", &env)
            .await
            .expect("start sandbox");

        let run = async {
            // (1) Configure git — the daemon's exact setup command.
            let setup = "git config --global 'credential.https://github.com.helper' \
                 '!f() { echo username=x-access-token; echo password=$AXO_GIT_TOKEN; }; f' && \
                 git config --global 'credential.https://github.com.useHttpPath' false && \
                 git config --global user.email 'agent@axocoatl.local' && \
                 git config --global user.name 'Axocoatl Agent'";
            let r = base
                .exec(&["sh", "-c", setup], Duration::from_secs(30))
                .await?;
            assert_eq!(r.exit_code, 0, "git config failed: {}", r.stderr);

            // (2) The helper reads the injected token and produces Basic creds.
            let fill = base
                .exec(
                    &[
                        "sh",
                        "-c",
                        "printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill",
                    ],
                    Duration::from_secs(30),
                )
                .await?;
            assert!(
                fill.stdout.contains("username=x-access-token"),
                "credential fill username: {}",
                fill.stdout
            );
            assert!(
                fill.stdout.contains("password=fake-token-xyz789"),
                "credential fill password (token from env): {}",
                fill.stdout
            );

            // (3) Public clone end-to-end, then re-root and run git inside it.
            let clone = "git clone --branch master --single-branch \
                 https://github.com/octocat/Hello-World.git /home/user/hello";
            let r = base
                .exec(&["sh", "-c", clone], Duration::from_secs(120))
                .await?;
            assert_eq!(r.exit_code, 0, "clone failed: {}", r.stderr);
            let rooted = base.with_root(Path::new("/home/user/hello"));
            let head = rooted
                .exec(&["git", "rev-parse", "HEAD"], Duration::from_secs(30))
                .await?;
            assert_eq!(
                head.stdout.trim().len(),
                40,
                "unexpected HEAD: {}",
                head.stdout
            );

            // (4) `git worktree add` inside the clone — the variant-lane mechanism.
            let wt = rooted
                .exec(
                    &[
                        "git",
                        "-c",
                        "safe.directory=*",
                        "-C",
                        "/home/user/hello",
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        "axo/variant-0",
                        "/home/user/hello/.axo-variants/0",
                        "HEAD",
                    ],
                    Duration::from_secs(30),
                )
                .await?;
            assert_eq!(wt.exit_code, 0, "worktree add failed: {}", wt.stderr);
            Ok::<_, IsolationError>(())
        }
        .await;

        base.stop().await;
        run.expect("live git-native flow");
    }

    /// Interactive PTY terminal end-to-end against real E2B: open a shell, send a
    /// keystroke command, see it execute, resize, list, and kill.
    #[tokio::test]
    #[ignore = "hits live E2B — requires E2B_API_KEY"]
    async fn live_pty_terminal_against_e2b() {
        let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
        let cfg = E2bConfig {
            api_url: "https://api.e2b.dev".into(),
            api_key,
            template: "base".into(),
            domain: "e2b.app".into(),
        };
        let sandbox = E2bSandbox::start(cfg, 120, "/home/user", &BTreeMap::new())
            .await
            .expect("start sandbox");

        let run = async {
            let term = Sandbox::spawn_pty(&sandbox, "sh", 24, 80).map_err(IsolationError::E2b)?;
            // Let the shell come up, then type a command + Enter.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            term.input_tx
                .send(b"echo PTY_LIVE_OK\n".to_vec())
                .expect("send keystrokes");
            tokio::time::sleep(Duration::from_millis(2500)).await;

            let out = term.snapshot();
            let text = String::from_utf8_lossy(&out);
            assert!(
                text.contains("PTY_LIVE_OK"),
                "shell should echo+run the command; got: {text:?}"
            );
            assert!(term.is_alive(), "shell should still be running");

            // Resize is best-effort (fire-and-forget Update) — must not panic.
            term.resize(40, 100);

            // Tracked in the terminal list, then killed.
            assert_eq!(Sandbox::list_terminals(&sandbox).len(), 1);
            let id = term.id.clone();
            assert!(Sandbox::kill_terminal(&sandbox, &id));
            assert_eq!(Sandbox::list_terminals(&sandbox).len(), 0);
            Ok::<_, IsolationError>(())
        }
        .await;

        sandbox.stop().await;
        run.expect("live PTY terminal flow");
    }
}
