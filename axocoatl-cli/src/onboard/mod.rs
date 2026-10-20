//! `axocoatl onboard`: the one setup command a new user runs after the
//! installer. It detects what this computer has (Step 0), asks who writes
//! the code (Step 1), sets up the Podman sandbox (Step 2), the writer and a
//! different reviewer model (Step 3: a local Ollama pair, Claude Code, Codex
//! or OpenRouter), writes the user configuration (Step 4), checks the whole
//! setup with the same code as `axocoatl doctor` (Step 5), starts the daemon
//! if asked (Step 6) and runs a first fix on a sample repository (Step 7).
//!
//! It asks once per change to the machine, showing the exact command and
//! what it downloads or costs; the default is yes only for a change whose
//! size is shown and that fits this computer, and no for anything that
//! changes an existing setup. It ends either ready (exit 0) or with a
//! numbered list of the steps left (exit 1); it never prints a ✓ banner
//! after a failure. Without a terminal it needs `--yes` (exit 3 otherwise);
//! `--plan` prints every action it would take and changes nothing. Every
//! command it runs is written to a log under `~/Library/Logs/Axocoatl` on
//! macOS or `~/.local/state/axocoatl` on Linux.

pub(crate) mod config;
pub(crate) mod detect;
pub(crate) mod host;
mod ollama;
mod openrouter;
mod sandbox;
pub(crate) mod smoke;
pub(crate) mod ui;
mod writers;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, ValueEnum};

use self::config::{ConfigPlan, KeyRef, ModelRef, Provider};
use self::detect::{Facts, Os, Wsl};
use self::host::{Cmd, Host, Log};
use self::ui::{Mode, Ui};
use crate::user_paths::UserPaths;

/// Exit codes of `axocoatl onboard`.
pub(crate) mod exit {
    /// Ready (and the first run passed, when it ran).
    pub(crate) const READY: i32 = 0;
    /// Not ready: the steps left are listed.
    pub(crate) const NOT_READY: i32 = 1;
    /// A command line onboard does not take, or no terminal and no `--yes`.
    pub(crate) const USAGE: i32 = 3;
}

/// Who writes the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WriterChoice {
    /// Claude Code, on your Claude Pro or Max plan
    ClaudeCode,
    /// Codex, on your ChatGPT plan or an OpenAI API key
    Codex,
    /// A local model through Ollama
    Ollama,
    /// A hosted model through OpenRouter
    Openrouter,
}

impl WriterChoice {
    fn label(self, facts: &Facts) -> String {
        match self {
            WriterChoice::ClaudeCode => format!(
                "Claude Code (your Claude Pro or Max plan; {})",
                match &facts.claude {
                    Some((path, _)) => format!("claude found at {}", path.display()),
                    None => "claude not found yet".to_string(),
                }
            ),
            WriterChoice::Codex => "Codex (your ChatGPT plan, or an OpenAI API key)".to_string(),
            WriterChoice::Ollama => {
                "A local model on this computer (Ollama, no account, needs 48 GB memory or more)"
                    .to_string()
            }
            WriterChoice::Openrouter => "A hosted model (OpenRouter credits)".to_string(),
        }
    }

    pub(crate) fn loadout(self) -> &'static str {
        match self {
            WriterChoice::ClaudeCode => "fix-claude-code",
            WriterChoice::Codex => "fix-codex",
            WriterChoice::Ollama | WriterChoice::Openrouter => "fix",
        }
    }
}

/// `--podman`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PodmanChoice {
    /// Install Podman and create and start its machine when they are missing
    Create,
    /// Leave Podman as it is
    Skip,
}

/// `--start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StartChoice {
    /// Run `axocoatl dev` in this terminal at the end
    Dev,
    /// Install and start the Always-On Service (launchd or systemd)
    Service,
    /// Start nothing
    None,
}

