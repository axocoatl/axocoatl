//! A read-only check of what native execution requires of an Ollama server
//! before any model is chosen: a loopback endpoint, a server version the
//! version policy accepts and server-reported cloud-disabled mode. Native admission enforces the same
//! requirements again before inference; setup and `doctor` use this check to
//! report them up front instead of after a Session is created.

use std::time::Duration;

use axocoatl_llm::ProviderError;
use serde_json::Value;

use super::{local_client, metadata_request, ollama_version_refusal, require_loopback};

/// Bounds the whole check, so setup and `doctor` never wait on a hung server
/// for the full inference response timeout.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest reported version or setting source this check keeps. Anything
/// longer is not an Ollama report and is treated as missing.
const MAX_REPORTED_TEXT: usize = 64;

/// Ollama's cloud mode as `GET /api/status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OllamaCloudMode {
    /// Cloud models and web search are disabled. `source` is where the server
    /// read the setting: `env` (`OLLAMA_NO_CLOUD`), `config`
    /// (`disable_ollama_cloud` in the server user's `~/.ollama/server.json`)
    /// or `both`; empty when not reported.
    Disabled { source: String },
    /// Cloud features are enabled. Native execution refuses this server.
    Enabled,
    /// No usable cloud status: a server without `/api/status`, or a malformed
    /// reply. Native execution refuses this server too.
    Unreported,
}

impl OllamaCloudMode {
    /// Read an `/api/status` body the way native admission does: only an
    /// explicit boolean `cloud.disabled: true` counts as disabled.
    pub fn from_status(status: &Value) -> Self {
        match status.pointer("/cloud/disabled").and_then(Value::as_bool) {
            Some(true) => Self::Disabled {
                source: reported_text(status.pointer("/cloud/source")).unwrap_or_default(),
            },
            Some(false) => Self::Enabled,
            None => Self::Unreported,
        }
    }
}

/// Native admission's cloud requirement, shared with [`OllamaCloudMode`] so
/// that setup, `doctor` and admission cannot disagree.
pub(super) fn reports_cloud_disabled(status: &Value) -> bool {
    matches!(
        OllamaCloudMode::from_status(status),
        OllamaCloudMode::Disabled { .. }
    )
}

/// What native execution would find at an Ollama server before a model is
/// chosen. The model itself is checked when its profile is reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeOllamaServerCheck {
    /// The version `GET /api/version` reported, if any.
    pub version: Option<String>,
    /// The cloud mode `GET /api/status` reported.
    pub cloud: OllamaCloudMode,
}

impl NativeOllamaServerCheck {
    /// The server runs an Ollama version native execution accepts.
    pub fn version_supported(&self) -> bool {
        self.version_refusal().is_none()
    }

    /// Why native execution refuses the server's version, or `None` when it
    /// is accepted.
    pub fn version_refusal(&self) -> Option<String> {
        match self.version.as_deref() {
            Some(version) => ollama_version_refusal(version),
            None => Some(format!(
                "Ollama did not report its version; native Sessions need Ollama {} or a later \
                 0.x release",
                super::AUDITED_OLLAMA_VERSION
            )),
        }
    }

    /// The server reports that its cloud features are disabled.
    pub fn cloud_disabled(&self) -> bool {
        matches!(self.cloud, OllamaCloudMode::Disabled { .. })
    }

    /// Both server requirements of native execution hold.
    pub fn ready(&self) -> bool {
        self.version_supported() && self.cloud_disabled()
    }
}

/// Refuse an endpoint native execution refuses: not HTTP(S), carrying
/// userinfo, a query or a fragment, or not a loopback address.
pub fn validate_native_ollama_endpoint(base_url: &str) -> Result<(), ProviderError> {
    require_loopback(base_url)
}

