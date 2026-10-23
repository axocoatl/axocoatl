//! What run admission refuses, and how it says so. Owner: workstream
//! `admission`.
//!
//! Admission checks a run in one pass before it creates anything: the
//! loadout and task, the parameters (with the `run.models` defaults), the
//! repository's check, each external writer's stored credential, each
//! native model's provider and, on Ollama, the server and model (reachable,
//! an accepted version, cloud models off, pulled, GGUF, loaded with enough
//! context), the budgets, the network and the recipe image. It collects
//! every problem and reports them together, usage problems (exit code 3)
//! before infrastructure ones (5). Podman is asked last, and only when no
//! usage problem stands; a run starts a stopped Podman machine only when
//! no other problem was found, so a run that cannot start never starts a
//! Podman machine. Nothing — no Workspace, Session, volume, run record or
//! receipt — exists for a refused run.

use axocoatl_config::loadout::{ModelSpec, MIN_RUN_CONTEXT_TOKENS};
use axocoatl_llm::ProviderError;
use axocoatl_llm_ollama::{NativeOllamaServerCheck, OllamaCloudMode};
use axocoatl_session::run_outcome::exit_code;

use super::api::RunProblem;
use super::RunError;
use crate::DaemonError;

/// Every problem admission found with one run, in the order it found them.
#[derive(Debug, Default, Clone)]
pub struct Problems {
    list: Vec<RunProblem>,
    /// The loadout does not exist (the API answers 404 when that is the
    /// only problem).
    not_found: bool,
}

impl Problems {
    fn push(&mut self, code: &str, exit_code: i32, message: String) {
        if self
            .list
            .iter()
            .any(|known| known.code == code && known.message == message)
        {
            return;
        }
        self.list.push(RunProblem {
            code: code.into(),
            exit_code,
            message,
        });
    }

    /// Something in the request or the setup to change (exit code 3).
    pub fn usage(&mut self, code: &str, message: impl Into<String>) {
        self.push(code, exit_code::USAGE, message.into());
    }

    /// The environment cannot run it (exit code 5).
    pub fn infrastructure(&mut self, code: &str, message: impl Into<String>) {
        self.push(code, exit_code::INFRASTRUCTURE, message.into());
    }

    /// A [`RunError`] of an admission step: a usage error is a usage
    /// problem, anything else an infrastructure one.
    pub fn run_error(&mut self, code: &str, error: RunError) {
        match error {
            RunError::Usage(message) => self.usage(code, message),
            other => self.infrastructure(code, other.to_string()),
        }
    }