/// `axocoatl onboard`.
#[derive(Debug, Clone, Default, Args)]
pub struct OnboardArgs {
    /// Answer yes to every question (sudo also needs --allow-sudo; sign-ins
    /// need --connect and a terminal)
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print what this computer has and every action onboarding would take,
    /// and change nothing
    #[arg(long, conflicts_with = "yes")]
    pub plan: bool,
    /// Who writes the code
    #[arg(long, value_enum)]
    pub writer: Option<WriterChoice>,
    /// The reviewer, a different model that only reads: ollama:MODEL or
    /// openrouter:VENDOR/MODEL
    #[arg(long, value_name = "PROVIDER:MODEL")]
    pub reviewer: Option<String>,
    /// The writer's model: ollama:MODEL or openrouter:VENDOR/MODEL (with
    /// --writer claude-code: anthropic:MODEL; with codex: openai:MODEL)
    #[arg(long, value_name = "PROVIDER:MODEL")]
    pub writer_model: Option<String>,
    /// The Ollama server native Sessions use (default http://localhost:11434)
    #[arg(long, value_name = "URL")]
    pub ollama_url: Option<String>,
    /// create: install Podman and create its machine when missing; skip:
    /// leave Podman as it is
    #[arg(long, value_enum)]
    pub podman: Option<PodmanChoice>,
    /// CPUs for a new Podman machine (default: min(6, half the cores))
    #[arg(long, value_name = "N")]
    pub machine_cpus: Option<u64>,
    /// Memory in MiB for a new Podman machine (default: min(8192, half the
    /// memory))
    #[arg(long, value_name = "MiB")]
    pub machine_memory: Option<u64>,
    /// Pull the chosen Ollama models
    #[arg(long, conflicts_with = "no_pull")]
    pub pull: bool,
    /// Do not pull models (they become steps left)
    #[arg(long)]
    pub no_pull: bool,
    /// Build the Claude Code or Codex sandbox image
    #[arg(long, conflicts_with = "no_recipe_build")]
    pub recipe_build: bool,
    /// Do not build the sandbox image
    #[arg(long)]
    pub no_recipe_build: bool,
    /// Sign in to Claude Code or Codex now (needs a terminal)
    #[arg(long, conflicts_with = "no_connect")]
    pub connect: bool,
    /// Do not sign in now
    #[arg(long)]
    pub no_connect: bool,
    /// Read the OpenRouter key from this environment variable; the
    /// configuration names the variable, not the key
    #[arg(long, value_name = "VAR")]
    pub openrouter_key_env: Option<String>,
    /// The workbench port (default 8080, or the first free port after it)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    /// How to start Axocoatl at the end: dev, service or none
    #[arg(long, value_enum)]
    pub start: Option<StartChoice>,
    /// Run a first fix on a sample repository at the end
    #[arg(long, conflicts_with = "no_smoke_test")]
    pub smoke_test: bool,
    /// Do not run the first fix
    #[arg(long)]
    pub no_smoke_test: bool,
    /// With --yes, also run the steps that need sudo
    #[arg(long)]
    pub allow_sudo: bool,
    /// Keep an existing configuration and change only what is missing
    #[arg(long, conflicts_with = "replace")]
    pub update: bool,
    /// Replace an existing configuration (it is backed up first)
    #[arg(long)]
    pub replace: bool,
    /// Same as --start service (deprecated)
    #[arg(long, hide = true)]
    pub install_daemon: bool,
}

fn tri(yes: bool, no: bool) -> Option<bool> {
    if yes {
        Some(true)
    } else if no {
        Some(false)
    } else {
        None
    }
}

impl OnboardArgs {
    fn pull(&self) -> Option<bool> {
        tri(self.pull, self.no_pull)
    }
    fn recipe_build(&self) -> Option<bool> {
        tri(self.recipe_build, self.no_recipe_build)
    }
    fn connect(&self) -> Option<bool> {
        tri(self.connect, self.no_connect)
    }
    fn smoke_test(&self) -> Option<bool> {
        tri(self.smoke_test, self.no_smoke_test)
    }
    fn start(&self) -> Option<StartChoice> {
        self.start
            .or(self.install_daemon.then_some(StartChoice::Service))
    }

