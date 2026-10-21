//! What a new user's first Session needs, checked the way `axocoatl doctor`
//! checks it, for the workbench's first screen (`GET /api/readiness`).
//!
//! The report has doctor's shape: `{"ready", "sections": {"sandbox",
//! "models", "writers", "daemon"}}`, each item `{"name", "status", "detail",
//! "fix"}` with status `ok`, `warn` or `fail`. It is ready when nothing fails.
//! A stopped Podman machine is fine: a Session's start starts it. The checks
//! are the shared ones (`axocoatl_isolation::podman::detect`,
//! `axocoatl_llm_ollama::check_native_ollama_server`); nothing here changes
//! the machine.

use std::path::PathBuf;
use std::time::Duration;

use axocoatl_config::AxocoatlConfig;
use axocoatl_isolation::podman::{InstallRoute, PodmanReadiness};
use axocoatl_llm_ollama::{NativeOllamaServerCheck, OllamaCloudMode};
use serde::Serialize;

/// This Axocoatl's version, as `/health` and the readiness report name it.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the model list may take before Ollama counts as unreachable.
const TAGS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatus {
    Ok,
    Warn,
    Fail,
}

/// One check: what it is, how it went, and the exact command or step that
/// fixes it when it did not pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessItem {
    /// A stable identifier for the check, such as `podman` or
    /// `ollama.model:qwen3-coder:30b`.
    pub id: String,
    pub name: String,
    pub status: ReadinessStatus,
    pub detail: String,
    pub fix: Option<String>,
}