    /// A [`DaemonError`] of an admission step, by its class: a refused or
    /// unknown request is usage, anything else infrastructure, in its own
    /// words.
    pub fn daemon_error(&mut self, code: &str, error: DaemonError) {
        match error {
            DaemonError::InvalidRequest(message) => self.usage(code, message),
            DaemonError::NotFound(message) => {
                self.not_found = true;
                self.usage(code, message)
            }
            DaemonError::Session(message) => self.infrastructure(code, message),
            other => self.infrastructure(code, other.to_string()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Whether a problem asks the person to change the request or setup.
    pub fn has_usage(&self) -> bool {
        self.list
            .iter()
            .any(|problem| problem.exit_code == exit_code::USAGE)
    }

    /// 0 with no problem, else the exit code of the first one reported.
    pub fn exit_code(&self) -> i32 {
        match (self.list.is_empty(), self.has_usage()) {
            (true, _) => exit_code::PASS,
            (false, true) => exit_code::USAGE,
            (false, false) => exit_code::INFRASTRUCTURE,
        }
    }

    /// The problems as reported: usage problems first, then infrastructure
    /// ones, each in the order found.
    pub fn sorted(&self) -> Vec<RunProblem> {
        let (mut usage, infrastructure): (Vec<_>, Vec<_>) = self
            .list
            .iter()
            .cloned()
            .partition(|problem| problem.exit_code == exit_code::USAGE);
        usage.extend(infrastructure);
        usage
    }

    /// The refusal of a run with these problems: one usage error listing
    /// them all when any asks for a change (404 when the loadout alone is
    /// unknown), else one infrastructure error.
    pub fn refusal(&self) -> DaemonError {
        let sorted = self.sorted();
        let message = refusal_text(&sorted);
        match (self.has_usage(), self.not_found && sorted.len() == 1) {
            (_, true) => DaemonError::NotFound(message),
            (true, false) => DaemonError::InvalidRequest(message),
            (false, false) => DaemonError::Session(message),
        }
    }
}

/// One problem as itself; several as a numbered list that says nothing was
/// started.
pub fn refusal_text(problems: &[RunProblem]) -> String {
    match problems {
        [] => String::new(),
        [only] => only.message.clone(),
        several => {
            let mut text = format!(
                "{} problems stop this run, and nothing was started:",
                several.len()
            );
            for (index, problem) in several.iter().enumerate() {
                text.push_str(&format!("\n  {}. {}", index + 1, problem.message));
            }
            text
        }
    }
}

/// `who` (one caller or several) as one phrase: `Agent writer`, `Agent
/// writer and the reviewer`.
pub fn callers(who: &[String]) -> String {
    axocoatl_config::loadout::and_list(who)
}

/// `who` and the verb: `Agent writer runs`, `Agent writer and the reviewer
/// run`.
pub fn run_by(who: &[String]) -> String {
    format!(
        "{} {}",
        callers(who),
        if who.len() > 1 { "run" } else { "runs" }
    )
}

/// Why `model` cannot run as `who`'s native model: native Agents and
/// reviewers run on Ollama or OpenRouter only.
pub fn unsupported_provider(who: &str, param: Option<&str>, model: &ModelSpec) -> String {
    let flag = match param.and_then(|param| param.strip_suffix("_model")) {
        Some(role) => {
            format!("pass --model {role}=ollama:MODEL or --model {role}=openrouter:VENDOR/MODEL")
        }
        None => "give it an ollama: or openrouter: model".into(),
    };
    let external = matches!(
        model.provider.as_str(),
        "claude-code" | "codex" | "anthropic" | "openai"
    );
    format!(
        "{who} runs {model}, on the provider {}, and a native Agent or reviewer runs only on \
         Ollama or OpenRouter{}: {flag}",
        model.provider,
        if external {
            " (Claude Code and Codex can write, in fix-claude-code and fix-codex, but cannot \
             review)"
        } else {
            ""
        }
    )
}

/// Why `who` cannot use Ollama: the configuration names no Ollama server.
pub fn ollama_not_configured(who: &[String]) -> String {
    format!(
        "{} on Ollama, and this daemon's configuration has no providers.ollama: run `axocoatl \
         onboard`, or add providers.ollama.base_url and restart the daemon",
        run_by(who)
    )
}

/// Why this configuration cannot run a native OpenRouter model, if it
/// cannot: the reasons native OpenRouter's own check gives
/// (`NativeProviderCredentials::openrouter_key`), in setup words.
pub fn openrouter_unready_reason(
    providers: &axocoatl_config::ProvidersConfigYaml,
) -> Option<String> {
    let Some(openrouter) = &providers.openrouter else {
        return Some("this daemon's configuration has no providers.openrouter".into());
    };
    if openrouter.api_key.expose_secret().trim().is_empty() {
        return Some("providers.openrouter has no API key".into());
    }
    if openrouter.fallback.is_some()
        || openrouter
            .base_url
            .as_deref()
            .is_some_and(|base| base.trim_end_matches('/') != "https://openrouter.ai/api/v1")
    {
        return Some(
            "native OpenRouter uses the official endpoint (https://openrouter.ai/api/v1) and no \
             providers.openrouter.fallback"
                .into(),
        );
    }
    if providers.openrouter_billing != Some(axocoatl_config::OpenRouterBilling::Credits) {
        return Some(
            "native OpenRouter runs on OpenRouter credits only, and providers.openrouter_billing \
             is not credits"
                .into(),
        );
    }
    None
}

/// Why `who` cannot run on OpenRouter: `reason`, and how to set it up.
pub fn openrouter_not_ready(who: &[String], reason: &str) -> String {
    let reason = reason.strip_prefix("Session dispatch: ").unwrap_or(reason);
    format!(
        "{} on OpenRouter, and {reason}: run `axocoatl onboard` and choose OpenRouter, or set \
         providers.openrouter (api_key) and providers.openrouter_billing: credits and restart \
         the daemon; or choose an Ollama model",
        run_by(who)
    )
}

/// Why the Ollama at `base_url` cannot be asked at all.
pub fn ollama_unreachable(base_url: &str, error: &ProviderError) -> String {
    match error {
        ProviderError::InvalidRequest { message, .. } => {
            format!("providers.ollama.base_url {base_url} cannot serve native runs: {message}")
        }
        other => format!(
            "the Ollama at {base_url} cannot be reached ({other}): start it (`ollama serve`, \
             `brew services start ollama` or the Ollama app), then run again"
        ),
    }
}

/// What the server check of the Ollama at `base_url` refuses: its version,
/// and its cloud mode.
pub fn ollama_server_problems(base_url: &str, check: &NativeOllamaServerCheck) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(refusal) = check.version_refusal() {
        problems.push(format!("the Ollama at {base_url}: {refusal}"));
    }
    let restart = "then restart Ollama and run again";
    match &check.cloud {
        OllamaCloudMode::Disabled { .. } => {}
        OllamaCloudMode::Enabled => problems.push(format!(
            "the Ollama at {base_url} has its cloud models on, and native runs refuse a server \
             that can send a prompt off this computer: add {{\"disable_ollama_cloud\": true}} \
             to ~/.ollama/server.json of the user Ollama runs as (or start it with \
             OLLAMA_NO_CLOUD=1, or turn Cloud off in the Ollama app's settings), {restart}"
        )),
        OllamaCloudMode::Unreported => problems.push(format!(
            "the Ollama at {base_url} does not report its cloud setting (GET /api/status), so \
             native runs refuse it: they need Ollama {} or a later 0.40 release with cloud \
             models off",
            axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION
        )),
    }
    problems
}

/// Why `model`, which `who` runs, cannot run on the Ollama at `base_url`:
/// Ollama's own reason, with what to do.
pub fn ollama_model_problem(
    who: &[String],
    model: &ModelSpec,
    base_url: &str,
    error: &ProviderError,
) -> String {
    let reason = match error {
        // Native execution's own refusals (not GGUF, an MLX runner, a cloud
        // model, no text completion) are already in words.
        ProviderError::InvalidRequest { message, .. } => {
            let mut reason = message.clone();
            let lower = message.to_ascii_lowercase();
            if lower.contains("gguf") || lower.contains("mlx") || lower.contains("safetensors") {
                reason.push_str(&format!(
                    ". Pull its GGUF variant with `ollama pull --runner llamacpp {}` (an \
                     undocumented Ollama flag), or choose another model",
                    model.model
                ));
            }
            reason
        }
        other => axocoatl_llm_ollama::describe_native_ollama_error(other),
    };
    format!(
        "{} {model}, which the Ollama at {base_url} cannot serve: {reason}",
        run_by(who)
    )
}

/// The context [`context_floor_problem`] asks Ollama to load models with,
/// as `axocoatl onboard` sets it.
pub const CONTEXT_FIX_TOKENS: u64 = 32_768;

/// How the Ollama on this computer runs, as its files show it, which
/// decides where `OLLAMA_CONTEXT_LENGTH` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaInstall {
    /// `brew services` (Homebrew's LaunchAgent for this user).
    Brew,
    /// The Ollama macOS app.
    App,
    /// The systemd unit Ollama's Linux installer creates.
    Systemd,
    /// Something else, such as `ollama serve` in a terminal.
    Unknown,
}

impl OllamaInstall {
    /// The install on this computer: on macOS Homebrew's LaunchAgent in
    /// `home`, else the app; on Linux the systemd unit. `root` is `/`
    /// outside tests.
    pub fn detect(home: Option<&std::path::Path>, root: &std::path::Path, macos: bool) -> Self {
        if macos {
            // Homebrew names the LaunchAgent `sh.brew.ollama` since Homebrew 5,
            // `homebrew.mxcl.ollama` before.
            if home.is_some_and(|home| {
                ["sh.brew.ollama.plist", "homebrew.mxcl.ollama.plist"]
                    .iter()
                    .any(|name| home.join("Library/LaunchAgents").join(name).is_file())
            }) {
                return Self::Brew;
            }
            let app = root.join("Applications/Ollama.app").is_dir()
                || home.is_some_and(|home| home.join("Applications/Ollama.app").is_dir());
            return if app { Self::App } else { Self::Unknown };
        }
        let unit = [
            "etc/systemd/system/ollama.service",
            "usr/lib/systemd/system/ollama.service",
            "lib/systemd/system/ollama.service",
        ]
        .iter()
        .any(|unit| root.join(unit).is_file());
        if unit {
            Self::Systemd
        } else {
            Self::Unknown
        }
    }