    /// Refuse a command line onboarding cannot follow, before anything runs.
    fn check(&self) -> Result<(), String> {
        if let Some(reviewer) = &self.reviewer {
            ModelRef::parse(reviewer).map_err(|error| format!("--reviewer {error}"))?;
        }
        if let Some(writer) = &self.writer_model {
            match self.writer {
                Some(WriterChoice::ClaudeCode) if !writer.starts_with("anthropic:") => {
                    return Err(format!(
                        "--writer-model {writer:?}: Claude Code runs anthropic:MODEL"
                    ))
                }
                Some(WriterChoice::Codex) if !writer.starts_with("openai:") => {
                    return Err(format!(
                        "--writer-model {writer:?}: Codex runs openai:MODEL"
                    ))
                }
                Some(WriterChoice::ClaudeCode | WriterChoice::Codex) => {}
                _ => {
                    let model = ModelRef::parse(writer)
                        .map_err(|error| format!("--writer-model {error}"))?;
                    if let Some(writer_choice) = self.writer {
                        let expected = match writer_choice {
                            WriterChoice::Ollama => Provider::Ollama,
                            _ => Provider::OpenRouter,
                        };
                        if model.provider != expected {
                            return Err(format!(
                                "--writer-model {writer:?} does not match --writer {}",
                                writer_choice
                                    .to_possible_value()
                                    .map(|value| value.get_name().to_string())
                                    .unwrap_or_default()
                            ));
                        }
                    }
                }
            }
            if self.reviewer.as_deref() == Some(writer.as_str()) {
                return Err(
                    "--reviewer must be a different model from --writer-model: a second model \
                     checks the writer's change"
                        .to_string(),
                );
            }
        }
        if let Some(url) = &self.ollama_url {
            axocoatl_llm_ollama::validate_native_ollama_endpoint(url.trim_end_matches('/'))
                .map_err(|error| {
                    format!(
                        "--ollama-url {url}: {error}; native Sessions use only an Ollama server \
                         on this computer"
                    )
                })?;
        }
        if let Some(variable) = &self.openrouter_key_env {
            let valid = !variable.is_empty()
                && variable
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !variable.starts_with(|c: char| c.is_ascii_digit());
            if !valid {
                return Err(format!(
                    "--openrouter-key-env {variable:?} is not an environment variable name"
                ));
            }
        }
        if self.machine_cpus == Some(0) {
            return Err("--machine-cpus must be at least 1".to_string());
        }
        if self.machine_memory.is_some_and(|memory| memory < 2048) {
            return Err("--machine-memory must be at least 2048 (MiB)".to_string());
        }
        Ok(())
    }
}

/// What onboarding prints when it cannot ask and was not told to answer.
pub(crate) const NO_TERMINAL: &str = "axocoatl onboard asks before every change to this computer, \
and there is no terminal to ask in. Run it in a terminal, or answer with flags:
  axocoatl onboard --yes --writer claude-code|codex|ollama|openrouter
      [--reviewer PROVIDER:MODEL] [--writer-model PROVIDER:MODEL] [--ollama-url URL]
      [--podman create|skip] [--machine-cpus N] [--machine-memory MiB]
      [--pull|--no-pull] [--recipe-build|--no-recipe-build] [--connect|--no-connect]
      [--openrouter-key-env VAR] [--port N] [--start dev|service|none]
      [--smoke-test|--no-smoke-test] [--allow-sudo] [--update|--replace]
  axocoatl onboard --plan    (prints every action it would take; changes nothing)
Nothing was changed.";

/// What an existing configuration becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExistingConfig {
    None,
    Update(String),
    Replace(String),
}

/// Everything onboarding learns and decides as it goes.
pub(crate) struct Onboarding {
    pub(crate) ui: Ui,
    pub(crate) host: Host,
    pub(crate) facts: Facts,
    pub(crate) args: OnboardArgs,
    pub(crate) paths: UserPaths,
    /// What is left for the person to do, each with its exact command.
    pub(crate) steps: Vec<String>,
    /// `https://openrouter.ai/api/v1`, or a test server.
    pub(crate) openrouter_api: String,
    /// The daemon's IPC socket (Step 5 asks whether a daemon runs).
    pub(crate) socket_path: PathBuf,
    /// The data directory, when it is not the one the configuration selects
    /// (tests).
    pub(crate) data_dir_override: Option<PathBuf>,
}

impl Onboarding {
    /// Record a step left (and print it).
    pub(crate) fn step_left(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.ui.fail(&text);
        self.steps.push(text);
    }

    pub(crate) fn memory_gb(&self) -> Option<u64> {
        self.facts.memory_gb()
    }

    /// The data directory this configuration uses.
    pub(crate) fn data_dir(&self) -> PathBuf {
        self.data_dir_override
            .clone()
            .unwrap_or_else(|| self.paths.data_dir.clone())
    }

