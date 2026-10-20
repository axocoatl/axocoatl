//! `axocoatl doctor`, and the check onboarding's Step 5 runs: one report in
//! four sections (Sandbox, Models, Writers, Daemon), each item OK, WARN or
//! FAIL with its fix, then what is ready to run and the steps left.
//! `doctor` exits 0 only when nothing failed. `--json` prints the same
//! report as one JSON document:
//! `{"ready": bool, "config": path, "sections": {"sandbox": [...], "models":
//! [...], "writers": [...], "daemon": [...]}, "ready_for": [...],
//! "steps_left": [...]}`, each item `{"name", "status": "ok|warn|fail",
//! "detail", "fix"}`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use axocoatl_isolation::podman_report::{self, Level, Platform};
use serde::Serialize;

use crate::ollama_setup::{self, OllamaHost};
use crate::onboard::config::{self as onboard_config, ModelRef, Provider};
use crate::onboard::detect::{self, Os};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Item {
    pub(crate) name: String,
    pub(crate) status: Status,
    pub(crate) detail: String,
    pub(crate) fix: Option<String>,
}

impl Item {
    /// The item as one line: its detail, after its name unless the detail
    /// starts by saying what it is about.
    pub(crate) fn line(&self) -> String {
        let name = self.name.to_ascii_lowercase();
        let key = name.trim_start_matches("model ");
        let detail = self.detail.to_ascii_lowercase().replace(['\'', '"'], "");
        if detail.starts_with(&name) || detail.starts_with(key) {
            self.detail.clone()
        } else {
            format!("{}: {}", self.name, self.detail)
        }
    }

    /// The item as a step left: the line and its fix.
    pub(crate) fn step(&self) -> String {
        match &self.fix {
            Some(fix) => format!("{}: {}", self.line(), fix),
            None => self.line(),
        }
    }

    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: Status::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &str, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            status: Status::Warn,
            detail: detail.into(),
            fix,
        }
    }

    fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: Status::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Sections {
    pub(crate) sandbox: Vec<Item>,
    pub(crate) models: Vec<Item>,
    pub(crate) writers: Vec<Item>,
    pub(crate) daemon: Vec<Item>,
}

impl Sections {
    fn all(&self) -> impl Iterator<Item = &Item> {
        self.sandbox
            .iter()
            .chain(&self.models)
            .chain(&self.writers)
            .chain(&self.daemon)
    }
}

/// One way to run Axocoatl and whether it is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RunPath {
    pub(crate) command: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) ready: bool,
    pub(crate) config: String,
    pub(crate) sections: Sections,
    pub(crate) ready_for: Vec<RunPath>,
    pub(crate) steps_left: Vec<String>,
}

/// How a model on Ollama fared.
#[derive(Debug, Clone, Default)]
struct ModelState {
    ok: bool,
    why: String,
}

/// What the check needs about this computer that a test may give instead.
pub(crate) struct Environment {
    pub(crate) os: Os,
    pub(crate) user: String,
    pub(crate) memory_bytes: Option<u64>,
    pub(crate) cpus: u64,
    pub(crate) home: PathBuf,
    pub(crate) xdg_config_home: Option<PathBuf>,
    /// Find a program on PATH.
    pub(crate) host: crate::onboard::host::Host,
    /// Load each configured Ollama model to read its context.
    pub(crate) load_models: bool,
    /// The daemon's IPC socket.
    pub(crate) socket_path: PathBuf,
    /// The Podman client.
    pub(crate) podman: String,
    /// The data directory, when it is not the one the configuration selects
    /// (tests).
    pub(crate) data_dir: Option<PathBuf>,
    /// The kernel's Landlock ABI on Linux.
    pub(crate) landlock_abi: Option<i64>,
}

impl Environment {
    pub(crate) async fn current() -> Result<Self, String> {
        let host = crate::onboard::host::Host::from_env(None)?;
        let os = if host.macos {
            Os::MacOs {
                version: None,
                apple_silicon: std::env::consts::ARCH == "aarch64",
                chip: None,
            }
        } else {
            let release = detect::OsRelease::parse(
                &std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
            );
            let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
            Os::Linux {
                release,
                wsl: detect::wsl_from(&kernel, std::env::var("WSL_DISTRO_NAME").ok().as_deref()),
                systemd: Path::new("/run/systemd/system").is_dir(),
            }
        };
        Ok(Self {
            os,
            user: detect::current_user(),
            memory_bytes: axocoatl_isolation::host::memory_bytes(),
            cpus: axocoatl_isolation::host::cpu_count(),
            home: host.home.clone(),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            host,
            load_models: true,
            socket_path: axocoatl_daemon::ipc::default_socket_path(),
            podman: "podman".to_string(),
            data_dir: None,
            landlock_abi: detect::landlock_abi(),
        })
    }

    fn platform(&self) -> Platform {
        let spec = axocoatl_isolation::podman::machine_spec(
            self.memory_bytes
                .unwrap_or((axocoatl_isolation::podman::TESTED_MACHINE_MEMORY_MIB * 2) << 20),
            self.cpus,
        );
        Platform {
            machine: self.os.is_macos(),
            user: self.user.clone(),
            install_fix: podman_install_fix(
                &self.os,
                self.os.is_macos() && self.host.find("brew").is_some(),
            ),
            too_old_note: self
                .os
                .release()
                .filter(|release| release.podman_too_old())
                .map(|_| "Ubuntu 22.04 ships Podman 3.4.4; use Ubuntu 24.04 or newer".to_string()),
            spec,
        }
    }
}

/// How to install Podman here; `homebrew`: Homebrew is installed (macOS),
/// so onboarding can install Podman with it.
pub(crate) fn podman_install_fix(os: &Os, homebrew: bool) -> String {
    match os {
        Os::MacOs {
            apple_silicon: false,
            ..
        } => "Podman 6 no longer supports Intel Macs and Homebrew refuses it: install the Podman \
              5.8 amd64 package from https://github.com/containers/podman/releases (Axocoatl has \
              not been tested on Intel Macs), then run `axocoatl onboard`"
            .to_string(),
        Os::MacOs { .. } if homebrew => {
            "Run `axocoatl onboard` (it installs Podman after asking), or: brew install podman"
                .to_string()
        }
        Os::MacOs { .. } => "Install Podman with the installer from https://podman.io (or \
                             install Homebrew and run `brew install podman`), then run \
                             `axocoatl onboard`"
            .to_string(),
        Os::Linux { release, .. } => match release.package_manager() {
            Some(manager) => format!(
                "Run `axocoatl onboard` (it installs Podman after asking), or: {}",
                manager.install(&["podman"]).display()
            ),
            None => "Install Podman 4.9 or later (https://podman.io/docs/installation), then run \
                     `axocoatl onboard`"
                .to_string(),
        },
    }
}