    /// The install of the computer this daemon runs on.
    pub fn here() -> Self {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(std::path::PathBuf::from);
        Self::detect(
            home.as_deref(),
            std::path::Path::new("/"),
            cfg!(target_os = "macos"),
        )
    }

    /// How to give this install's Ollama a [`CONTEXT_FIX_TOKENS`] context,
    /// lasting across restarts. `services_env` is Homebrew's service
    /// environment file for Ollama.
    pub fn context_fix(self, services_env: &str) -> String {
        match self {
            Self::Brew => format!(
                "add OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} to {services_env} and run `brew \
                 services restart ollama` (`axocoatl onboard` does this after asking)"
            ),
            Self::App => format!(
                "in the Ollama app's Settings, set the context length to {}k, then quit and \
                 reopen Ollama",
                CONTEXT_FIX_TOKENS / 1024
            ),
            Self::Systemd => format!(
                "add a drop-in with `Environment=\"OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS}\"` \
                 (`sudo systemctl edit ollama`), then `sudo systemctl restart ollama` (`axocoatl \
                 onboard` does this after asking)"
            ),
            Self::Unknown => format!(
                "start Ollama with OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} in its environment \
                 (for example `OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} OLLAMA_NO_CLOUD=1 ollama \
                 serve`), or set it in the service that starts it"
            ),
        }
    }
}

/// Homebrew's service environment file for Ollama:
/// `$XDG_CONFIG_HOME/homebrew/services/ollama.env`, else
/// `~/.homebrew/services/ollama.env`, as `axocoatl onboard` writes it.
pub fn brew_services_env() -> String {
    match std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(config) => config
            .join("homebrew/services/ollama.env")
            .display()
            .to_string(),
        None => "~/.homebrew/services/ollama.env".into(),
    }
}