    /// What Step 5's check (the code `doctor` runs) needs about this
    /// computer.
    fn doctor_env(&self) -> crate::doctor::Environment {
        crate::doctor::Environment {
            os: self.facts.os.clone(),
            user: self.facts.user.clone(),
            memory_bytes: self.facts.memory_bytes,
            cpus: self.facts.cpus,
            home: self.host.home.clone(),
            xdg_config_home: self.facts.xdg_config_home.clone(),
            host: self.host.clone(),
            load_models: true,
            socket_path: self.socket_path.clone(),
            podman: self
                .host
                .find("podman")
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "podman".to_string()),
            data_dir: self.data_dir_override.clone(),
            landlock_abi: self.facts.landlock_abi,
        }
    }
}

/// The setup chosen in Step 3.
#[derive(Debug, Clone, Default)]
pub(crate) struct Choice {
    pub(crate) ollama_base_url: Option<String>,
    pub(crate) openrouter_key: Option<KeyRef>,
    pub(crate) writer_model: Option<ModelRef>,
    pub(crate) reviewer_model: Option<ModelRef>,
    /// A writer model for an external writer (`anthropic:…`, `openai:…`).
    pub(crate) external_writer_model: Option<String>,
    /// The credential a Codex run uses, named on its command line.
    pub(crate) codex_credential: Option<&'static str>,
}

/// `axocoatl onboard`; returns the exit code.
pub(crate) async fn cmd_onboard(args: OnboardArgs) -> i32 {
    if let Err(error) = args.check() {
        eprintln!("✗ {error}");
        return exit::USAGE;
    }
    let mode = if args.plan {
        Mode::Plan
    } else if args.yes {
        Mode::Yes
    } else {
        Mode::Interactive
    };
    if mode == Mode::Interactive && !std::io::stdin().is_terminal() {
        eprintln!("✗ {NO_TERMINAL}");
        return exit::USAGE;
    }
    let paths = match UserPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("✗ Cannot determine the user configuration location: {error}");
            return exit::NOT_READY;
        }
    };
    let log = if mode == Mode::Plan {
        None
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let directory = host::log_directory(
            &home,
            cfg!(target_os = "macos"),
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .as_deref(),
        );
        Log::create(&directory, &host::stamp()).ok().map(Arc::new)
    };
    let host = match Host::from_env(log.clone()) {
        Ok(host) => host,
        Err(error) => {
            eprintln!("✗ {error}");
            return exit::NOT_READY;
        }
    };
    let ui = Ui::new(mode, args.allow_sudo, log.clone());
    ui.say("Axocoatl setup");
    if let Some(log) = &log {
        ui.say(format!(
            "Every command this runs is logged to {}",
            log.path().display()
        ));
    }
    if mode == Mode::Plan {
        ui.say("--plan: nothing is changed; every action is printed instead.");
    }

    let existing_url = std::fs::read_to_string(&paths.config_path)
        .ok()
        .and_then(|text| config::read_existing(&text).ok())
        .and_then(|existing| existing.providers?.ollama?.base_url);
    let ollama_url = args
        .ollama_url
        .clone()
        .or(existing_url)
        .unwrap_or_else(|| crate::ollama_setup::DEFAULT_OLLAMA_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    ui.heading("Step 0 · This computer");
    let facts = detect::detect(&host, &ollama_url).await;
    for line in detect::summary(&facts) {
        ui.say(line);
    }
    let mut onboarding = Onboarding {
        ui,
        host,
        facts,
        args,
        paths,
        steps: Vec::new(),
        openrouter_api: openrouter::OPENROUTER_API.to_string(),
        socket_path: axocoatl_daemon::ipc::default_socket_path(),
        data_dir_override: None,
    };
    run(&mut onboarding).await
}