/// Linux: the subordinate ids `/etc/subuid` and `/etc/subgid` (under the
/// host's root) give `user`, this process's user. `None` on macOS, where
/// the machine has its own.
pub(crate) fn subordinate_ids(
    host: &crate::onboard::host::Host,
    os: &Os,
    user: &str,
) -> Option<podman_report::SubordinateIds> {
    if os.is_macos() {
        return None;
    }
    let uid = {
        #[cfg(unix)]
        {
            // SAFETY: getuid takes no arguments and cannot fail.
            Some(unsafe { libc::getuid() })
        }
        #[cfg(not(unix))]
        {
            None
        }
    };
    podman_report::read_subordinate_ids(&host.root, user, uid)
}

/// The Landlock line for a Linux kernel with `abi` (`None`: no Landlock).
pub(crate) fn landlock_item(
    abi: Option<i64>,
    release: Option<&detect::OsRelease>,
    wsl: bool,
) -> Item {
    let kernel_fix = if wsl {
        "Run `wsl --update` in PowerShell, then restart WSL".to_string()
    } else if release.is_some_and(|release| release.id == "debian") {
        "Install a newer kernel from bookworm-backports (`sudo apt-get install -t \
         bookworm-backports linux-image-$(dpkg --print-architecture)`), then reboot"
            .to_string()
    } else {
        "Upgrade to Linux 6.2 or later (6.7 or later for read-only helper shells)".to_string()
    };
    match abi {
        None | Some(0) => Item::fail(
            "Landlock",
            "this kernel has no Landlock; the Session supervisor's filter needs it",
            kernel_fix,
        ),
        Some(abi @ (1 | 2)) => Item::fail(
            "Landlock",
            format!(
                "Landlock ABI {abi}: fix, fix-claude-code, fix-codex and audit need Linux 6.2 or \
                 later (Landlock ABI 3) for their read-only reviewer"
            ),
            kernel_fix,
        ),
        Some(3) => Item::warn(
            "Landlock",
            "Landlock ABI 3: read-only helper shells need Linux 6.7 or later (ABI 4) for their \
             network restriction, so they do not run here",
            Some(kernel_fix),
        ),
        Some(abi) => Item::ok("Landlock", format!("Landlock ABI {abi}")),
    }
}

fn level(level: Level) -> Status {
    match level {
        Level::Ok => Status::Ok,
        Level::Warn => Status::Warn,
        Level::Fail => Status::Fail,
    }
}

/// The models configured for native Agents on Ollama, from the Agents and
/// `run.models`.
fn ollama_models(
    config: &axocoatl_config::AxocoatlConfig,
    run: &onboard_config::RunModels,
) -> BTreeMap<String, bool> {
    let default_model = config
        .providers
        .ollama
        .as_ref()
        .and_then(|ollama| ollama.model.clone())
        .unwrap_or_else(|| "llama3.2".to_string());
    let mut models = BTreeMap::new();
    for agent in config
        .agents
        .iter()
        .filter(|agent| agent.provider == "ollama")
    {
        let model = if agent.model.is_empty() {
            default_model.clone()
        } else {
            agent.model.clone()
        };
        *models.entry(model).or_default() |= !agent.tools.is_empty();
    }
    for spec in [&run.writer, &run.reviewer].into_iter().flatten() {
        if let Ok(model) = ModelRef::parse(spec) {
            if model.provider == Provider::Ollama {
                // A run's writer calls tools; its reviewer reads files.
                *models.entry(model.model).or_default() |= true;
            }
        }
    }
    models
}