/// One Ollama model a run loads with too small a context: who runs it, the
/// roles (`writer`, `reviewer`) whose `--model` sets it, the model and the
/// context Ollama loaded it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmallContext {
    pub who: Vec<String>,
    pub roles: Vec<String>,
    pub model: ModelSpec,
    pub context: u64,
}

/// Whether `context` is below what a loadout run needs.
pub fn below_context_floor(context: u64) -> bool {
    context < MIN_RUN_CONTEXT_TOKENS
}

/// Why the run's models that the Ollama at `base_url` loads with too small
/// a context cannot run it: each with the context observed, the context
/// needed, and once for all of them the fix for `install` and the roles to
/// run on OpenRouter instead. `None` when there is none.
pub fn context_floor_problem(
    small: &[SmallContext],
    base_url: &str,
    install: OllamaInstall,
    services_env: &str,
) -> Option<String> {
    let first = small.first()?;
    let runs: Vec<String> = small
        .iter()
        .map(|model| format!("{} {}", run_by(&model.who), model.model))
        .collect();
    let contexts: Vec<u64> = small.iter().map(|model| model.context).collect();
    let loaded = if contexts.iter().all(|context| *context == first.context) {
        format!(
            "loads with a {}-token context{}",
            first.context,
            if small.len() == 1 { "" } else { " each" }
        )
    } else {
        let numbers: Vec<String> = contexts.iter().map(u64::to_string).collect();
        format!(
            "loads with {}-token contexts",
            axocoatl_config::loadout::and_list(&numbers)
        )
    };
    let roles: Vec<String> = small
        .iter()
        .flat_map(|model| model.roles.iter().cloned())
        .fold(Vec::new(), |mut roles, role| {
            if !roles.contains(&role) {
                roles.push(role);
            }
            roles
        });
    let instead = if roles.is_empty() {
        "use an OpenRouter model instead".to_string()
    } else {
        let flags: Vec<String> = roles
            .iter()
            .map(|role| format!("--model {role}=openrouter:VENDOR/MODEL"))
            .collect();
        format!(
            "run the {} on OpenRouter instead ({})",
            axocoatl_config::loadout::and_list(&roles),
            flags.join(" ")
        )
    };
    Some(format!(
        "{}, which the Ollama at {base_url} {loaded}, and a loadout run needs at least \
         {MIN_RUN_CONTEXT_TOKENS} (the fix writer's first call alone is about 9,700 tokens). \
         Give Ollama a {CONTEXT_FIX_TOKENS}-token context: {}, then run again. A larger context \
         uses more memory; on a computer that cannot spare it, {instead}",
        axocoatl_config::loadout::and_list(&runs),
        install.context_fix(services_env)
    ))
}