/// Read the server's version and cloud mode with `GET /api/version` and
/// `GET /api/status`, through the same loopback-only client native execution
/// uses. Returns an error when the endpoint is refused or the server cannot be
/// reached. A server that answers without a usable version or cloud status is
/// reported as such rather than as an error.
pub async fn check_native_ollama_server(
    base_url: &str,
) -> Result<NativeOllamaServerCheck, ProviderError> {
    let client = local_client(base_url)?;
    // A server that answers, but refuses the request or answers something
    // unreadable, did not report the value: only a server that cannot be
    // reached is an error.
    let unreported = |error: &ProviderError| {
        matches!(
            error,
            ProviderError::InvalidRequest { .. }
                | ProviderError::ApiError { .. }
                | ProviderError::ModelNotFound { .. }
        )
    };
    let check = async {
        let version = match metadata_request(&client, base_url, "api/version", None).await {
            Ok(value) => reported_text(value.get("version")),
            Err(error) if unreported(&error) => None,
            Err(error) => return Err(error),
        };
        let cloud = match metadata_request(&client, base_url, "api/status", None).await {
            Ok(value) => OllamaCloudMode::from_status(&value),
            Err(error) if unreported(&error) => OllamaCloudMode::Unreported,
            Err(error) => return Err(error),
        };
        Ok(NativeOllamaServerCheck { version, cloud })
    };
    tokio::time::timeout(CHECK_TIMEOUT, check)
        .await
        .map_err(|_| {
            ProviderError::Network(format!(
                "the Ollama server did not answer within {} seconds",
                CHECK_TIMEOUT.as_secs()
            ))
        })?
}

/// `POST /api/show` for `model`, through the same loopback-only client native
/// execution uses, within the check's time bound. [`crate::check_shown_model`]
/// reads the answer the way native admission does.
pub async fn show_native_ollama_model(base_url: &str, model: &str) -> Result<Value, ProviderError> {
    let client = local_client(base_url)?;
    tokio::time::timeout(
        CHECK_TIMEOUT,
        metadata_request(
            &client,
            base_url,
            "api/show",
            Some(serde_json::json!({"model": model})),
        ),
    )
    .await
    .map_err(|_| {
        ProviderError::Network(format!(
            "the Ollama server did not answer within {} seconds",
            CHECK_TIMEOUT.as_secs()
        ))
    })?
}