/// Collect the report for `config_path`.
pub(crate) async fn collect(config_path: &Path, env: &Environment) -> Report {
    let mut sections = Sections::default();
    let mut model_states: BTreeMap<String, ModelState> = BTreeMap::new();
    let mut openrouter_ready = false;

    // ── Daemon: the configuration first; the rest needs it ──
    let text = std::fs::read_to_string(config_path);
    let config = match &text {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            sections.daemon.push(Item::fail(
                "Config",
                format!(
                    "Config missing: no configuration at {}",
                    config_path.display()
                ),
                "Run `axocoatl onboard`",
            ));
            None
        }
        _ => match load(config_path, env).await {
            Ok(config) => {
                sections.daemon.push(Item::ok(
                    "Config",
                    format!("Config valid: {}", config_path.display()),
                ));
                for warning in axocoatl_config::no_tools_warnings(&config) {
                    sections.daemon.push(Item::warn(
                        "Config",
                        warning.problem(),
                        Some(axocoatl_config::NoToolsWarning::HINT.to_string()),
                    ));
                }
                for warning in axocoatl_config::network_warnings(&config) {
                    sections.daemon.push(Item::warn(
                        "Config",
                        warning.field,
                        Some(warning.message),
                    ));
                }
                Some(config)
            }
            Err(error) => {
                sections.daemon.push(Item::fail(
                    "Config",
                    format!("Config invalid: {}", config_path.display()),
                    error,
                ));
                None
            }
        },
    };
    let run = text
        .as_deref()
        .map(onboard_config::run_models)
        .unwrap_or_default();
    // Who writes, as onboarding recorded it; else what `run.models.writer`
    // implies.
    let marker = text.as_deref().ok().and_then(onboard_config::writer_marker);
    let chosen_writer = marker
        .as_ref()
        .map(|marker| marker.writer.clone())
        .or_else(|| {
            run.writer
                .as_deref()
                .and_then(|spec| ModelRef::parse(spec).ok())
                .map(|model| model.provider.id().to_string())
        });

    // ── Sandbox ──
    let mut report = podman_report::inspect_with(&env.podman).await;
    report.subordinate_ids = subordinate_ids(&env.host, &env.os, &env.user);
    let e2b = config
        .as_ref()
        .is_some_and(|config| config.sandbox.backend == "e2b");
    // `axocoatl run` always needs Podman, whatever runs workbench Sessions:
    // whether it is ready is decided before an E2B setup softens the lines.
    let mut podman_ok = true;
    for finding in podman_report::findings(&report, &env.platform()) {
        let mut status = level(finding.level);
        let mut detail = finding.detail;
        if status == Status::Fail {
            podman_ok = false;
            if e2b {
                status = Status::Warn;
                detail.push_str(" (workbench Sessions use E2B; axocoatl run still needs Podman)");
            }
        }
        sections.sandbox.push(Item {
            name: finding.name.to_string(),
            status,
            detail,
            fix: finding.fix,
        });
    }
    if let Os::Linux { release, wsl, .. } = &env.os {
        sections.sandbox.push(landlock_item(
            env.landlock_abi,
            Some(release),
            wsl.is_some(),
        ));
    }
    let sandbox_ok = podman_ok
        && !sections
            .sandbox
            .iter()
            .any(|item| item.status == Status::Fail);
    if let Some(config) = &config {
        sections.sandbox.push(Item::ok(
            "Sandbox network",
            crate::sandbox_network_doctor_line(&config.sandbox),
        ));
        match crate::sandbox_workload_doctor_line(&config.sandbox, report.rootless()) {
            Ok(line) => sections.sandbox.push(Item::ok("Sandbox workload", line)),
            Err((line, hint)) => {
                sections
                    .sandbox
                    .push(Item::warn("Sandbox workload", line, Some(hint)))
            }
        }
        for line in crate::credentials_doctor_lines(config) {
            sections.sandbox.push(match line.hint {
                None => Item::ok("Credentials", line.text),
                Some(hint) => Item::warn("Credentials", line.text, Some(hint.to_string())),
            });
        }
        if let Some(browser) = &config.browser {
            let image = browser
                .image
                .clone()
                .unwrap_or_else(|| axocoatl_config::DEFAULT_BROWSER_IMAGE.to_string());
            sections.sandbox.push(
                match axocoatl_daemon::browser_install::image_present(&image).await {
                    Ok(true) => Item::ok("Browser", format!("image {image} present")),
                    Ok(false) => Item::warn(
                        "Browser",
                        format!("image {image} missing"),
                        Some(
                            "Run `axocoatl browser install` before Agents use the browser tools."
                                .into(),
                        ),
                    ),
                    Err(error) => {
                        Item::warn("Browser", format!("image {image} not checked"), Some(error))
                    }
                },
            );
        }
    }

    // ── Models ──
    if let Some(config) = &config {
        if let Some(ollama) = &config.providers.ollama {
            let wanted = ollama_models(config, &run);
            let cli = env.host.find("ollama");
            let cli_version = match &cli {
                Some(path) => env
                    .host
                    .probe_env(
                        &path.to_string_lossy(),
                        &["--version"],
                        &[("OLLAMA_HOST", ollama.base_url.as_str())],
                    )
                    .await
                    .and_then(|probe| {
                        detect::parse_ollama_version(&format!("{}\n{}", probe.stdout, probe.stderr))
                            .0
                    }),
                None => None,
            };
            let service = detect::ollama_service(
                &env.host,
                env.xdg_config_home.as_deref(),
                detect::url_port(&ollama.base_url),
            );
            let host = OllamaHost {
                service,
                macos: env.os.is_macos(),
                brew_env_file: Some(
                    detect::homebrew_services_dir(&env.home, env.xdg_config_home.as_deref())
                        .join("ollama.env"),
                ),
                cli_version,
            };
            let server_ready = check_ollama(
                &ollama.base_url,
                &host,
                &wanted,
                env,
                &mut sections.models,
                &mut model_states,
            )
            .await;
            if !server_ready {
                for model in wanted.keys() {
                    model_states.entry(model.clone()).or_insert(ModelState {
                        ok: false,
                        why: format!("the Ollama server at {} is not ready", ollama.base_url),
                    });
                }
            }
        }
        let uses_openrouter = config
            .agents
            .iter()
            .any(|agent| agent.provider == "openrouter")
            || [&run.writer, &run.reviewer]
                .into_iter()
                .flatten()
                .any(|spec| spec.starts_with("openrouter:"));
        let key_present =
            |key: &str| !(key.is_empty() || key.contains("your-key") || key.starts_with("${"));
        if let Some(openrouter) = &config.providers.openrouter {
            if key_present(openrouter.api_key.expose_secret()) {
                openrouter_ready = true;
                sections
                    .models
                    .push(Item::ok("OpenRouter", "OpenRouter API key present"));
            } else if uses_openrouter {
                sections.models.push(Item::fail(
                    "OpenRouter",
                    "OpenRouter API key not set, and an Agent or run default uses OpenRouter",
                    crate::missing_provider_key_hint("OpenRouter"),
                ));
            } else {
                sections.models.push(Item::warn(
                    "OpenRouter",
                    "OpenRouter API key not set",
                    Some(crate::missing_provider_key_hint("OpenRouter")),
                ));
            }
        } else if uses_openrouter {
            sections.models.push(Item::fail(
                "OpenRouter",
                "an Agent or run default uses OpenRouter, which is not configured",
                "Run `axocoatl onboard --update --writer openrouter`",
            ));
        }
        for (name, credentials) in [
            ("OpenAI", &config.providers.openai),
            ("Anthropic", &config.providers.anthropic),
            ("Gemini", &config.providers.gemini),
            ("Mistral", &config.providers.mistral),
        ] {
            if let Some(credentials) = credentials {
                sections
                    .models
                    .push(if key_present(credentials.api_key.expose_secret()) {
                        Item::ok(name, format!("{name} API key present"))
                    } else {
                        Item::warn(
                            name,
                            format!("{name} API key not set"),
                            Some(crate::missing_provider_key_hint(name)),
                        )
                    });
            }
        }
    }

    // ── Writers ──
    let data_dir = match &env.data_dir {
        Some(data_dir) => Some(data_dir.clone()),
        None => crate::configure_data_dir(config_path).ok(),
    };
    let mut secrets = BTreeSet::new();
    let mut images = BTreeSet::new();
    if let Some(data_dir) = data_dir.as_ref().filter(|dir| dir.is_dir()) {
        if let Ok(names) = axocoatl_daemon::secret_store::list_secrets(data_dir) {
            secrets.extend(names);
        }
        if let Ok(records) =
            axocoatl_daemon::external_agent::recipe_images::recorded_images_at(data_dir)
        {
            images.extend(records.into_iter().map(|record| record.recipes.join(" ")));
        }
    }
    let claude_code = writer_items(
        &WriterPath {
            name: "Claude Code",
            recipe: "claude-code",
            credentials: &[axocoatl_daemon::external_agent::claude_code::CLAUDE_CODE_SECRET],
            connect: "axocoatl connect claude-code",
            chosen: chosen_writer.as_deref() == Some("claude-code"),
        },
        images.contains("claude-code"),
        &secrets,
        &mut sections.writers,
    );
    // A Codex setup names its credential: that one must be stored.
    let codex_credential = marker
        .as_ref()
        .filter(|marker| marker.writer == "codex")
        .and_then(|marker| marker.credential.clone());
    let codex_credentials: Vec<&str> = match codex_credential.as_deref() {
        Some(credential) => vec![credential],
        None => vec![
            axocoatl_daemon::external_agent::codex::CODEX_CHATGPT_SECRET,
            axocoatl_daemon::external_agent::codex::CODEX_SECRET,
        ],
    };
    let codex_connect = if codex_credential.as_deref()
        == Some(axocoatl_daemon::external_agent::codex::CODEX_SECRET)
    {
        "axocoatl connect codex --api-key-only"
    } else {
        "axocoatl connect codex"
    };
    let codex = writer_items(
        &WriterPath {
            name: "Codex",
            recipe: "codex",
            credentials: &codex_credentials,
            connect: codex_connect,
            chosen: chosen_writer.as_deref() == Some("codex"),
        },
        images.contains("codex"),
        &secrets,
        &mut sections.writers,
    );
    match crate::connect_cmd::find_claude(
        None,
        Some(env.host.run_path().as_os_str()),
        Some(&env.home),
    ) {
        Ok(path) => {
            let version = env
                .host
                .probe(&path.to_string_lossy(), &["--version"])
                .await
                .filter(|probe| probe.success)
                .map(|probe| probe.stdout.trim().to_string())
                .unwrap_or_default();
            sections.writers.push(Item::ok(
                "claude CLI",
                format!("{} {}", path.display(), version).trim().to_string(),
            ));
        }
        Err(_) => sections.writers.push(Item::ok(
            "claude CLI",
            "not found; needed only to sign in with `axocoatl connect claude-code`",
        )),
    }
    sections.writers.push(match env.host.find("codex") {
        Some(path) => Item::ok("codex CLI", path.display().to_string()),
        None => Item::ok(
            "codex CLI",
            "not found; not needed: `axocoatl connect codex` runs the pinned Codex in a container",
        ),
    });
    sections.writers.push(match env.host.find("gh") {
        None => Item::ok("gh", "not found; needed only for `axocoatl run --keep pr`"),
        Some(path) => match env
            .host
            .probe(&path.to_string_lossy(), &["auth", "status"])
            .await
        {
            Some(probe) if probe.success => Item::ok("gh", "signed in (for `--keep pr`)"),
            _ => Item::warn(
                "gh",
                "not signed in; `axocoatl run --keep pr` needs it",
                Some("gh auth login".to_string()),
            ),
        },
    });
    if !secrets.is_empty() {
        sections.writers.push(Item::ok(
            "Stored credentials",
            secrets.iter().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    // ── Daemon ──
    if let Some(data_dir) = data_dir.as_ref().filter(|_| config.is_some()) {
        match crate::probe_data_dir(data_dir) {
            Ok(()) => {
                sections.daemon.push(Item::ok(
                    "Data dir",
                    format!("Data dir writable: {}", data_dir.display()),
                ));
                let ownership =
                    axocoatl_session::execution_ownership::inspect_format_ownership(data_dir);
                sections
                    .daemon
                    .push(match crate::format_ownership_doctor_line(&ownership) {
                        Ok(line) => Item::ok("Data dir ownership", line),
                        Err((line, hint)) => Item::fail("Data dir ownership", line, hint),
                    });
            }
            Err(error) => sections.daemon.push(Item::fail(
                "Data dir",
                format!("Data dir not writable: {}", data_dir.display()),
                error.to_string(),
            )),
        }
    }
    let (server_host, port) = config
        .as_ref()
        .map(|config| (config.server.host.clone(), config.server.port))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 8080));
    match axocoatl_daemon::ipc::IpcClient::connect(&env.socket_path).await {
        Ok(mut client) => {
            let version = daemon_version(&server_host, port).await;
            let cli = env!("CARGO_PKG_VERSION");
            sections.daemon.push(match &version {
                Some(version) if version != cli => Item::warn(
                    "Daemon",
                    format!("the running daemon is Axocoatl {version}; this CLI is {cli}"),
                    Some(
                        "Restart it so it runs this version: `axocoatl service stop && axocoatl \
                         service start`, or stop `axocoatl dev` and start it again"
                            .to_string(),
                    ),
                ),
                Some(version) => Item::ok("Daemon", format!("running (Axocoatl {version})")),
                None => Item::ok("Daemon", "running (IPC reachable)"),
            });
            let check = match client
                .request(&axocoatl_daemon::ipc::IpcRequest::RuntimeVolumes)
                .await
            {
                Ok(axocoatl_daemon::ipc::IpcResponse::RuntimeVolumes { check }) => Some(check),
                _ => None,
            };
            sections
                .daemon
                .push(match crate::runtime_volume_doctor_line(check.as_ref()) {
                    Ok(line) => Item::ok("Session volumes", line),
                    Err((line, hint)) => Item::warn("Session volumes", line, Some(hint)),
                });
        }
        Err(_) => {
            sections.daemon.push(Item::warn(
                "Daemon",
                "no daemon running; `axocoatl run` and the workbench need one",
                Some("Start it with `axocoatl dev` (or `axocoatl service start`)".to_string()),
            ));
            // Without a configuration there is no port to ask about.
            if config.is_some()
                && is_loopback(&server_host)
                && !crate::onboard::host::port_free(port)
            {
                sections.daemon.push(Item::warn(
                    "Port",
                    format!(
                        "port {port} is in use by another program, so `axocoatl dev` would not \
                         start on it"
                    ),
                    Some(format!(
                        "Choose another port: `axocoatl onboard --update --port {}`, or set \
                         server.port in {}",
                        crate::onboard::host::first_free_port(
                            port.saturating_add(1),
                            crate::onboard::host::port_free
                        ),
                        config_path.display()
                    )),
                ));
            }
        }
    }
    if let Some(program) = installed_service_program(&env.home, env.os.is_macos()) {
        let current = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .ok();
        sections
            .daemon
            .push(if current.as_deref() == Some(program.as_path()) {
                Item::ok(
                    "Always-On Service",
                    format!("installed; runs {}", program.display()),
                )
            } else {
                Item::warn(
                    "Always-On Service",
                    format!(
                        "installed; runs {}, not this axocoatl ({})",
                        program.display(),
                        current
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "unknown".into())
                    ),
                    Some(
                        "Install it again from this axocoatl: `axocoatl service install`, then \
                     `axocoatl service stop && axocoatl service start`"
                            .to_string(),
                    ),
                )
            });
    }
    if let Some(config) = &config {
        if config.webhooks.is_empty() {
            sections
                .daemon
                .push(Item::ok("Webhooks", "none (no outbound event egress)"));
        } else {
            let names: Vec<&str> = config
                .webhooks
                .iter()
                .filter(|webhook| webhook.enabled)
                .map(|webhook| webhook.name.as_str())
                .collect();
            sections.daemon.push(Item::ok(
                "Webhooks",
                format!(
                    "{} active; Skill events leave the box to: {}",
                    names.len(),
                    names.join(", ")
                ),
            ));
        }
    }

    // ── What is ready ──
    let model_ok = |spec: &str| -> Result<(), String> {
        let model = ModelRef::parse(spec)?;
        match model.provider {
            Provider::Ollama => match model_states.get(&model.model) {
                Some(state) if state.ok => Ok(()),
                Some(state) => Err(format!("{spec}: {}", state.why)),
                None => Err(format!("{spec}: Ollama is not configured")),
            },
            Provider::OpenRouter if openrouter_ready => Ok(()),
            Provider::OpenRouter => Err(format!("{spec}: OpenRouter is not configured")),
        }
    };
    let sandbox_blocker = (!sandbox_ok).then(|| "the sandbox is not ready".to_string());
    let mut ready_for = Vec::new();
    let reviewer = run.reviewer.as_deref();
    let mut path = |command: &str, problems: Vec<String>, detail: String| {
        ready_for.push(RunPath {
            command: command.to_string(),
            ok: problems.is_empty(),
            detail: if problems.is_empty() {
                detail
            } else {
                problems.join("; ")
            },
        });
    };
    let reviewer_problem = || -> Option<String> {
        match reviewer {
            None => Some(
                "no reviewer model: run `axocoatl onboard`, or pass --model \
                 reviewer=PROVIDER:MODEL"
                    .to_string(),
            ),
            Some(spec) => model_ok(spec).err(),
        }
    };
    let native_writer =
        run.writer.is_some() || matches!(chosen_writer.as_deref(), Some("ollama" | "openrouter"));
    if native_writer {
        let mut problems: Vec<String> = sandbox_blocker.iter().cloned().collect();
        match run.writer.as_deref() {
            None => problems.push(
                "no writer model: pass --model writer=PROVIDER:MODEL, or run `axocoatl onboard \
                 --update --writer ollama`"
                    .to_string(),
            ),
            Some(spec) => problems.extend(model_ok(spec).err()),
        }
        problems.extend(reviewer_problem());
        path(
            "axocoatl run fix",
            problems,
            format!(
                "writer {}, reviewer {}",
                run.writer.as_deref().unwrap_or("-"),
                reviewer.unwrap_or("-")
            ),
        );
    }
    let codex_command = match &codex_credential {
        Some(credential) => format!("axocoatl run fix-codex --param credential={credential}"),
        None => "axocoatl run fix-codex".to_string(),
    };
    // Claude Code and Codex are listed even when not set up, with the
    // command that sets them up.
    for (command, writer) in [
        ("axocoatl run fix-claude-code", &claude_code),
        (codex_command.as_str(), &codex),
    ] {
        let mut problems = Vec::new();
        if writer.set_up_started || writer.chosen {
            problems.extend(sandbox_blocker.iter().cloned());
            problems.extend(writer.missing.clone());
            problems.extend(reviewer_problem());
        } else {
            problems.extend(writer.missing.clone());
        }
        path(
            command,
            problems,
            format!("reviewer {}", reviewer.unwrap_or("-")),
        );
    }
    if let Some(config) = &config {
        if let Some(lead) = config.agents.iter().find(|agent| agent.id == "lead") {
            let spec = format!("{}:{}", lead.provider, lead.model);
            let mut problems: Vec<String> = if e2b {
                Vec::new()
            } else {
                sandbox_blocker.iter().cloned().collect()
            };
            if lead.provider == "ollama" || lead.provider == "openrouter" {
                problems.extend(model_ok(&spec).err());
            }
            path("workbench Sessions", problems, format!("Lead {spec}"));
        }
    }

    let steps_left: Vec<String> = sections
        .all()
        .filter(|item| item.status == Status::Fail)
        .map(Item::step)
        .collect();
    Report {
        ready: steps_left.is_empty(),
        config: config_path.display().to_string(),
        sections,
        ready_for,
        steps_left,
    }
}