/// Whether an Ollama error says how the server is doing now (busy, failing
/// to load, not answering), which is infrastructure (exit code 5), rather
/// than refusing the model or the server's setup, which is usage (3).
pub fn ollama_error_is_infrastructure(error: &ProviderError) -> bool {
    match error {
        ProviderError::ApiError { status, .. } => *status >= 500 || *status == 429,
        ProviderError::RateLimited { .. }
        | ProviderError::Network(_)
        | ProviderError::Stream(_)
        | ProviderError::IncompleteStream { .. }
        | ProviderError::Serialization(_) => true,
        _ => false,
    }
}

/// A loadout Session cannot run under Podman that runs as root.
pub fn rootful_podman() -> String {
    let fix = if cfg!(any(target_os = "macos", target_os = "windows")) {
        "switch the machine to rootless (`podman machine stop`, `podman machine set \
         --rootful=false`, `podman machine start`), which affects every tool that uses it"
    } else {
        "run Axocoatl as a normal user, with that user's rootless Podman"
    };
    format!(
        "a loadout Session runs its commands as non-root workload users, which needs rootless \
         Podman; this Podman runs as root: {fix}"
    )
}

/// Podman could not be asked, and could not be made ready.
pub fn podman_not_ready(error: &str) -> String {
    format!(
        "a loadout Session needs rootless Podman, and Podman is not ready: {error}{}",
        id_map_tools_note(error)
    )
}

/// Podman could not be asked whether it runs rootless.
pub fn podman_unanswered(error: &str) -> String {
    format!(
        "a loadout Session needs rootless Podman, and Podman could not be asked: {error}{}",
        id_map_tools_note(error)
    )
}