/// Steps 0 to 7 after detection.
pub(crate) async fn run(o: &mut Onboarding) -> i32 {
    // Stops before anything else.
    if o.facts.root_user {
        o.ui.fail("Run Axocoatl as a normal user; loadout runs need rootless Podman.");
        return exit::NOT_READY;
    }
    if o.facts.os.wsl() == Some(Wsl::One) {
        o.ui.fail(
            "This is WSL 1, which cannot run Podman. In PowerShell run `wsl --set-version <distro> \
             2` (wsl -l -v names the distro), then run axocoatl onboard again.",
        );
        return exit::NOT_READY;
    }
    if o.facts.git.is_none() {
        if o.facts.os.is_macos() {
            let install = Cmd::new("xcode-select", &["--install"]);
            if o.ui.consent(
                "git is missing (macOS asks for the Command Line Tools). Install them with \
                 `xcode-select --install`? This opens Apple's installer.",
                true,
                false,
            ) {
                let _ = o.ui.execute(&o.host, &install).await;
            }
            o.step_left(
                "Finish installing the Command Line Tools (git), then run `axocoatl onboard` again",
            );
        } else {
            let fix = o
                .facts
                .os
                .release()
                .and_then(|release| release.package_manager())
                .map(|manager| manager.install(&["git"]).display())
                .unwrap_or_else(|| "install git".to_string());
            o.step_left(format!("Install git: {fix}"));
        }
    }

    // Existing configuration.
    let existing = match std::fs::read_to_string(&o.paths.config_path) {
        Err(_) => ExistingConfig::None,
        Ok(text) => {
            o.ui.say(format!(
                "\nAn Axocoatl configuration exists: {}",
                o.paths.config_path.display()
            ));
            if o.args.replace {
                ExistingConfig::Replace(text)
            } else if o.args.update || o.ui.mode != Mode::Interactive {
                ExistingConfig::Update(text)
            } else {
                let items = vec![
                    "Update this setup (keep it, back it up, change only what is missing)"
                        .to_string(),
                    "Replace it (it is backed up first)".to_string(),
                    "Leave it unchanged and stop".to_string(),
                ];
                match o.ui.select("What should onboarding do with it?", &items, 0) {
                    0 => ExistingConfig::Update(text),
                    1 => ExistingConfig::Replace(text),
                    _ => {
                        o.ui.say(format!(
                            "Configuration left unchanged: {}. `axocoatl doctor` checks it.",
                            o.paths.config_path.display()
                        ));
                        return exit::READY;
                    }
                }
            }
        }
    };

    // Step 1: who writes.
    o.ui.heading("Step 1 · Who writes the code");
    let writer = choose_writer(o);
    o.ui.say(
        "Every fix run also needs a reviewer: a different model that only reads the change. \
         Claude Code and Codex can write but cannot review.",
    );

    // Step 2: the sandbox.
    o.ui.heading("Step 2 · The sandbox (rootless Podman)");
    let sandbox_ready = sandbox::run(o).await;

    // Step 3: the writer and the reviewer.
    let mut choice = Choice::default();
    match writer {
        WriterChoice::Ollama => {
            o.ui.heading("Step 3 · A local writer and reviewer (Ollama)");
            ollama::writer_and_reviewer(o, &mut choice).await;
        }
        WriterChoice::Openrouter => {
            o.ui.heading("Step 3 · A hosted writer and reviewer (OpenRouter)");
            openrouter::run(o, &mut choice, true).await;
        }
        WriterChoice::ClaudeCode | WriterChoice::Codex => {
            o.ui.heading(if writer == WriterChoice::ClaudeCode {
                "Step 3 · Claude Code"
            } else {
                "Step 3 · Codex"
            });
            writers::run(o, writer, sandbox_ready, &mut choice).await;
        }
    }

    // Step 4: the configuration.
    o.ui.heading("Step 4 · The configuration");
    let Some(plan) = config_plan(o, writer, &choice, &existing) else {
        return finish_not_ready(o);
    };
    if !write_config(o, &plan, &existing) {
        return finish_not_ready(o);
    }

    // Step 5: the same check as `axocoatl doctor`.
    o.ui.heading("Step 5 · Check");
    if o.ui.plan() {
        o.ui.say("  would run the same checks as `axocoatl doctor` on the new configuration");
        o.ui.say("\nPlan only: nothing was changed.");
        return exit::READY;
    }
    let ready = check(o).await;
    if !ready {
        return finish_not_ready(o);
    }

    // Step 6 and 7.
    let start = choose_start(o);
    let mut code = exit::READY;
    if start == StartChoice::Service && !start_service(o).await {
        return finish_not_ready(o);
    }
    if wants_smoke_test(o, writer) {
        o.ui.heading("Step 7 · A first run");
        if !smoke::run(o, writer, &choice).await {
            code = exit::NOT_READY;
        }
    }
    o.ui.say(format!("\n{}", next_command(writer, &choice)));
    if !o.steps.is_empty() {
        return finish_not_ready(o);
    }
    if start == StartChoice::Dev && code == exit::READY {
        o.ui.say("\nStarting `axocoatl dev` (Ctrl+C stops it).");
        crate::cmd_dev(&o.paths.config_path).await;
    }
    code
}

