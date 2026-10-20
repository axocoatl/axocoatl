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
//! before infrastructure ones (5). Only a run with no usage problem asks
//! Podman, so a run that cannot start never starts a Podman machine.
//! Nothing — no Workspace, Session, volume, run record or receipt — exists
//! for a refused run.

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

/// Why `model` cannot run as `who`'s native model: native Agents and
/// reviewers run on Ollama or OpenRouter only.
pub fn unsupported_provider(who: &str, param: Option<&str>, model: &ModelSpec) -> String {
    let flag = match param.and_then(|param| param.strip_suffix("_model")) {
        Some(role) => format!(
            "pass --model {role}=ollama:<model> or --model {role}=openrouter:<vendor>/<model>"
        ),
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
pub fn ollama_not_configured(who: &str) -> String {
    format!(
        "{who} runs on Ollama, and this daemon's configuration has no providers.ollama: run \
         `axocoatl onboard`, or add providers.ollama.base_url and restart the daemon"
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
pub fn openrouter_not_ready(who: &str, reason: &str) -> String {
    let reason = reason.strip_prefix("Session dispatch: ").unwrap_or(reason);
    format!(
        "{who} runs on OpenRouter, and {reason}: run `axocoatl onboard` and choose OpenRouter, \
         or set providers.openrouter (api_key) and providers.openrouter_billing: credits and \
         restart the daemon; or choose an Ollama model"
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
    who: &str,
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
    format!("{who} runs {model}, which the Ollama at {base_url} cannot serve: {reason}")
}

/// How to give Ollama a larger context on this kind of computer.
fn context_length_fix() -> &'static str {
    if cfg!(target_os = "macos") {
        "restart Ollama with OLLAMA_CONTEXT_LENGTH=32768 in its environment: stop it \
         (`brew services stop ollama`, or quit the Ollama app) and run \
         `OLLAMA_CONTEXT_LENGTH=32768 ollama serve`, or raise Context length in the Ollama \
         app's settings"
    } else {
        "restart Ollama with OLLAMA_CONTEXT_LENGTH=32768 in its environment: for the \
         systemd service, add Environment=\"OLLAMA_CONTEXT_LENGTH=32768\" with `sudo \
         systemctl edit ollama` and run `sudo systemctl restart ollama`; otherwise run \
         `OLLAMA_CONTEXT_LENGTH=32768 ollama serve`"
    }
}

/// Why `model`, which `who` runs, is loaded with too small a context for a
/// loadout run: the context observed and the context needed, with the fix.
/// `None` when the context is enough.
pub fn context_floor_problem(
    who: &str,
    model: &ModelSpec,
    base_url: &str,
    context: u64,
) -> Option<String> {
    (context < MIN_RUN_CONTEXT_TOKENS).then(|| {
        format!(
            "{who} runs {model}, which the Ollama at {base_url} loads with a {context}-token \
             context, and a loadout run needs at least {MIN_RUN_CONTEXT_TOKENS} (the fix \
             writer's first call alone is about 9,700 tokens): {}, then run again. A larger \
             context uses more memory; on a computer that cannot spare it, run this role on \
             OpenRouter instead (--model <role>=openrouter:<vendor>/<model>)",
            context_length_fix()
        )
    })
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
    format!("a loadout Session needs rootless Podman, and Podman is not ready: {error}")
}

/// Podman could not be asked whether it runs rootless.
pub fn podman_unanswered(error: &str) -> String {
    format!("a loadout Session needs rootless Podman, and Podman could not be asked: {error}")
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
             reviewer=ollama:<model> or --model reviewer=openrouter:<vendor>/<model>"
        );
        assert!(
            !unsupported_provider("Agent writer", None, &spec("gemini:gemini-3"))
                .contains("Claude Code")
        );
    }

    #[test]
    fn a_small_context_names_both_numbers_and_ollama_context_length() {
        let url = "http://127.0.0.1:11434";
        let model = spec("ollama:qwen3-coder:30b");
        assert!(context_floor_problem("Agent writer", &model, url, 16_384).is_none());
        let problem = context_floor_problem("Agent writer", &model, url, 4096).unwrap();
        assert!(
            problem.starts_with(
                "Agent writer runs ollama:qwen3-coder:30b, which the Ollama at \
                 http://127.0.0.1:11434 loads with a 4096-token context, and a loadout run \
                 needs at least 16384"
            ),
            "{problem}"
        );
        assert!(problem.contains("OLLAMA_CONTEXT_LENGTH=32768"), "{problem}");
        assert!(problem.contains("openrouter"), "{problem}");
    }

    #[test]
    fn ollama_refusals_keep_ollamas_reason_and_the_fix() {
        let url = "http://127.0.0.1:11434";
        let missing = ProviderError::ModelNotFound {
            provider: "ollama".into(),
            model: "gpt-oss:20b".into(),
        };
        assert_eq!(
            ollama_model_problem("the reviewer", &spec("ollama:gpt-oss:20b"), url, &missing),
            "the reviewer runs ollama:gpt-oss:20b, which the Ollama at http://127.0.0.1:11434 \
             cannot serve: Model not found: gpt-oss:20b on ollama. Ollama does not have \
             gpt-oss:20b: run `ollama pull gpt-oss:20b`"
        );
        let mlx = ProviderError::InvalidRequest {
            provider: "ollama".into(),
            message: "x is a safetensors model".into(),
        };
        assert!(
            ollama_model_problem("Agent writer", &spec("ollama:x"), url, &mlx)
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