/// Load the configuration the way the daemon does, selecting its data
/// directory unless the environment names one.
async fn load(
    config_path: &Path,
    env: &Environment,
) -> Result<axocoatl_config::AxocoatlConfig, String> {
    if env.data_dir.is_some() {
        axocoatl_config::load_config(config_path)
            .await
            .map_err(|error| error.to_string())
    } else {
        crate::load_cli_config(config_path).await
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// What `GET /health` of the daemon on `host:port` says its version is.
pub(crate) async fn daemon_version(host: &str, port: u16) -> Option<String> {
    let url = crate::onboard::smoke::health_url(host, port);
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?;
    let health: serde_json::Value = response.json().await.ok()?;
    health
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// The program the installed Always-On Service runs, read from its
/// definition (the LaunchAgent plist or the systemd user unit).
pub(crate) fn installed_service_program(home: &Path, macos: bool) -> Option<PathBuf> {
    if macos {
        let plist =
            std::fs::read_to_string(home.join("Library/LaunchAgents/ai.axocoatl.daemon.plist"))
                .ok()?;
        let after = plist.split_once("<key>ProgramArguments</key>")?.1;
        let start = after.find("<string>")? + "<string>".len();
        let end = after[start..].find("</string>")?;
        Some(PathBuf::from(
            after[start..start + end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        ))
    } else {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let unit =
            std::fs::read_to_string(config_home.join("systemd/user/axocoatl.service")).ok()?;
        let line = unit
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))?;
        let program = if let Some(quoted) = line.strip_prefix('"') {
            quoted.split('"').next()?.to_string()
        } else {
            line.split_whitespace().next()?.to_string()
        };
        Some(PathBuf::from(program))
    }
}

/// An external writer: Claude Code or Codex.
struct WriterPath<'a> {
    name: &'a str,
    recipe: &'a str,
    /// The credentials it runs with (any one of them).
    credentials: &'a [&'a str],
    connect: &'a str,
    /// The configuration says this is the writer (onboarding chose it), so
    /// what it lacks fails instead of warning.
    chosen: bool,
}