/// Print the steps left and return [`exit::NOT_READY`].
fn finish_not_ready(o: &Onboarding) -> i32 {
    let count = o.steps.len();
    if count == 0 {
        o.ui.say("\nNot ready yet.");
    } else {
        o.ui.say(format!(
            "\nNot ready yet, {count} step{} left:",
            if count == 1 { "" } else { "s" }
        ));
        for (index, step) in o.steps.iter().enumerate() {
            o.ui.say(format!("  {}) {step}", index + 1));
        }
    }
    o.ui.say("Run `axocoatl onboard` again once they are done; it keeps what is already set up.");
    exit::NOT_READY
}

/// Step 1's default: Claude Code when `claude` is installed, else Codex
/// when `codex` is, else a local model with 48 GB or more, else OpenRouter.
pub(crate) fn default_writer(facts: &Facts) -> WriterChoice {
    if facts.claude.is_some() {
        WriterChoice::ClaudeCode
    } else if facts.codex.is_some() {
        WriterChoice::Codex
    } else if facts.memory_gb().is_some_and(|gb| gb >= 48) {
        WriterChoice::Ollama
    } else {
        WriterChoice::Openrouter
    }
}

fn choose_writer(o: &mut Onboarding) -> WriterChoice {
    if let Some(writer) = o.args.writer {
        o.ui.say(format!("  Writer: {} (--writer)", writer.label(&o.facts)));
        return writer;
    }
    let choices = [
        WriterChoice::ClaudeCode,
        WriterChoice::Codex,
        WriterChoice::Ollama,
        WriterChoice::Openrouter,
    ];
    let default = default_writer(&o.facts);
    let items: Vec<String> = choices
        .iter()
        .map(|choice| choice.label(&o.facts))
        .collect();
    let index = o.ui.select(
        "Who writes the code?",
        &items,
        choices
            .iter()
            .position(|choice| *choice == default)
            .unwrap_or(0),
    );
    choices[index]
}

/// The configuration this setup writes, or `None` when nothing usable was
/// chosen (the steps left say why).
fn config_plan(
    o: &mut Onboarding,
    writer: WriterChoice,
    choice: &Choice,
    existing: &ExistingConfig,
) -> Option<ConfigPlan> {
    let reviewer = choice.reviewer_model.clone();
    let Some(reviewer) = reviewer else {
        o.step_left(
            "Choose a reviewer model: run `axocoatl onboard` again, or pass --reviewer \
             ollama:MODEL or --reviewer openrouter:VENDOR/MODEL",
        );
        return None;
    };
    let (lead, run_writer, note) = match writer {
        WriterChoice::Ollama | WriterChoice::Openrouter => {
            let Some(model) = choice.writer_model.clone() else {
                o.step_left(
                    "Choose a writer model: run `axocoatl onboard` again, or pass --writer-model",
                );
                return None;
            };
            let note = if model.provider == Provider::Ollama {
                "local Ollama setup".to_string()
            } else {
                "OpenRouter setup".to_string()
            };
            (model.clone(), Some(model), note)
        }
        WriterChoice::ClaudeCode | WriterChoice::Codex => (
            reviewer.clone(),
            None,
            format!(
                "{} writes in `axocoatl run {}`; workbench Sessions run Lead on the reviewer's \
                 model",
                if writer == WriterChoice::ClaudeCode {
                    "Claude Code"
                } else {
                    "Codex"
                },
                writer.loadout()
            ),
        ),
    };
    let existing_port = match existing {
        ExistingConfig::Update(text) => config::read_existing(text)
            .ok()
            .and_then(|existing| existing.server?.port),
        _ => None,
    };
    let port = match (o.args.port, existing_port) {
        (Some(port), _) => port,
        (None, Some(port)) => port,
        (None, None) => host::first_free_port(8080, host::port_free),
    };
    if o.args.port.is_none() && existing_port.is_none() && port != 8080 {
        o.ui.say(format!(
            "  Port 8080 is in use; the workbench will use port {port} (server.port)."
        ));
    }
    Some(ConfigPlan {
        ollama_base_url: choice.ollama_base_url.clone(),
        openrouter_key: choice.openrouter_key.clone(),
        lead,
        reviewer: reviewer.clone(),
        run_writer,
        run_reviewer: Some(reviewer),
        port,
        note,
    })
}