/// `. ` and what to do when `error` is Podman failing for want of
/// `newuidmap` or `newgidmap` (whose own message says to run `podman system
/// migrate`, which alone changes nothing); else nothing, also when `error`
/// already says it (`ensure_ready`'s message does).
fn id_map_tools_note(error: &str) -> String {
    axocoatl_isolation::podman_report::id_map_tools_hint(error)
        .filter(|hint| !error.contains(hint.as_str()))
        .map(|hint| format!(". {hint}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> ModelSpec {
        ModelSpec::parse(text).unwrap()
    }

    #[test]
    fn usage_problems_come_first_and_one_problem_is_itself() {
        let mut problems = Problems::default();
        assert_eq!(problems.exit_code(), 0);
        problems.infrastructure("recipe_image", "the image is not built");
        assert_eq!(problems.exit_code(), exit_code::INFRASTRUCTURE);
        assert!(matches!(
            problems.refusal(),
            DaemonError::Session(message) if message == "the image is not built"
        ));
        problems.usage("parameters", "a parameter is missing");
        problems.usage("parameters", "a parameter is missing");
        problems.run_error("credential", RunError::Usage("no credential".into()));
        assert_eq!(problems.exit_code(), exit_code::USAGE);
        assert_eq!(
            problems
                .sorted()
                .iter()
                .map(|problem| problem.code.as_str())
                .collect::<Vec<_>>(),
            ["parameters", "credential", "recipe_image"]
        );
        let DaemonError::InvalidRequest(message) = problems.refusal() else {
            panic!("a usage problem makes a usage refusal");
        };
        assert_eq!(
            message,
            "3 problems stop this run, and nothing was started:\n  1. a parameter is \
             missing\n  2. no credential\n  3. the image is not built"
        );
        let mut unknown = Problems::default();
        unknown.daemon_error(
            "loadout",
            DaemonError::NotFound("no loadout named \"x\"".into()),
        );
        assert!(matches!(unknown.refusal(), DaemonError::NotFound(_)));
        unknown.usage("task", "the task is empty");
        assert!(matches!(unknown.refusal(), DaemonError::InvalidRequest(_)));
    }

    #[test]
    fn a_reviewer_on_another_provider_is_told_which_providers_review() {
        assert_eq!(
            unsupported_provider(
                "the reviewer",
                Some("reviewer_model"),
                &spec("anthropic:opus")
            ),
            "the reviewer runs anthropic:opus, on the provider anthropic, and a native Agent or \
             reviewer runs only on Ollama or OpenRouter (Claude Code and Codex can write, in \
             fix-claude-code and fix-codex, but cannot review): pass --model \
             reviewer=ollama:MODEL or --model reviewer=openrouter:VENDOR/MODEL"
        );
        assert!(
            !unsupported_provider("Agent writer", None, &spec("gemini:gemini-3"))
                .contains("Claude Code")
        );
    }

    fn small(who: &str, roles: &[&str], model: &str, context: u64) -> SmallContext {
        SmallContext {
            who: vec![who.into()],
            roles: roles.iter().map(|role| role.to_string()).collect(),
            model: spec(model),
            context,
        }
    }

    #[test]
    fn a_small_context_names_both_numbers_and_ollama_context_length() {
        let url = "http://127.0.0.1:11434";
        let env = "~/.homebrew/services/ollama.env";
        assert!(!below_context_floor(16_384));
        assert!(below_context_floor(4096));
        assert!(context_floor_problem(&[], url, OllamaInstall::Brew, env).is_none());
        let writer = small("Agent writer", &["writer"], "ollama:qwen3-coder:30b", 4096);
        let problem =
            context_floor_problem(std::slice::from_ref(&writer), url, OllamaInstall::Brew, env)
                .unwrap();
        assert_eq!(
            problem,
            "Agent writer runs ollama:qwen3-coder:30b, which the Ollama at \
             http://127.0.0.1:11434 loads with a 4096-token context, and a loadout run needs at \
             least 16384 (the fix writer's first call alone is about 9,700 tokens). Give Ollama \
             a 32768-token context: add OLLAMA_CONTEXT_LENGTH=32768 to \
             ~/.homebrew/services/ollama.env and run `brew services restart ollama` (`axocoatl \
             onboard` does this after asking), then run again. A larger context uses more \
             memory; on a computer that cannot spare it, run the writer on OpenRouter instead \
             (--model writer=openrouter:VENDOR/MODEL)"
        );
        // Two models: one sentence names both, the fix and both roles once.
        let reviewer = small("the reviewer", &["reviewer"], "ollama:gpt-oss:20b", 8192);
        let problem =
            context_floor_problem(&[writer, reviewer], url, OllamaInstall::Systemd, env).unwrap();
        assert!(
            problem.starts_with(
                "Agent writer runs ollama:qwen3-coder:30b and the reviewer runs \
                 ollama:gpt-oss:20b, which the Ollama at http://127.0.0.1:11434 loads with 4096 \
                 and 8192-token contexts"
            ),
            "{problem}"
        );
        assert_eq!(problem.matches("OLLAMA_CONTEXT_LENGTH=32768").count(), 1);
        assert!(problem.contains("sudo systemctl edit ollama"), "{problem}");
        assert!(
            problem.ends_with(
                "run the writer and reviewer on OpenRouter instead (--model \
                 writer=openrouter:VENDOR/MODEL --model reviewer=openrouter:VENDOR/MODEL)"
            ),
            "{problem}"
        );
        for (install, fix) in [
            (
                OllamaInstall::App,
                "in the Ollama app's Settings, set the context length to 32k",
            ),
            (
                OllamaInstall::Unknown,
                "start Ollama with OLLAMA_CONTEXT_LENGTH=32768 in its environment",
            ),
        ] {
            assert!(install.context_fix(env).starts_with(fix), "{install:?}");
        }
    }

    #[test]
    fn the_install_is_read_from_its_files() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let detect = |macos| OllamaInstall::detect(Some(&home), root.path(), macos);
        assert_eq!(detect(true), OllamaInstall::Unknown);
        assert_eq!(detect(false), OllamaInstall::Unknown);
        std::fs::create_dir_all(root.path().join("Applications/Ollama.app")).unwrap();
        assert_eq!(detect(true), OllamaInstall::App);
        std::fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        std::fs::write(
            home.join("Library/LaunchAgents/homebrew.mxcl.ollama.plist"),
            "",
        )
        .unwrap();
        assert_eq!(detect(true), OllamaInstall::Brew);
        std::fs::remove_file(home.join("Library/LaunchAgents/homebrew.mxcl.ollama.plist")).unwrap();
        assert_eq!(detect(true), OllamaInstall::App);
        std::fs::write(home.join("Library/LaunchAgents/sh.brew.ollama.plist"), "").unwrap();
        assert_eq!(detect(true), OllamaInstall::Brew);
        std::fs::create_dir_all(root.path().join("etc/systemd/system")).unwrap();
        std::fs::write(root.path().join("etc/systemd/system/ollama.service"), "").unwrap();
        assert_eq!(detect(false), OllamaInstall::Systemd);
    }

    #[test]
    fn a_busy_or_failing_ollama_is_infrastructure_and_a_refused_model_is_usage() {
        let api = |status| ProviderError::ApiError {
            provider: "ollama".into(),
            status,
            message: "server busy, please try again".into(),
        };
        for error in [
            api(503),
            api(500),
            ProviderError::Network("connection reset".into()),
        ] {
            assert!(ollama_error_is_infrastructure(&error), "{error}");
        }
        for error in [
            api(400),
            ProviderError::ModelNotFound {
                provider: "ollama".into(),
                model: "x".into(),
            },
            ProviderError::InvalidRequest {
                provider: "ollama".into(),
                message: "x is a safetensors model".into(),
            },
        ] {
            assert!(!ollama_error_is_infrastructure(&error), "{error}");
        }
    }

    #[test]
    fn ollama_refusals_keep_ollamas_reason_and_the_fix() {
        let url = "http://127.0.0.1:11434";
        let missing = ProviderError::ModelNotFound {
            provider: "ollama".into(),
            model: "gpt-oss:20b".into(),
        };
        assert_eq!(
            ollama_model_problem(
                &["the reviewer".to_string()],
                &spec("ollama:gpt-oss:20b"),
                url,
                &missing
            ),
            "the reviewer runs ollama:gpt-oss:20b, which the Ollama at http://127.0.0.1:11434 \
             cannot serve: Model not found: gpt-oss:20b on ollama. Ollama does not have \
             gpt-oss:20b: run `ollama pull gpt-oss:20b`"
        );
        let mlx = ProviderError::InvalidRequest {
            provider: "ollama".into(),
            message: "x is a safetensors model".into(),
        };
        assert!(
            ollama_model_problem(&["Agent writer".to_string()], &spec("ollama:x"), url, &mlx)
                .contains("ollama pull --runner llamacpp x")
        );
        let down = ProviderError::Network("connection refused".into());
        assert!(ollama_unreachable(url, &down).contains("ollama serve"));
        let check = NativeOllamaServerCheck {
            version: Some("0.20.6".into()),
            cloud: OllamaCloudMode::Enabled,
        };
        let problems = ollama_server_problems(url, &check);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems[0].contains("Install Ollama 0.40.6 or later"),
            "{problems:?}"
        );
        assert!(problems[1].contains("disable_ollama_cloud"), "{problems:?}");
    }
}