fn reported_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| text.len() <= MAX_REPORTED_TEXT && !text.chars().any(char::is_control))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::AUDITED_OLLAMA_VERSION;
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    async fn server(version: Option<Value>, status: Option<Value>) -> MockServer {
        let server = MockServer::start().await;
        for (route, body) in [("/api/version", version), ("/api/status", status)] {
            let response = match body {
                Some(body) => ResponseTemplate::new(200).set_body_json(body),
                None => ResponseTemplate::new(404).set_body_string("404 page not found"),
            };
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(response)
                .mount(&server)
                .await;
        }
        server
    }

    #[tokio::test]
    async fn a_cloud_disabled_audited_server_is_ready() {
        let server = server(
            Some(json!({"version": AUDITED_OLLAMA_VERSION})),
            Some(json!({"cloud": {"disabled": true, "source": "env"}})),
        )
        .await;
        let check = check_native_ollama_server(&server.uri()).await.unwrap();
        assert_eq!(check.version.as_deref(), Some(AUDITED_OLLAMA_VERSION));
        assert_eq!(check.version_refusal(), None);
        assert_eq!(
            check.cloud,
            OllamaCloudMode::Disabled {
                source: "env".into()
            }
        );
        assert!(check.ready());
    }

    #[tokio::test]
    async fn a_server_with_cloud_enabled_is_not_ready() {
        // The exact reply of an Ollama server (0.20.6 and 0.40.6 alike)
        // started without OLLAMA_NO_CLOUD and without server.json.
        let server = server(
            Some(json!({"version": AUDITED_OLLAMA_VERSION})),
            Some(json!({"cloud": {"disabled": false, "source": "none"}})),
        )
        .await;
        let check = check_native_ollama_server(&server.uri()).await.unwrap();
        assert!(check.version_supported());
        assert_eq!(check.cloud, OllamaCloudMode::Enabled);
        assert!(!check.cloud_disabled());
        assert!(!check.ready());
    }

    #[tokio::test]
    async fn an_older_server_without_status_reports_neither_requirement() {
        let server = server(Some(json!({"version": "0.12.3"})), None).await;
        let check = check_native_ollama_server(&server.uri()).await.unwrap();
        assert_eq!(check.version.as_deref(), Some("0.12.3"));
        assert!(!check.version_supported());
        assert!(check.version_refusal().unwrap().contains("upgrade Ollama"));
        assert_eq!(check.cloud, OllamaCloudMode::Unreported);
        assert!(!check.ready());
    }

    #[tokio::test]
    async fn the_previously_audited_release_is_refused_with_its_own_reason() {
        let server = server(
            Some(json!({"version": "0.20.6"})),
            Some(json!({"cloud": {"disabled": true, "source": "config"}})),
        )
        .await;
        let check = check_native_ollama_server(&server.uri()).await.unwrap();
        assert!(check.cloud_disabled());
        assert!(!check.ready());
        let refusal = check.version_refusal().unwrap();
        assert!(refusal.contains("no longer supported"), "{refusal}");
    }

    #[tokio::test]
    async fn a_later_patch_of_the_audited_line_is_ready() {
        let server = server(
            Some(json!({"version": "0.40.7"})),
            Some(json!({"cloud": {"disabled": true, "source": "env"}})),
        )
        .await;
        assert!(check_native_ollama_server(&server.uri())
            .await
            .unwrap()
            .ready());
    }

    #[tokio::test]
    async fn malformed_reports_are_missing_not_errors() {
        let server = server(
            Some(json!({"version": "x".repeat(MAX_REPORTED_TEXT + 1)})),
            Some(json!({"cloud": {"disabled": "true"}})),
        )
        .await;
        let check = check_native_ollama_server(&server.uri()).await.unwrap();
        assert_eq!(check.version, None);
        assert_eq!(check.cloud, OllamaCloudMode::Unreported);
    }

    #[tokio::test]
    async fn non_loopback_and_credential_endpoints_are_refused_without_a_request() {
        for endpoint in [
            "http://192.0.2.10:11434",
            "http://ollama.example:11434",
            "http://user:secret@localhost:11434",
            "http://localhost:11434/?key=value",
            "ftp://localhost:11434",
        ] {
            assert!(
                validate_native_ollama_endpoint(endpoint).is_err(),
                "{endpoint}"
            );
            assert!(
                matches!(
                    check_native_ollama_server(endpoint).await,
                    Err(ProviderError::InvalidRequest { .. })
                ),
                "{endpoint}"
            );
        }
        for endpoint in [
            "http://localhost:11434",
            "http://127.0.0.1:11436/",
            "http://[::1]:11434",
        ] {
            assert!(
                validate_native_ollama_endpoint(endpoint).is_ok(),
                "{endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_server_is_an_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let result = check_native_ollama_server(&format!("http://127.0.0.1:{port}")).await;
        assert!(
            matches!(result, Err(ProviderError::Network(_))),
            "{result:?}"
        );
    }

    #[test]
    fn admission_and_the_check_read_status_identically() {
        for (status, disabled) in [
            (
                json!({"cloud": {"disabled": true, "source": "config"}}),
                true,
            ),
            (json!({"cloud": {"disabled": true}}), true),
            (
                json!({"cloud": {"disabled": false, "source": "none"}}),
                false,
            ),
            (json!({"cloud": {"disabled": 1}}), false),
            (json!({"cloud": {}}), false),
            (json!({}), false),
        ] {
            assert_eq!(reports_cloud_disabled(&status), disabled, "{status}");
            assert_eq!(
                matches!(
                    OllamaCloudMode::from_status(&status),
                    OllamaCloudMode::Disabled { .. }
                ),
                disabled
            );
        }
        assert_eq!(
            OllamaCloudMode::from_status(&json!({"cloud": {"disabled": true}})),
            OllamaCloudMode::Disabled {
                source: String::new()
            }
        );
    }
}