/// Write the configuration: a new one, the update of an existing one, or
/// its replacement, after showing the change and backing the old file up.
fn write_config(o: &mut Onboarding, plan: &ConfigPlan, existing: &ExistingConfig) -> bool {
    let path = o.paths.config_path.clone();
    let (old, new) = match existing {
        ExistingConfig::None => (None, config::render(plan)),
        ExistingConfig::Replace(old) => (Some(old.clone()), config::render(plan)),
        ExistingConfig::Update(old) => match config::update(old, plan, &path) {
            Ok(new) => (Some(old.clone()), new),
            Err(error) => {
                o.step_left(format!(
                    "{error}: run `axocoatl onboard --replace` (the old file is backed up first)"
                ));
                return false;
            }
        },
    };
    if let Some(old) = &old {
        let changes = config::diff(old, &new);
        if changes.is_empty() {
            o.ui.ok(format!("{} already has this setup", path.display()));
            return true;
        }
        o.ui.say(format!("  Changes to {}:", path.display()));
        for line in redact_diff(&changes) {
            o.ui.say(format!("    {line}"));
        }
        let backup = format!("config.yaml.bak-{}", host::stamp());
        if !o.ui.consent(
            &format!("Write these changes? The current file is kept as {backup} next to it."),
            false,
            false,
        ) {
            o.step_left(format!(
                "The configuration was not changed: run `axocoatl onboard` again to write it ({})",
                path.display()
            ));
            return false;
        }
        if o.ui.plan() {
            return true;
        }
        if let Err(error) = write_backup(&o.paths, &backup, old) {
            o.step_left(format!("Could not back up the configuration: {error}"));
            return false;
        }
        o.ui.ok(format!(
            "Backed up to {}",
            o.paths.config_dir.join(&backup).display()
        ));
    } else if o.ui.plan() {
        o.ui.say(format!("  would write {}:", path.display()));
        for line in redact_diff(&config::diff("", &new)) {
            o.ui.say(format!("    {line}"));
        }
        return true;
    }
    if let Err(error) = crate::write_user_configuration(&o.paths, &new) {
        o.step_left(format!("Could not save the configuration: {error}"));
        return false;
    }
    o.host.log(&format!("wrote {}", path.display()));
    o.ui.ok(format!("Configuration written: {}", path.display()));
    o.ui.ok(format!("Data: {}", o.paths.data_dir.display()));
    true
}

/// A diff with an API key's value hidden.
fn redact_diff(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| match line.split_once("api_key:") {
            Some((before, value)) if !value.contains("${") => {
                format!("{before}api_key: (hidden)")
            }
            _ => line.clone(),
        })
        .collect()
}

fn write_backup(paths: &UserPaths, name: &str, text: &str) -> std::io::Result<()> {
    let directory = axocoatl_core::SecureDir::open_or_create_all(&paths.config_dir)?;
    crate::admit_private_directory(&directory)?;
    directory.atomic_write(name, text.as_bytes())
}

/// Step 5: the same checks as `axocoatl doctor`. Prints what failed and
/// what is ready; adds each failure to the steps left.
async fn check(o: &mut Onboarding) -> bool {
    let env = o.doctor_env();
    if env.data_dir.is_none() {
        if let Err(error) = crate::configure_data_dir(&o.paths.config_path) {
            o.step_left(format!("Cannot select the data directory: {error}"));
            return false;
        }
    }
    let report = crate::doctor::collect(&o.paths.config_path, &env).await;
    for item in report
        .sections
        .sandbox
        .iter()
        .chain(&report.sections.models)
        .chain(&report.sections.writers)
        .chain(&report.sections.daemon)
    {
        match item.status {
            crate::doctor::Status::Ok => {}
            crate::doctor::Status::Warn => o.ui.warn(item.line()),
            crate::doctor::Status::Fail => o.ui.fail(item.line()),
        }
    }
    for step in &report.steps_left {
        if !o.steps.contains(step) {
            o.steps.push(step.clone());
        }
    }
    if report.ready && o.steps.is_empty() {
        for line in crate::doctor::summary(&report).lines() {
            o.ui.say(line);
        }
        true
    } else {
        false
    }
}