/// What a writer path has and lacks.
struct WriterState {
    /// Something of it is set up (an image or a credential), so doctor
    /// reports it.
    set_up_started: bool,
    chosen: bool,
    missing: Vec<String>,
}

fn writer_items(
    path: &WriterPath,
    image: bool,
    stored: &BTreeSet<String>,
    items: &mut Vec<Item>,
) -> WriterState {
    let WriterPath {
        name,
        recipe,
        credentials,
        connect,
        chosen,
    } = *path;
    let credential = credentials
        .iter()
        .find(|credential| stored.contains(**credential));
    let started = image || credential.is_some();
    let mut missing = Vec::new();
    if !started && !chosen {
        items.push(Item::ok(
            name,
            format!("not set up (`axocoatl onboard --update --writer {recipe}` sets it up)"),
        ));
        return WriterState {
            set_up_started: false,
            chosen,
            missing: vec![format!(
                "not set up: run `axocoatl onboard --update --writer {recipe}`"
            )],
        };
    }
    let lacking = |detail: String, fix: String| {
        if chosen {
            Item::fail(name, detail, fix)
        } else {
            Item::warn(name, detail, Some(fix))
        }
    };
    items.push(if image {
        Item::ok(name, format!("{recipe} image built"))
    } else {
        missing.push(format!(
            "its image is not built: run `axocoatl recipe build {recipe}`"
        ));
        lacking(
            format!("{recipe} image not built"),
            format!("axocoatl recipe build {recipe}"),
        )
    });
    items.push(match credential {
        Some(credential) => Item::ok(name, format!("signed in ({credential} stored)")),
        None => {
            missing.push(format!("not connected: run `{connect}`"));
            lacking(
                match credentials {
                    [only] => format!("not connected ({only} is not stored)"),
                    _ => "not connected".to_string(),
                },
                connect.to_string(),
            )
        }
    });
    WriterState {
        set_up_started: true,
        chosen,
        missing,
    }
}