impl ReadinessItem {
    fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        status: ReadinessStatus,
        detail: impl Into<String>,
        fix: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status,
            detail: detail.into(),
            fix,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReadinessSections {
    pub sandbox: Vec<ReadinessItem>,
    pub models: Vec<ReadinessItem>,
    pub writers: Vec<ReadinessItem>,
    pub daemon: Vec<ReadinessItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessReport {
    /// Nothing failed.
    pub ready: bool,
    pub sections: ReadinessSections,
    /// Why no Session can be created yet (Podman is not installed or has no
    /// machine), when that is so. A stopped machine does not block.
    pub sessions_blocked: Option<String>,
    pub version: &'static str,
}

impl ReadinessReport {
    fn from_sections(sections: ReadinessSections, sessions_blocked: Option<String>) -> Self {
        let ready = [
            &sections.sandbox,
            &sections.models,
            &sections.writers,
            &sections.daemon,
        ]
        .into_iter()
        .flatten()
        .all(|item| item.status != ReadinessStatus::Fail);
        Self {
            ready,
            sections,
            sessions_blocked,
            version: DAEMON_VERSION,
        }
    }
}

/// What the checks need from a running daemon, copied so they run without
/// holding it.
#[derive(Debug, Clone)]
pub struct ReadinessInputs {
    pub config: AxocoatlConfig,
    pub data_dir: PathBuf,
    pub agents_running: usize,
}

/// The Podman item, and why Sessions are blocked when they are. `route` is
/// how Podman gets onto this computer ([`InstallRoute::detect`]): a missing
/// Podman's fix is `axocoatl doctor`'s, from the same
/// [`axocoatl_isolation::podman::install_fix`], so it says onboarding
/// installs Podman only where it does.
pub fn podman_item(
    readiness: &PodmanReadiness,
    route: &InstallRoute,
) -> (ReadinessItem, Option<String>) {
    match readiness {
        PodmanReadiness::Ready => (
            ReadinessItem::new(
                "podman",
                "Podman",
                ReadinessStatus::Ok,
                "Podman is ready; Sessions run in its rootless sandbox.",
                None,
            ),
            None,
        ),
        PodmanReadiness::MachineStopped => (
            ReadinessItem::new(
                "podman",
                "Podman",
                ReadinessStatus::Ok,
                "The Podman machine is stopped; the first Session starts it.",
                None,
            ),
            None,
        ),
        PodmanReadiness::NotInstalled => {
            let detail = "Podman is not installed. Every Session runs in a Podman sandbox.";
            (
                ReadinessItem::new(
                    "podman",
                    "Podman",
                    ReadinessStatus::Fail,
                    detail,
                    Some(format!(
                        "{}.",
                        axocoatl_isolation::podman::install_fix(route)
                    )),
                ),
                Some(detail.to_string()),
            )
        }
        PodmanReadiness::MachineMissing => {
            let detail =
                "Podman is installed but has no machine yet. Every Session runs in a Podman sandbox.";
            (
                ReadinessItem::new(
                    "podman",
                    "Podman",
                    ReadinessStatus::Fail,
                    detail,
                    Some(format!(
                        "Run `axocoatl onboard` (it creates the machine after asking), or: `{}`.",
                        axocoatl_isolation::podman::machine_create_hint()
                    )),
                ),
                Some(detail.to_string()),
            )
        }
        PodmanReadiness::Unavailable(error) => (
            ReadinessItem::new(
                "podman",
                "Podman",
                ReadinessStatus::Fail,
                format!("Podman did not answer its readiness check: {error}"),
                Some("Run `axocoatl doctor` to see what Podman reports.".to_string()),
            ),
            None,
        ),
    }
}

/// The Ollama server items: the server itself, its version and its cloud
/// mode, as native Sessions require them.
pub fn ollama_server_items(
    base_url: &str,
    check: &Result<NativeOllamaServerCheck, String>,
) -> Vec<ReadinessItem> {
    if let Err(error) = axocoatl_llm_ollama::validate_native_ollama_endpoint(base_url) {
        return vec![ReadinessItem::new(
            "ollama.server",
            "Ollama",
            ReadinessStatus::Fail,
            format!("Native Sessions cannot use Ollama at {base_url}: {error}."),
            Some(
                "Use an Ollama server on this computer, such as http://localhost:11434, in \
                 providers.ollama.base_url, then restart Axocoatl."
                    .to_string(),
            ),
        )];
    }
    let check = match check {
        Ok(check) => check,
        Err(error) => {
            return vec![ReadinessItem::new(
                "ollama.server",
                "Ollama",
                ReadinessStatus::Fail,
                format!("Ollama is not reachable at {base_url}: {error}"),
                Some(
                    "Start Ollama: `brew services start ollama` with Homebrew, `ollama serve` \
                     otherwise."
                        .to_string(),
                ),
            )]
        }
    };
    let mut items = Vec::new();
    items.push(match check.version_refusal() {
        None => ReadinessItem::new(
            "ollama.version",
            "Ollama version",
            ReadinessStatus::Ok,
            format!(
                "Ollama {} at {base_url}",
                check.version.as_deref().unwrap_or_default()
            ),
            None,
        ),
        Some(refusal) => ReadinessItem::new(
            "ollama.version",
            "Ollama version",
            ReadinessStatus::Fail,
            format!(
                "Ollama at {base_url} reports version {}; native Sessions refuse it: {refusal}.",
                check.version.as_deref().unwrap_or("(none)")
            ),
            Some(format!(
                "Install Ollama {} or a later release this Axocoatl accepts, restart Ollama, \
                 then check again with `axocoatl doctor`.",
                axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION
            )),
        ),
    });
    let disable = format!(
        "Add `{{\"disable_ollama_cloud\": true}}` to `~/.ollama/server.json` of the user that \
         runs Ollama, or start it with `OLLAMA_NO_CLOUD=1`, and restart Ollama; then `curl -s \
         {}/api/status` shows \"disabled\":true.",
        base_url.trim_end_matches('/')
    );
    items.push(match &check.cloud {
        OllamaCloudMode::Disabled { .. } => ReadinessItem::new(
            "ollama.cloud",
            "Ollama cloud models",
            ReadinessStatus::Ok,
            "Off, as native Sessions require.",
            None,
        ),
        OllamaCloudMode::Enabled => ReadinessItem::new(
            "ollama.cloud",
            "Ollama cloud models",
            ReadinessStatus::Fail,
            format!("Ollama at {base_url} has its cloud models on; native Sessions refuse it."),
            Some(disable),
        ),
        OllamaCloudMode::Unreported => ReadinessItem::new(
            "ollama.cloud",
            "Ollama cloud models",
            ReadinessStatus::Fail,
            format!(
                "Ollama at {base_url} does not report whether its cloud models are off; native \
                 Sessions refuse it."
            ),
            Some(disable),
        ),
    });
    items
}

/// One item per configured Ollama model: pulled or not.
pub fn ollama_model_items(wanted: &[String], pulled: &[String]) -> Vec<ReadinessItem> {
    wanted
        .iter()
        .map(|model| {
            let have = pulled
                .iter()
                .any(|name| name == model || name.starts_with(&format!("{model}:")));
            if have {
                ReadinessItem::new(
                    format!("ollama.model:{model}"),
                    format!("Model {model}"),
                    ReadinessStatus::Ok,
                    "Pulled.",
                    None,
                )
            } else {
                ReadinessItem::new(
                    format!("ollama.model:{model}"),
                    format!("Model {model}"),
                    ReadinessStatus::Fail,
                    format!("{model} is configured but not pulled."),
                    Some(format!("Run `ollama pull {model}`.")),
                )
            }
        })
        .collect()
}

async fn ollama_tags(base_url: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(TAGS_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(format!("{}/api/tags", base_url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
    Ok(body
        .get("models")
        .and_then(serde_json::Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("name").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

/// The models native Agents of `config` run on Ollama, sorted and unique.
fn ollama_models(config: &AxocoatlConfig) -> Vec<String> {
    let mut models: Vec<String> = config
        .agents
        .iter()
        .filter(|agent| agent.provider == "ollama")
        .filter_map(|agent| {
            if agent.model.trim().is_empty() {
                config.providers.ollama.as_ref()?.model.clone()
            } else {
                Some(agent.model.clone())
            }
        })
        .collect();
    models.sort();
    models.dedup();
    models
}

/// The external writers' sign-ins, by stored credential name only.
pub fn writer_items(stored: &[String]) -> Vec<ReadinessItem> {
    let has = |name: &str| stored.iter().any(|stored| stored == name);
    let mut items = Vec::new();
    for (id, name, secrets, connect) in [
        (
            "writer.claude-code",
            "Claude Code",
            &[crate::external_agent::claude_code::CLAUDE_CODE_SECRET][..],
            "axocoatl connect claude-code",
        ),
        (
            "writer.codex",
            "Codex",
            &[
                crate::external_agent::codex::CODEX_CHATGPT_SECRET,
                crate::external_agent::codex::CODEX_SECRET,
            ][..],
            "axocoatl connect codex",
        ),
    ] {
        let connected = secrets.iter().any(|secret| has(secret));
        items.push(ReadinessItem::new(
            id,
            name,
            ReadinessStatus::Ok,
            if connected {
                format!("Connected; `axocoatl run fix-{}` can use it.", id.trim_start_matches("writer."))
            } else {
                format!("Not connected. Optional: native Agents write without it; `{connect}` signs in.")
            },
            None,
        ));
    }
    items
}

/// Run every check. Nothing here changes the machine.
pub async fn check_readiness(inputs: ReadinessInputs) -> ReadinessReport {
    let mut sections = ReadinessSections::default();
    let config = &inputs.config;

    let mut sessions_blocked = None;
    if config.sandbox.backend == "e2b" {
        sections.sandbox.push(ReadinessItem::new(
            "sandbox",
            "Sandbox",
            ReadinessStatus::Ok,
            "Sessions run in the configured E2B template.",
            None,
        ));
    } else {
        let (item, blocked) = podman_item(
            &axocoatl_isolation::podman::detect().await,
            &InstallRoute::detect(),
        );
        sections.sandbox.push(item);
        sessions_blocked = blocked;
    }

    if let Some(ollama) = &config.providers.ollama {
        let check = axocoatl_llm_ollama::check_native_ollama_server(&ollama.base_url)
            .await
            .map_err(|error| error.to_string());
        let reachable = check.is_ok();
        sections
            .models
            .extend(ollama_server_items(&ollama.base_url, &check));
        let wanted = ollama_models(config);
        if reachable && !wanted.is_empty() {
            match ollama_tags(&ollama.base_url).await {
                Ok(pulled) => sections.models.extend(ollama_model_items(&wanted, &pulled)),
                Err(error) => sections.models.push(ReadinessItem::new(
                    "ollama.models",
                    "Ollama models",
                    ReadinessStatus::Warn,
                    format!("Could not list Ollama's models: {error}"),
                    None,
                )),
            }
        }
    }
    if let Some(openrouter) = &config.providers.openrouter {
        let key = openrouter.api_key.expose_secret();
        let set = !key.is_empty() && !key.starts_with("${") && !key.contains("your-key");
        sections.models.push(ReadinessItem::new(
            "openrouter.key",
            "OpenRouter",
            if set {
                ReadinessStatus::Ok
            } else {
                ReadinessStatus::Fail
            },
            if set {
                "An API key is configured."
            } else {
                "No OpenRouter API key is configured."
            },
            (!set).then(|| "Run `axocoatl onboard` to add one.".to_string()),
        ));
    }
    if sections.models.is_empty() {
        sections.models.push(ReadinessItem::new(
            "models",
            "Models",
            ReadinessStatus::Warn,
            "No Ollama or OpenRouter provider is configured for native Agents.",
            Some("Run `axocoatl onboard`.".to_string()),
        ));
    }

    let stored = crate::secret_store::list_secrets(&inputs.data_dir).unwrap_or_default();
    sections.writers = writer_items(&stored);

    sections.daemon.push(ReadinessItem::new(
        "daemon",
        "Axocoatl daemon",
        ReadinessStatus::Ok,
        format!(
            "Axocoatl {DAEMON_VERSION} is running with {} {}.",
            inputs.agents_running,
            if inputs.agents_running == 1 {
                "Agent"
            } else {
                "Agents"
            }
        ),
        None,
    ));
    ReadinessReport::from_sections(sections, sessions_blocked)
}

/// Log, when the daemon starts, an Ollama server native Sessions will
/// refuse: its version or its cloud mode. Without this the daemon starts
/// silently and the refusal first shows at Team and budget.
pub async fn warn_unready_ollama(base_url: String) -> Vec<String> {
    let check = axocoatl_llm_ollama::check_native_ollama_server(&base_url)
        .await
        .map_err(|error| error.to_string());
    let refused: Vec<String> = ollama_server_items(&base_url, &check)
        .into_iter()
        .filter(|item| item.status == ReadinessStatus::Fail)
        .map(|item| match item.fix {
            Some(fix) => format!("{} {fix}", item.detail),
            None => item.detail,
        })
        .collect();
    for line in &refused {
        tracing::warn!(base_url = %base_url, "{line}");
    }
    refused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(version: &str, cloud: OllamaCloudMode) -> Result<NativeOllamaServerCheck, String> {
        Ok(NativeOllamaServerCheck {
            version: Some(version.into()),
            cloud,
        })
    }

    const UBUNTU: &str = "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n";

    #[test]
    fn a_missing_podman_or_machine_fails_and_blocks_sessions_a_stopped_one_does_not() {
        let here = InstallRoute::detect();
        let (item, blocked) = podman_item(&PodmanReadiness::MachineMissing, &here);
        assert_eq!(item.status, ReadinessStatus::Fail);
        // The machine Axocoatl creates, sized, never Podman's 2 GiB default.
        assert_eq!(
            item.fix.as_deref(),
            Some(
                format!(
                    "Run `axocoatl onboard` (it creates the machine after asking), or: `{}`.",
                    axocoatl_isolation::podman::machine_create_hint()
                )
                .as_str()
            )
        );
        assert!(item.fix.as_deref().unwrap().contains(" --memory "));
        assert_eq!(blocked.as_deref(), Some(item.detail.as_str()));
        let (item, blocked) = podman_item(&PodmanReadiness::NotInstalled, &here);
        assert_eq!(item.status, ReadinessStatus::Fail);
        assert_eq!(
            item.fix,
            Some(format!(
                "{}.",
                axocoatl_isolation::podman::install_fix(&here)
            ))
        );
        assert_eq!(blocked.as_deref(), Some(item.detail.as_str()));
        let (item, blocked) = podman_item(&PodmanReadiness::MachineStopped, &here);
        assert_eq!(item.status, ReadinessStatus::Ok);
        assert!(blocked.is_none());
        let (item, blocked) = podman_item(&PodmanReadiness::Unavailable("timed out".into()), &here);
        assert_eq!(item.status, ReadinessStatus::Fail);
        assert!(blocked.is_none(), "a failed probe is reported, not a block");
    }

    /// The first screen's fix for a missing Podman names onboarding once and
    /// says it installs Podman only where it does, on every platform.
    #[test]
    fn a_missing_podman_fix_says_once_what_onboarding_does_on_each_platform() {
        let fix = |route: InstallRoute| {
            podman_item(&PodmanReadiness::NotInstalled, &route)
                .0
                .fix
                .unwrap()
        };
        assert_eq!(
            fix(InstallRoute::for_mac(true, true)),
            "Run `axocoatl onboard` (it installs Podman after asking), or: `brew install podman`."
        );
        assert_eq!(
            fix(InstallRoute::for_mac(true, false)),
            "Install Podman with the installer from https://podman.io (or install Homebrew and \
             run `brew install podman`), then run `axocoatl onboard`."
        );
        assert!(fix(InstallRoute::for_mac(false, true))
            .starts_with("Podman 6 no longer supports Intel Macs"));
        assert_eq!(
            fix(InstallRoute::for_linux(UBUNTU)),
            "Run `axocoatl onboard` (it installs Podman after asking), or: `sudo apt-get install \
             -y podman`."
        );
        assert_eq!(
            fix(InstallRoute::for_linux("ID=ubuntu\nVERSION_ID=\"22.04\"\n")),
            "This Ubuntu's Podman is too old (22.04 ships 3.4.4; Axocoatl runs need 4.3 or \
             later, 4.9 tested): use Ubuntu 24.04 or newer."
        );
        assert_eq!(
            fix(InstallRoute::for_linux("ID=nixos\n")),
            "Install Podman 4.9 or later (https://podman.io/docs/installation), then run \
             `axocoatl onboard`."
        );
    }

    #[test]
    fn a_refused_ollama_version_and_cloud_mode_fail_with_what_to_do() {
        let items = ollama_server_items(
            "http://127.0.0.1:11434",
            &check(
                "0.20.6",
                OllamaCloudMode::Disabled {
                    source: "config".into(),
                },
            ),
        );
        assert_eq!(items[0].id, "ollama.version");
        assert_eq!(items[0].status, ReadinessStatus::Fail);
        assert!(
            items[0].detail.contains("reports version 0.20.6"),
            "{:?}",
            items[0]
        );
        assert!(items[0].fix.as_deref().unwrap().contains("Install Ollama"));
        assert_eq!(items[1].status, ReadinessStatus::Ok);

        let items = ollama_server_items(
            "http://127.0.0.1:11434",
            &check(
                axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION,
                OllamaCloudMode::Enabled,
            ),
        );
        assert_eq!(items[0].status, ReadinessStatus::Ok);
        assert_eq!(items[1].status, ReadinessStatus::Fail);
        assert!(items[1]
            .fix
            .as_deref()
            .unwrap()
            .contains("disable_ollama_cloud"));

        let unreachable =
            ollama_server_items("http://127.0.0.1:11434", &Err("connection refused".into()));
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].status, ReadinessStatus::Fail);
        assert!(unreachable[0]
            .fix
            .as_deref()
            .unwrap()
            .contains("ollama serve"));
        let remote = ollama_server_items("http://192.0.2.10:11434", &Err(String::new()));
        assert!(remote[0].detail.contains("cannot use Ollama"));
    }

    #[test]
    fn models_name_the_exact_pull_and_never_a_default_model() {
        let items = ollama_model_items(
            &["gpt-oss:20b".into(), "qwen3-coder:30b".into()],
            &["qwen3-coder:30b".into()],
        );
        assert_eq!(items[0].status, ReadinessStatus::Fail);
        assert_eq!(
            items[0].fix.as_deref(),
            Some("Run `ollama pull gpt-oss:20b`.")
        );
        assert_eq!(items[1].status, ReadinessStatus::Ok);
        assert!(items.iter().all(|item| !item.detail.contains("llama3.2")
            && !item.fix.as_deref().unwrap_or_default().contains("llama3.2")));
    }

    #[test]
    fn writers_are_optional_and_named_by_credential_only() {
        let none = writer_items(&[]);
        assert!(none.iter().all(|item| item.status == ReadinessStatus::Ok));
        assert!(none[0].detail.contains("axocoatl connect claude-code"));
        let connected = writer_items(&["claude-code-oauth".into(), "codex-chatgpt".into()]);
        assert!(connected[0].detail.starts_with("Connected"));
        assert!(connected[1].detail.starts_with("Connected"));
    }

    #[test]
    fn the_report_is_ready_only_without_a_failure() {
        let mut sections = ReadinessSections::default();
        sections
            .sandbox
            .push(podman_item(&PodmanReadiness::Ready, &InstallRoute::detect()).0);
        sections.models.push(ReadinessItem::new(
            "m",
            "M",
            ReadinessStatus::Warn,
            "note",
            None,
        ));
        let report = ReadinessReport::from_sections(sections.clone(), None);
        assert!(report.ready);
        assert_eq!(report.version, DAEMON_VERSION);
        sections.sandbox[0] =
            podman_item(&PodmanReadiness::MachineMissing, &InstallRoute::detect()).0;
        let report = ReadinessReport::from_sections(sections, Some("blocked".into()));
        assert!(!report.ready);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["sections"]["sandbox"][0]["status"], "fail");
        assert_eq!(json["sections"]["sandbox"][0]["name"], "Podman");
        assert!(json["sections"]["models"][0]["fix"].is_null());
        assert_eq!(json["sessions_blocked"], "blocked");
    }
}
