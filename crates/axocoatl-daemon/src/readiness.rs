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

static STARTED_AT_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// When this daemon started (Unix milliseconds), as `/health` names it.
/// The first call fixes it: `axocoatl dev` and `axocoatl serve` make it
/// before they read the configuration, so a configuration file changed
/// later is newer than the one the daemon runs (the router's call covers
/// other entry points).
pub fn started_at_ms() -> u64 {
    *STARTED_AT_MS.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    })
}

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
                // Without newuidmap or newgidmap, Podman's own message says
                // to run `podman system migrate`, which alone changes nothing.
                Some(
                    match axocoatl_isolation::podman_report::id_map_tools_hint(error) {
                        Some(hint) => format!("{hint}."),
                        None => "Run `axocoatl doctor` to see what Podman reports.".to_string(),
                    },
                ),
            ),
            None,
        ),
    }
}

/// The resource caps a Session container gets on this Podman host (`host`,
/// `container_host`), when it cannot get every one: a warning naming each
/// cap left out, or, with `sandbox.require_resource_limits` (`require`), a
/// failure, since Sessions then refuse to start (the second value says so
/// for the blocked Create session). `None` when every cap applies.
pub fn resource_caps_item(
    host: &axocoatl_isolation::container_host::ContainerHost,
    require: bool,
) -> Option<(ReadinessItem, Option<String>)> {
    let missing = host.caps.not_applied();
    if missing.is_empty() {
        return None;
    }
    let controllers = format!("[{}]", host.controllers.join(", "));
    let fix = "Run `axocoatl doctor`: it says how to give rootless Podman the missing cgroup \
               controllers."
        .to_string();
    Some(if require {
        let detail = format!(
            "Podman can use the cgroup controllers {controllers}, so a Session container would \
             run without its {} cap, and sandbox.require_resource_limits refuses to start it.",
            missing.join(", ")
        );
        (
            ReadinessItem::new(
                "sandbox.resource_caps",
                "Resource caps",
                ReadinessStatus::Fail,
                detail.clone(),
                Some(format!(
                    "{fix} Or set sandbox.require_resource_limits to false to start Sessions \
                     without that cap."
                )),
            ),
            Some(detail),
        )
    } else {
        (
            ReadinessItem::new(
                "sandbox.resource_caps",
                "Resource caps",
                ReadinessStatus::Warn,
                format!(
                    "Podman can use the cgroup controllers {controllers}, so Session containers \
                     run without their {} cap; each Session's record and each run's notes and \
                     receipt say so.",
                    missing.join(", ")
                ),
                Some(fix),
            ),
            None,
        )
    })
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

/// One item per pulled model: the context Ollama loaded it with, when it
/// has it loaded (`contexts`, from `/api/ps`, which loads nothing), against
/// the 16,384 tokens a run needs; `context_fix` gives Ollama a larger one.
/// A model that is not loaded has no context yet: Ollama sets it when it
/// loads the model, which `axocoatl doctor` does.
pub fn ollama_context_items(
    contexts: &[(String, Result<Option<usize>, String>)],
    context_fix: &str,
) -> Vec<ReadinessItem> {
    let floor = axocoatl_config::loadout::MIN_RUN_CONTEXT_TOKENS;
    contexts
        .iter()
        .map(|(model, context)| {
            let (status, detail, fix) = match context {
                Ok(Some(tokens)) if (*tokens as u64) < floor => (
                    ReadinessStatus::Fail,
                    format!(
                        "Ollama has {model} loaded with a {tokens}-token context, and a run needs \
                         at least {floor} (the fix writer's first call alone is about 9,700 \
                         tokens)."
                    ),
                    Some(format!(
                        "Give Ollama a {}-token context: {context_fix}.",
                        crate::loadout::admission::CONTEXT_FIX_TOKENS
                    )),
                ),
                Ok(Some(tokens)) => (
                    ReadinessStatus::Ok,
                    format!("Ollama has it loaded with a {tokens}-token context."),
                    None,
                ),
                Ok(None) => (
                    ReadinessStatus::Ok,
                    "Not loaded now: Ollama sets its context when it loads it. `axocoatl doctor` \
                     loads it and checks that context."
                        .to_string(),
                    None,
                ),
                Err(error) => (
                    ReadinessStatus::Warn,
                    format!("Could not read its context from Ollama: {error}"),
                    Some("Run `axocoatl doctor`.".to_string()),
                ),
            };
            ReadinessItem::new(
                format!("ollama.context:{model}"),
                format!("Context of {model}"),
                status,
                detail,
                fix,
            )
        })
        .collect()
}

async fn ollama_tags(base_url: &str) -> Result<Vec<String>, String> {
    // Ollama is on this computer or its network: never through a proxy.
    let client = reqwest::Client::builder()
        .timeout(TAGS_TIMEOUT)
        .no_proxy()
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
        let podman = axocoatl_isolation::podman::detect().await;
        let (item, blocked) = podman_item(&podman, &InstallRoute::detect());
        sections.sandbox.push(item);
        sessions_blocked = blocked;
        if matches!(podman, PodmanReadiness::Ready) {
            if let Some((item, blocked)) = axocoatl_isolation::container_host::container_host()
                .await
                .and_then(|host| resource_caps_item(&host, config.sandbox.require_resource_limits))
            {
                sections.sandbox.push(item);
                sessions_blocked = sessions_blocked.or(blocked);
            }
        }
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
                Ok(pulled) => {
                    let items = ollama_model_items(&wanted, &pulled);
                    // The context each pulled model is loaded with, when it
                    // is: read, never loaded.
                    let mut contexts = Vec::new();
                    for (model, item) in wanted.iter().zip(&items) {
                        if item.status != ReadinessStatus::Ok {
                            continue;
                        }
                        let context = tokio::time::timeout(
                            TAGS_TIMEOUT,
                            axocoatl_llm_ollama::inspect_native_ollama_model(
                                &ollama.base_url,
                                model,
                            ),
                        )
                        .await
                        .map_err(|_| "Ollama did not answer in time".to_string())
                        .and_then(|inspected| inspected.map_err(|error| error.to_string()));
                        contexts.push((model.clone(), context));
                    }
                    sections.models.extend(items);
                    sections.models.extend(ollama_context_items(
                        &contexts,
                        &crate::loadout::admission::OllamaInstall::here()
                            .context_fix(&crate::loadout::admission::brew_services_env()),
                    ));
                }
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
        assert_eq!(
            item.fix.as_deref(),
            Some("Run `axocoatl doctor` to see what Podman reports.")
        );
    }

    /// Podman 4.9.3 with a range in /etc/subuid and no newuidmap on PATH:
    /// `podman info` fails, and Podman's message says to run `podman system
    /// migrate`. The first screen names the missing helper and its package
    /// instead.
    #[test]
    fn a_podman_without_newuidmap_is_told_to_install_it() {
        let error = "OCI container setup failed: podman info failed: Error: command required \
                     for rootless mode with multiple IDs: exec: \"newuidmap\": executable file \
                     not found in $PATH";
        let (item, blocked) = podman_item(
            &PodmanReadiness::Unavailable(error.into()),
            &InstallRoute::for_linux(UBUNTU),
        );
        assert_eq!(item.status, ReadinessStatus::Fail);
        assert!(blocked.is_none());
        let hint = "Rootless Podman needs newuidmap and newgidmap to map your subordinate user \
                    and group IDs, and newuidmap is not found on PATH: install (or reinstall) \
                    them (Debian and Ubuntu: uidmap; Fedora and RHEL: shadow-utils; Arch: \
                    shadow), then run `podman system migrate`; `axocoatl doctor` names the \
                    command for this computer";
        assert_eq!(item.fix, Some(format!("{hint}.")));
        let refused = crate::loadout::admission::podman_not_ready(error);
        assert!(refused.ends_with(hint), "{refused}");
        assert!(!crate::loadout::admission::podman_unanswered("timed out").contains("newuidmap"));
        // A loadout run that made Podman ready first (`ensure_ready`) gets
        // its message, which says it already: once.
        let ensured = axocoatl_isolation::podman::unavailable_message(error);
        assert!(ensured.ends_with(hint), "{ensured}");
        let refused = crate::loadout::admission::podman_not_ready(&ensured);
        assert_eq!(refused.matches(hint).count(), 1, "{refused}");
        assert!(refused.ends_with(hint), "{refused}");
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

    /// A pulled model Ollama has loaded with less context than a run needs
    /// fails, with how to give it more; one loaded with enough passes; one
    /// not loaded passes, saying its context is set when it loads.
    #[test]
    fn a_loaded_models_context_is_checked_against_what_a_run_needs() {
        let fix = "add OLLAMA_CONTEXT_LENGTH=32768 to ~/.homebrew/services/ollama.env and run \
                   `brew services restart ollama`";
        let items = ollama_context_items(
            &[
                ("qwen3-coder:30b".into(), Ok(Some(4096))),
                ("gpt-oss:20b".into(), Ok(Some(32768))),
                ("devstral:24b".into(), Ok(None)),
                ("mistral-small3.2:24b".into(), Err("model not found".into())),
            ],
            fix,
        );
        assert_eq!(items[0].id, "ollama.context:qwen3-coder:30b");
        assert_eq!(items[0].status, ReadinessStatus::Fail);
        assert!(
            items[0].detail.contains("4096-token context"),
            "{:?}",
            items[0]
        );
        assert!(items[0].detail.contains("at least 16384"), "{:?}", items[0]);
        assert_eq!(
            items[0].fix.as_deref(),
            Some(format!("Give Ollama a 32768-token context: {fix}.").as_str())
        );
        assert_eq!(items[1].status, ReadinessStatus::Ok);
        assert!(items[1].detail.contains("32768-token"));
        assert_eq!(items[2].status, ReadinessStatus::Ok);
        assert!(items[2].detail.contains("axocoatl doctor"));
        assert_eq!(items[3].status, ReadinessStatus::Warn);
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

    /// A host that cannot apply a resource cap is a warning that names it,
    /// or a failure that blocks Sessions when every cap is required; one
    /// that can apply them all says nothing.
    #[test]
    fn a_resource_cap_the_host_cannot_apply_is_said() {
        use axocoatl_isolation::container_host::ContainerHost;
        let all = ContainerHost::parse(r#"["cpu","io","memory","pids"] false"#, true).unwrap();
        assert!(resource_caps_item(&all, true).is_none());
        let no_cpu = ContainerHost::parse(r#"["memory","pids"] false"#, true).unwrap();
        let (item, blocked) = resource_caps_item(&no_cpu, false).unwrap();
        assert_eq!(item.status, ReadinessStatus::Warn);
        assert!(
            item.detail.contains("[memory, pids]")
                && item
                    .detail
                    .contains("without their CPU (2 CPUs, cgroup controller cpu) cap"),
            "{item:?}"
        );
        assert!(item.fix.as_deref().unwrap().contains("axocoatl doctor"));
        assert_eq!(blocked, None);
        let (item, blocked) = resource_caps_item(&no_cpu, true).unwrap();
        assert_eq!(item.status, ReadinessStatus::Fail);
        assert!(
            item.fix
                .as_deref()
                .unwrap()
                .contains("sandbox.require_resource_limits to false"),
            "{item:?}"
        );
        assert!(blocked.unwrap().contains("refuses to start it"));
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