/// The Ollama server and each model; returns whether the server is ready.
async fn check_ollama(
    base_url: &str,
    host: &OllamaHost,
    wanted: &BTreeMap<String, bool>,
    env: &Environment,
    items: &mut Vec<Item>,
    states: &mut BTreeMap<String, ModelState>,
) -> bool {
    let installed = match ollama_setup::installed_models(base_url).await {
        Ok(models) => models,
        Err(error) => {
            items.push(Item::fail(
                "Ollama",
                format!("Ollama not reachable at {base_url} ({error})"),
                format!("Start it: {}", host.start()),
            ));
            return false;
        }
    };
    let check = ollama_setup::native_check(base_url).await;
    let mut ready = true;
    for finding in ollama_setup::native_findings(base_url, &check, crate::DOCTOR_OTHER_OLLAMA, host)
    {
        let fix = (!finding.hint.is_empty()).then(|| finding.hint.join(" "));
        items.push(if !finding.ok {
            ready = false;
            Item {
                name: "Ollama".into(),
                status: Status::Fail,
                detail: finding.label,
                fix,
            }
        } else if finding.warn {
            Item::warn("Ollama", finding.label, fix)
        } else {
            Item::ok("Ollama", finding.label)
        });
    }
    if !ready {
        return false;
    }
    for (model, needs_tools) in wanted {
        let name = format!("Model {model}");
        let size = installed
            .iter()
            .find(|candidate| candidate.name() == model)
            .map(|candidate| candidate.size_bytes());
        if !ollama_setup::is_installed(model, &installed) {
            let size = ollama_setup::known_size(model)
                .map(|size| format!(" ({})", ollama_setup::size_text(size)))
                .unwrap_or_default();
            // Another tag of the same model does not count (Ollama reads an
            // untagged name as `:latest`): the hint names it exactly.
            items.push(Item::fail(
                &name,
                format!("{model} is not pulled"),
                ollama_setup::not_pulled_hint(
                    model,
                    &format!("ollama pull {model}{size}"),
                    &installed,
                ),
            ));
            states.insert(
                model.clone(),
                ModelState {
                    ok: false,
                    why: "not pulled".into(),
                },
            );
            continue;
        }
        let finding = ollama_setup::model_finding(base_url, model, *needs_tools).await;
        if !finding.ok {
            items.push(Item::fail(
                &name,
                finding.label.clone(),
                finding.hint.join(" "),
            ));
            states.insert(
                model.clone(),
                ModelState {
                    ok: false,
                    why: finding.label,
                },
            );
            continue;
        }
        let mut parts = vec!["GGUF".to_string()];
        let mut status = Status::Ok;
        let mut fix = None;
        if let (Some(size), Some(memory)) = (size.filter(|size| *size > 0), env.memory_bytes) {
            if ollama_setup::fits_memory(size, memory) {
                parts.push(format!("{}, fits in memory", ollama_setup::size_text(size)));
            } else {
                status = Status::Warn;
                parts.push(format!(
                    "{}, too large for this computer's {} GB",
                    ollama_setup::size_text(size),
                    axocoatl_isolation::host::memory_gb(memory)
                ));
                fix = Some(
                    "Choose a smaller model (`axocoatl onboard --update`), or use OpenRouter for \
                     it"
                    .to_string(),
                );
            }
        }
        let mut model_ok = true;
        let mut why = String::new();
        if env.load_models {
            eprintln!("  (loading {model} once to read its context…)");
            match ollama_setup::observed_context(base_url, model).await {
                Ok((context, _)) if context < ollama_setup::MIN_CONTEXT_TOKENS => {
                    status = Status::Fail;
                    model_ok = false;
                    why = format!(
                        "loaded with a {context}-token context; a fix run needs at least {}",
                        ollama_setup::MIN_CONTEXT_TOKENS
                    );
                    parts.push(why.clone());
                    fix = Some(format!(
                        "Give Ollama a {}-token context: {}; or use OpenRouter for this model",
                        ollama_setup::CONTEXT_FIX_TOKENS,
                        host.context()
                    ));
                }
                Ok((context, _)) => {
                    parts.push(format!("{context}-token context, contract probes passed"));
                    if let Some((loaded, vram)) = ollama_setup::loaded_memory(base_url, model).await
                    {
                        parts.push(ollama_setup::loaded_memory_text(loaded, vram));
                    }
                }
                Err(error) => {
                    status = Status::Fail;
                    model_ok = false;
                    why = error.clone();
                    parts.push(format!("its context could not be read: {error}"));
                    fix = Some(
                        "Check the model with `ollama run`, or choose another (`axocoatl onboard \
                         --update`)"
                            .to_string(),
                    );
                }
            }
        }
        items.push(Item {
            name: name.clone(),
            status,
            detail: format!("{model}: {}", parts.join("; ")),
            fix,
        });
        states.insert(model.clone(), ModelState { ok: model_ok, why });
    }
    true
}