fn choose_start(o: &mut Onboarding) -> StartChoice {
    if let Some(start) = o.args.start() {
        return start;
    }
    if o.ui.mode != Mode::Interactive {
        return StartChoice::None;
    }
    o.ui.heading("Step 6 · Start Axocoatl");
    let service = if matches!(o.facts.os, Os::Linux { systemd: false, .. }) {
        "Always on, starting at login (not available: this Linux runs without systemd)"
    } else {
        "Always on, starting at login (a launchd or systemd service)"
    };
    let items = vec![
        "In this terminal now (axocoatl dev)".to_string(),
        service.to_string(),
        "I'll start it later".to_string(),
    ];
    match o.ui.select("How should Axocoatl run?", &items, 0) {
        0 => StartChoice::Dev,
        1 if !service.contains("not available") => StartChoice::Service,
        _ => StartChoice::None,
    }
}

/// Install and start the Always-On Service (asked for with Step 6).
async fn start_service(o: &mut Onboarding) -> bool {
    let config = std::fs::read_to_string(&o.paths.config_path).unwrap_or_default();
    if let Err(error) = crate::reject_uninherited_service_environment(&config) {
        o.step_left(format!("Cannot install the Always-On Service: {error}"));
        return false;
    }
    if !o.ui.consent(
        "Install the Always-On Service (it starts Axocoatl at login and keeps it running)?",
        true,
        false,
    ) {
        return true;
    }
    if o.ui.plan() {
        o.ui.say("    would run: axocoatl service install && axocoatl service start");
        return true;
    }
    let config = o.paths.config_path.display().to_string();
    for args in [
        vec!["service", "install", "--config", &config],
        vec!["service", "start"],
    ] {
        let cmd = Cmd::new(&o.host.exe.display().to_string(), &args);
        if let Err(error) = o.host.run(&cmd).await {
            o.step_left(format!("{error}; see `axocoatl service status`"));
            return false;
        }
    }
    true
}

fn wants_smoke_test(o: &mut Onboarding, writer: WriterChoice) -> bool {
    match o.args.smoke_test() {
        Some(choice) => choice,
        None if o.ui.mode != Mode::Interactive => false,
        None => {
            let cost = match writer {
                WriterChoice::Ollama => "runs locally",
                WriterChoice::ClaudeCode => "uses your Claude plan quota",
                WriterChoice::Codex => "uses your ChatGPT plan or OpenAI credits",
                WriterChoice::Openrouter => "costs about $0.01 on OpenRouter",
            };
            o.ui.consent(
                &format!(
                    "Run a first fix now on a small sample repository to prove the setup end to \
                     end? It {cost}."
                ),
                true,
                false,
            )
        }
    }
}

/// The exact next command, with the user's models.
pub(crate) fn next_command(writer: WriterChoice, choice: &Choice) -> String {
    let mut command = format!(
        "axocoatl run {} --task \"Describe the bug\"",
        writer.loadout()
    );
    if let Some(credential) = choice.codex_credential {
        command.push_str(&format!(" --param credential={credential}"));
    }
    let defaults = match writer {
        WriterChoice::Ollama | WriterChoice::Openrouter => format!(
            "writer {}, reviewer {} come from your setup",
            choice
                .writer_model
                .as_ref()
                .map(ModelRef::spec)
                .unwrap_or_default(),
            choice
                .reviewer_model
                .as_ref()
                .map(ModelRef::spec)
                .unwrap_or_default()
        ),
        WriterChoice::ClaudeCode | WriterChoice::Codex => format!(
            "reviewer {} comes from your setup",
            choice
                .reviewer_model
                .as_ref()
                .map(ModelRef::spec)
                .unwrap_or_default()
        ),
    };
    format!(
        "Next, in your own repository (start Axocoatl first with `axocoatl dev` if it is not \
         running):\n  cd ~/src/my-app\n  {command}\n({defaults}; --model ROLE=PROVIDER:MODEL \
         overrides them)"
    )
}

#[cfg(test)]
mod tests;