/// The report as `doctor` prints it.
pub(crate) fn text(report: &Report) -> String {
    let mut out = format!("Axocoatl doctor (config: {})\n", report.config);
    for (title, items) in [
        ("Sandbox", &report.sections.sandbox),
        ("Models", &report.sections.models),
        ("Writers", &report.sections.writers),
        ("Daemon", &report.sections.daemon),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title}\n"));
        for item in items {
            let tag = match item.status {
                Status::Ok => "[ OK ]",
                Status::Warn => "[WARN]",
                Status::Fail => "[FAIL]",
            };
            out.push_str(&format!("  {tag} {}\n", item.line()));
            if let Some(fix) = &item.fix {
                if item.status != Status::Ok {
                    out.push_str(&format!("         → {fix}\n"));
                }
            }
        }
    }
    out.push('\n');
    out.push_str(&summary(report));
    out
}

/// "Ready for" or "Not ready yet", then `Ready: yes|no` as the last line.
pub(crate) fn summary(report: &Report) -> String {
    let mut out = String::new();
    let ready: Vec<&RunPath> = report.ready_for.iter().filter(|path| path.ok).collect();
    if report.ready && !ready.is_empty() {
        out.push_str(&format!(
            "Ready for: {}\n",
            ready
                .iter()
                .map(|path| path.command.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        for path in &report.ready_for {
            out.push_str(&format!(
                "  {} {} ({})\n",
                if path.ok { "✓" } else { "✗" },
                path.command,
                path.detail
            ));
        }
    } else if !report.ready {
        let count = report.steps_left.len();
        out.push_str(&format!(
            "Not ready yet, {count} step{} left:\n",
            if count == 1 { "" } else { "s" }
        ));
        for (index, step) in report.steps_left.iter().enumerate() {
            out.push_str(&format!("  {}) {step}\n", index + 1));
        }
    } else {
        out.push_str("Nothing to run yet: no run defaults and no workbench Lead are configured.\n");
    }
    out.push_str(&format!(
        "Ready: {}\n",
        if report.ready { "yes" } else { "no" }
    ));
    out
}

/// `axocoatl doctor [--json]`: exit 0 when nothing failed, 1 otherwise.
pub(crate) async fn cmd_doctor(config_path: &Path, json: bool) -> i32 {
    let env = match Environment::current().await {
        Ok(env) => env,
        Err(error) => {
            eprintln!("✗ {error}");
            return 1;
        }
    };
    let report = collect(config_path, &env).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("the report serializes")
        );
    } else {
        print!("{}", text(&report));
    }
    if report.ready {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, status: Status) -> Item {
        Item {
            name: name.into(),
            status,
            detail: format!("{name} detail"),
            fix: (status != Status::Ok).then(|| format!("fix {name}")),
        }
    }

    fn report(fail: bool) -> Report {
        let mut sections = Sections::default();
        sections
            .sandbox
            .push(item("Podman", if fail { Status::Fail } else { Status::Ok }));
        sections.models.push(item("Ollama", Status::Warn));
        Report {
            ready: !fail,
            config: "/c.yaml".into(),
            steps_left: if fail {
                vec!["Podman detail: fix Podman".into()]
            } else {
                Vec::new()
            },
            sections,
            ready_for: vec![
                RunPath {
                    command: "axocoatl run fix".into(),
                    ok: !fail,
                    detail: "writer ollama:qwen3-coder:30b, reviewer ollama:gpt-oss:20b".into(),
                },
                RunPath {
                    command: "axocoatl run fix-codex".into(),
                    ok: false,
                    detail: "not connected: run `axocoatl connect codex`".into(),
                },
            ],
        }
    }

    /// The JSON contract other tools read: four sections, items with
    /// name/status/detail/fix, and `ready`.
    #[test]
    fn the_json_report_has_the_documented_shape() {
        let value = serde_json::to_value(report(true)).unwrap();
        assert_eq!(value["ready"], false);
        for section in ["sandbox", "models", "writers", "daemon"] {
            assert!(value["sections"][section].is_array(), "{section}");
        }
        let podman = &value["sections"]["sandbox"][0];
        assert_eq!(podman["name"], "Podman");
        assert_eq!(podman["status"], "fail");
        assert_eq!(podman["fix"], "fix Podman");
        assert_eq!(value["sections"]["models"][0]["status"], "warn");
        assert_eq!(value["steps_left"][0], "Podman detail: fix Podman");
        assert_eq!(value["ready_for"][0]["command"], "axocoatl run fix");
    }

    /// Never a ✓ banner after a failure: the summary lists the steps left
    /// and ends with `Ready: no`; a ready report names what it is ready for.
    #[test]
    fn the_summary_says_ready_only_without_failures() {
        let not_ready = text(&report(true));
        assert!(not_ready.contains("[FAIL] Podman detail\n         → fix Podman"));
        assert!(not_ready.contains("Not ready yet, 1 step left:\n  1) Podman detail: fix Podman"));
        assert!(!not_ready.contains("Ready for"));
        assert!(not_ready.ends_with("Ready: no\n"));
        let ready = text(&report(false));
        assert!(ready.contains("Ready for: axocoatl run fix\n"));
        assert!(ready.contains(
            "  ✓ axocoatl run fix (writer ollama:qwen3-coder:30b, reviewer ollama:gpt-oss:20b)"
        ));
        assert!(ready
            .contains("  ✗ axocoatl run fix-codex (not connected: run `axocoatl connect codex`)"));
        assert!(ready.ends_with("Ready: yes\n"));
    }

    /// A line names what it is about once: the item's name only when the
    /// detail does not start with it.
    #[test]
    fn a_line_names_its_subject_once() {
        let line = |name: &str, detail: &str| Item::ok(name, detail).line();
        assert_eq!(
            line(
                "Codex",
                "not set up (`axocoatl onboard --update --writer codex` sets it up)"
            ),
            "Codex: not set up (`axocoatl onboard --update --writer codex` sets it up)"
        );
        assert_eq!(line("Landlock", "Landlock ABI 6"), "Landlock ABI 6");
        assert_eq!(
            line("Model gpt-oss:20b", "gpt-oss:20b: GGUF"),
            "gpt-oss:20b: GGUF"
        );
        assert_eq!(
            line(
                "Model qwen3-coder:30b",
                "Model 'qwen3-coder:30b' cannot run in native Sessions"
            ),
            "Model 'qwen3-coder:30b' cannot run in native Sessions"
        );
        assert_eq!(
            line("Daemon", "running (Axocoatl 1.5.0)"),
            "Daemon: running (Axocoatl 1.5.0)"
        );
    }

    #[test]
    fn landlock_abi_decides_what_runs() {
        let debian = detect::OsRelease::parse("ID=debian\nVERSION_ID=12\n");
        let old = landlock_item(Some(2), Some(&debian), false);
        assert_eq!(old.status, Status::Fail);
        assert!(old.detail.contains("Linux 6.2"));
        assert!(old.fix.unwrap().contains("bookworm-backports"));
        let wsl = landlock_item(Some(1), None, true);
        assert!(wsl.fix.unwrap().contains("wsl --update"));
        assert_eq!(landlock_item(None, None, false).status, Status::Fail);
        let three = landlock_item(Some(3), None, false);
        assert_eq!(three.status, Status::Warn);
        assert!(three.detail.contains("6.7"));
        assert_eq!(landlock_item(Some(6), None, false).status, Status::Ok);
    }

    #[test]
    fn podman_install_fixes_follow_the_platform() {
        let intel_mac = Os::MacOs {
            version: None,
            apple_silicon: false,
            chip: None,
        };
        for homebrew in [true, false] {
            let intel = podman_install_fix(&intel_mac, homebrew);
            assert!(intel.contains("Podman 6 no longer supports Intel Macs"));
            assert!(!intel.contains("brew install"));
        }
        let mac = Os::MacOs {
            version: None,
            apple_silicon: true,
            chip: None,
        };
        let arm = podman_install_fix(&mac, true);
        assert!(arm.contains("it installs Podman after asking"), "{arm}");
        assert!(arm.contains("brew install podman"));
        // Without Homebrew onboarding cannot install Podman: the installer.
        let no_brew = podman_install_fix(&mac, false);
        assert!(no_brew.starts_with("Install Podman with the installer from https://podman.io"));
        assert!(
            no_brew.ends_with("then run `axocoatl onboard`"),
            "{no_brew}"
        );
        assert!(!no_brew.contains("it installs Podman"), "{no_brew}");
        let ubuntu = Os::Linux {
            release: detect::OsRelease::parse("ID=ubuntu\nID_LIKE=debian\n"),
            wsl: None,
            systemd: true,
        };
        for homebrew in [true, false] {
            let fix = podman_install_fix(&ubuntu, homebrew);
            assert!(fix.ends_with("sudo apt-get install -y podman"), "{fix}");
            assert!(!fix.contains("brew"), "{fix}");
        }
    }

    #[test]
    fn the_installed_service_program_is_read_from_its_definition() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(installed_service_program(home.path(), true), None);
        let agents = home.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("ai.axocoatl.daemon.plist"),
            "<dict><key>ProgramArguments</key>\n  <array>\n    <string>/opt/a &amp; b/axocoatl</string>\n    <string>serve</string>",
        )
        .unwrap();
        assert_eq!(
            installed_service_program(home.path(), true),
            Some(PathBuf::from("/opt/a & b/axocoatl"))
        );
    }

    fn writer(image: bool, stored: &[&str], chosen: bool) -> (WriterState, Vec<Item>) {
        let mut items = Vec::new();
        let stored: BTreeSet<String> = stored.iter().map(|s| s.to_string()).collect();
        let state = writer_items(
            &WriterPath {
                name: "Codex",
                recipe: "codex",
                credentials: &["codex-chatgpt", "codex-openai"],
                connect: "axocoatl connect codex",
                chosen,
            },
            image,
            &stored,
            &mut items,
        );
        (state, items)
    }

    #[test]
    fn a_writer_path_is_reported_once_part_of_it_is_set_up() {
        let (state, items) = writer(false, &[], false);
        assert!(!state.set_up_started);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, Status::Ok);
        assert_eq!(
            items[0].line(),
            "Codex: not set up (`axocoatl onboard --update --writer codex` sets it up)"
        );
        let (state, items) = writer(true, &[], false);
        assert!(state.set_up_started);
        assert_eq!(
            state.missing,
            ["not connected: run `axocoatl connect codex`"]
        );
        assert_eq!(items[1].status, Status::Warn);
        let (state, _) = writer(true, &["codex-openai"], false);
        assert!(state.missing.is_empty());
    }

    /// The writer the configuration names fails when its image or its
    /// sign-in is missing: doctor never says ready for a Claude Code or
    /// Codex setup that cannot run.
    #[test]
    fn the_chosen_writer_fails_without_its_image_or_sign_in() {
        let (state, items) = writer(false, &[], true);
        assert!(state.chosen);
        assert_eq!(
            items.iter().map(|item| item.status).collect::<Vec<_>>(),
            [Status::Fail, Status::Fail]
        );
        assert_eq!(items[0].fix.as_deref(), Some("axocoatl recipe build codex"));
        assert_eq!(items[1].fix.as_deref(), Some("axocoatl connect codex"));
        let (_, items) = writer(true, &["codex-chatgpt"], true);
        assert!(items.iter().all(|item| item.status == Status::Ok));
    }
}
