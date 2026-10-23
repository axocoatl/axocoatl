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
//! changes a setup the person did not ask to change (an Ollama
//! `server.json`, a service environment). Step 4 shows the configuration
//! before writing it: what a new file sets (written without a question:
//! the answers before chose it, and nothing existing changes), or the
//! changes to an existing file, asked once with yes as the default, since
//! the person chose to update or replace it. It ends either ready (exit 0)
//! or with a numbered list of the steps left (exit 1), saying whether
//! anything was saved; it never prints a ✓ banner
//! after a failure. Without a terminal it needs `--yes` (exit 3 otherwise);
//! `--plan` prints every action it would take and changes nothing. Every
//! command it runs is written to a log under `~/Library/Logs/Axocoatl` on
//! macOS or `~/.local/state/axocoatl` on Linux.

pub(crate) mod config;
pub(crate) mod detect;
pub(crate) mod host;
mod ollama;
pub(crate) mod openrouter;
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

    /// The name `--writer` takes and the configuration records.
    pub(crate) fn id(self) -> &'static str {
        match self {
            WriterChoice::ClaudeCode => "claude-code",
            WriterChoice::Codex => "codex",
            WriterChoice::Ollama => "ollama",
            WriterChoice::Openrouter => "openrouter",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        [
            WriterChoice::ClaudeCode,
            WriterChoice::Codex,
            WriterChoice::Ollama,
            WriterChoice::Openrouter,
        ]
        .into_iter()
        .find(|writer| writer.id() == id)
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
    /// Set up a missing Podman: on a Mac, install it with Homebrew (Apple
    /// silicon) and create and start its machine; on Linux, install it with
    /// apt-get, dnf, pacman or zypper (with sudo), except on Ubuntu 22.04,
    /// whose Podman 3.4.4 is too old (onboarding refuses it and says to use
    /// Ubuntu 24.04 or newer); elsewhere, say how
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
    /// Answer yes to every question (sudo also needs --allow-sudo; the Claude
    /// Code and Codex sign-ins need --connect and a terminal; sign in to
    /// OpenRouter first with `axocoatl connect openrouter`)
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
    /// create (the default): set up a missing Podman where onboarding can
    /// (Mac: Homebrew on Apple silicon, then the machine; Linux: the
    /// distribution's package manager, with sudo; Ubuntu 22.04 is refused,
    /// its Podman 3.4.4 is too old); skip: leave Podman as it is
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
    /// Do not sign in to Claude Code or Codex now
    #[arg(long)]
    pub no_connect: bool,
    /// Read the OpenRouter key from this environment variable instead of a
    /// browser sign-in; the configuration names the variable, not the key
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
        if let (Some(WriterChoice::Openrouter), Some(reviewer)) = (self.writer, &self.reviewer) {
            if !reviewer.starts_with("openrouter:") {
                return Err(format!(
                    "--reviewer {reviewer:?}: with --writer openrouter the reviewer runs on \
                     OpenRouter too (--reviewer openrouter:VENDOR/MODEL); for a local reviewer \
                     use --writer ollama"
                ));
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
            if !is_variable_name(variable) {
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

/// A letter or `_`, then letters, digits or `_`.
pub(crate) fn is_variable_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// What onboarding prints when it cannot ask and was not told to answer.
/// Every command line in it can be pasted as it is: no `a|b` alternatives
/// (a shell runs them as a pipeline) and no `[…]` or `(…)`.
pub(crate) const NO_TERMINAL: &str = "axocoatl onboard asks before every change to this computer, \
and there is no terminal to ask in. Run it in a terminal, or answer with flags, for example:
  axocoatl onboard --yes --writer WRITER
where WRITER is claude-code, codex, ollama or openrouter. `axocoatl onboard --help` lists the \
flags that answer the other questions (the models, Podman, pulls, sign-ins, how to start, sudo, \
an existing configuration). To print every action onboarding would take, changing nothing:
  axocoatl onboard --plan
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
    /// Step 3D's browser sign-in: `None` for OpenRouter's sign-in page and
    /// API, opened with this computer's browser; a test's otherwise (used
    /// once).
    pub(crate) openrouter_sign_in: Option<(
        crate::connect_cmd::openrouter::Browser,
        crate::connect_cmd::openrouter::Endpoints,
    )>,
    /// What an existing configuration already has, kept by "Update this
    /// setup".
    pub(crate) kept: Kept,
    /// What the steps left so far are about (`sandbox`, `ollama`,
    /// `model:NAME`, `openrouter`, `writer:ID`), so Step 5 does not list
    /// the same problem again from the check.
    pub(crate) step_keys: std::collections::BTreeSet<String>,
}

/// The setup an existing configuration describes, which "Update this
/// setup" keeps unless a flag says otherwise. (No `Debug`: it may hold the
/// configured OpenRouter key.)
#[derive(Clone, Default)]
pub(crate) struct Kept {
    pub(crate) writer: Option<WriterChoice>,
    /// `run.models.writer`.
    pub(crate) writer_model: Option<String>,
    /// `run.models.reviewer`.
    pub(crate) reviewer: Option<String>,
    pub(crate) codex_credential: Option<String>,
    /// `providers.openrouter.api_key` as written: a key, or `${NAME}`.
    pub(crate) openrouter_key: Option<zeroize::Zeroizing<String>>,
    /// `providers.openrouter.api_key_secret`: the secret in Axocoatl's
    /// secret store that holds the key (`axocoatl connect openrouter`, or
    /// onboarding's browser sign-in, stores it). It takes the place of
    /// `api_key`.
    pub(crate) openrouter_key_secret: Option<String>,
    /// The Ollama models run by the existing configuration's Agents that
    /// "Update this setup" keeps as they are: the person's own and those of
    /// onboarding's team edited by hand (`config::update` moves onboarding's
    /// own to the models chosen). `None` when this run writes the team: no
    /// configuration, Replace, or one without Agents.
    pub(crate) agent_models: Option<Vec<String>>,
}

impl Kept {
    /// Read what `text` (an existing configuration) sets up.
    pub(crate) fn from_config(text: &str) -> Self {
        let run = config::run_models(text);
        let marker = config::writer_marker(text);
        let writer = marker
            .as_ref()
            .and_then(|marker| WriterChoice::from_id(&marker.writer))
            .or_else(|| {
                run.writer
                    .as_deref()
                    .and_then(|spec| ModelRef::parse(spec).ok())
                    .map(|model| match model.provider {
                        Provider::Ollama => WriterChoice::Ollama,
                        Provider::OpenRouter => WriterChoice::Openrouter,
                    })
            });
        let existing = config::read_existing(text).ok();
        let agent_models = existing
            .as_ref()
            .and_then(|existing| config::kept_agent_models(existing, text));
        let openrouter = existing.and_then(|existing| existing.providers?.openrouter);
        let openrouter_key_secret = openrouter
            .as_ref()
            .and_then(|openrouter| openrouter.get("api_key_secret"))
            .and_then(serde_json::Value::as_str)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        let openrouter_key = openrouter
            .as_ref()
            .and_then(|openrouter| {
                openrouter
                    .get("api_key")
                    .and_then(serde_json::Value::as_str)
                    .map(|key| zeroize::Zeroizing::new(key.to_string()))
            })
            .filter(|key| !key.trim().is_empty() && !key.contains("your-key"))
            // The secret, when one is named, is the key (as the daemon
            // reads it).
            .filter(|_| openrouter_key_secret.is_none());
        Self {
            writer,
            writer_model: run.writer,
            reviewer: run.reviewer,
            codex_credential: marker.and_then(|marker| marker.credential),
            openrouter_key,
            openrouter_key_secret,
            agent_models,
        }
    }
}

impl Onboarding {
    /// How `axocoatl onboard` runs again to put other models in place of
    /// `models`, and why: `--replace` when an Agent "Update this setup"
    /// keeps as it is (one edited by hand, or the person's own) runs one of
    /// them; else `--update`, which moves onboarding's team and `run.models`
    /// to the new models.
    pub(crate) fn model_change(&self, models: &[&str]) -> (&'static str, String) {
        let agents_run = self.kept.agent_models.as_ref().is_some_and(|kept| {
            models
                .iter()
                .any(|model| kept.iter().any(|kept| kept == model))
        });
        if agents_run {
            (
                "--replace",
                format!(
                    " (--replace backs up the configuration and writes the team for the new \
                     models; --update would keep the Agents you edited, which run {})",
                    models.join(" and ")
                ),
            )
        } else {
            ("--update", String::new())
        }
    }

    /// Record a step left (and print it).
    pub(crate) fn step_left(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.ui.fail(&text);
        self.steps.push(text);
    }

    /// Mark the steps from `from` on as being about what `key` returns for
    /// each.
    fn key_steps_since(&mut self, from: usize, key: impl Fn(&str) -> Vec<String>) {
        let keys: Vec<String> = self.steps[from.min(self.steps.len())..]
            .iter()
            .flat_map(|step| key(step))
            .collect();
        self.step_keys.extend(keys);
    }

    pub(crate) fn memory_gb(&self) -> Option<u64> {
        self.facts.memory_gb()
    }

    /// The data directory this configuration uses: the one `recipe build`
    /// and `connect` record into (`AXOCOATL_DATA_DIR` when it is set).
    pub(crate) fn data_dir(&self) -> PathBuf {
        self.data_dir_override.clone().unwrap_or_else(|| {
            std::env::var_os("AXOCOATL_DATA_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| self.paths.data_dir.clone())
        })
    }

    /// What Step 5's check (the code `doctor` runs) needs about this
    /// computer; `contexts`: the models Step 3 already loaded.
    fn doctor_env(
        &self,
        contexts: &std::collections::BTreeMap<String, usize>,
    ) -> crate::doctor::Environment {
        crate::doctor::Environment {
            contexts_read: contexts.clone(),
            os: self.facts.os.clone(),
            user: self.facts.user.clone(),
            memory_bytes: self.facts.memory_bytes,
            cpus: self.facts.cpus,
            home: self.host.home.clone(),
            xdg_config_home: self.facts.xdg_config_home.clone(),
            host: self.host.clone(),
            load_models: true,
            socket_path: self.socket_path.clone(),
            // The Podman the daemon will use: the one on PATH. None there is
            // Podman not installed, as the daemon will find it.
            podman: self
                .host
                .find_on_path("podman")
                .unwrap_or_else(|| PathBuf::from("/nonexistent/podman"))
                .display()
                .to_string(),
            podman_connection: self.facts.podman.connection.clone(),
            id_root: self.host.root.clone(),
            data_dir: self.data_dir_override.clone(),
            landlock_abi: self.facts.landlock_abi,
            user_systemd: self.facts.user_systemd.clone(),
            selinux_enforcing: self.facts.selinux_enforcing,
            git: self.facts.git.clone(),
            proxy_variables: self.facts.proxy_variables.clone(),
            // Said as Step 3 says it, and logged.
            on_load: Some({
                let log = self.ui.log.clone();
                std::sync::Arc::new(move |model: &str| {
                    let line = format!(
                        "  Loading {model} once to read its context (your configuration runs \
                         it)…"
                    );
                    println!("{line}");
                    if let Some(log) = &log {
                        log.line(&line);
                    }
                })
            }),
            // The configuration onboarding writes is the default one.
            default_config: self.paths.config_path.clone(),
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
    /// Claude Code or Codex, when one of them writes.
    pub(crate) external_writer: Option<WriterChoice>,
    /// A writer model for an external writer (`anthropic:…`, `openai:…`).
    pub(crate) external_writer_model: Option<String>,
    /// The credential a Codex run uses, named on its command line.
    pub(crate) codex_credential: Option<&'static str>,
    /// The Ollama models Step 3 loaded and read with enough context, and
    /// that context: Step 5's check does not load them again.
    pub(crate) contexts: std::collections::BTreeMap<String, usize>,
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
    let mut facts = detect::detect(&host, &ollama_url, args.port.is_none()).await;
    if facts.daemon_running {
        let (server_host, port) = std::fs::read_to_string(&paths.config_path)
            .ok()
            .and_then(|text| config::read_existing(&text).ok())
            .and_then(|existing| existing.server?.port)
            .map_or(("127.0.0.1".to_string(), 8080), |port| {
                ("127.0.0.1".to_string(), port)
            });
        facts.daemon_version = crate::doctor::daemon_version(&server_host, port).await;
    }
    for line in detect::summary(&facts) {
        ui.say(line);
    }
    for item in step_0_notes(&facts) {
        ui.warn(item.line());
        if let Some(fix) = &item.fix {
            ui.say(format!("    {fix}"));
        }
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
        openrouter_sign_in: None,
        kept: Kept::default(),
        step_keys: Default::default(),
    };
    run(&mut onboarding).await
}

/// What Step 0 says beyond what it found: a platform not verified yet, and
/// a proxy that a Session's network does not use (Step 5's check names them
/// again only as part of doctor's report, so it leaves them out).
pub(crate) fn step_0_notes(facts: &Facts) -> Vec<crate::doctor::Item> {
    crate::doctor::platform_items(&facts.os, facts.selinux_enforcing, None, false)
        .into_iter()
        .chain(crate::doctor::proxy_item(&facts.proxy_variables))
        .filter(|item| item.status != crate::doctor::Status::Ok)
        .collect()
}

/// Items of Step 5's check that Step 0 or Step 3 already said.
const SAID_BEFORE: [&str; 2] = ["Platform", "Proxy"];

/// Steps 0 to 7 after detection.
pub(crate) async fn run(o: &mut Onboarding) -> i32 {
    // Stops before anything else.
    if o.facts.root_user {
        o.ui.fail("Run Axocoatl as a normal user; loadout runs need rootless Podman.");
        return exit::NOT_READY;
    }
    if o.facts.os.wsl() == Some(Wsl::One) {
        o.ui.fail(
            "This is WSL 1, which cannot run Podman. In PowerShell run `wsl --set-version DISTRO \
             2`, where DISTRO is the name `wsl -l -v` lists for this distribution, then run \
             axocoatl onboard again.",
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
                            "Configuration left unchanged: {}. Checking it as it is:",
                            o.paths.config_path.display()
                        ));
                        o.ui.heading("Step 5 · Check");
                        return if check(o, &Default::default()).await {
                            exit::READY
                        } else {
                            finish_not_ready(o)
                        };
                    }
                }
            }
        }
    };
    if let ExistingConfig::Update(text) = &existing {
        o.kept = Kept::from_config(text);
    }

    // Step 1: who writes.
    o.ui.heading("Step 1 · Who writes the code");
    let mut writer = choose_writer(o);
    keep_models(o, writer);
    o.ui.say(
        "Every fix run also needs a reviewer: a different model that only reads the change. \
         Claude Code and Codex can write but cannot review.",
    );

    // Step 2: the sandbox.
    o.ui.heading("Step 2 · The sandbox (rootless Podman)");
    let from = o.steps.len();
    let sandbox_ready = sandbox::run(o).await;
    o.key_steps_since(from, |_| vec!["sandbox".to_string()]);

    // Step 3: the writer and the reviewer.
    let mut choice = Choice::default();
    let from = o.steps.len();
    match writer {
        WriterChoice::Ollama => {
            o.ui.heading("Step 3 · A local writer and reviewer (Ollama)");
            // Too little memory for a tested local pair: the person may
            // have chosen a hosted writer instead.
            match ollama::writer_and_reviewer(o, &mut choice).await {
                None => {}
                Some(WriterChoice::Openrouter) => writer = WriterChoice::Openrouter,
                Some(other) => {
                    writer = other;
                    // A kept or named local writer model is not theirs.
                    o.args.writer_model = None;
                    o.ui.heading(writers::heading(writer));
                    writers::run(o, writer, sandbox_ready, &mut choice).await;
                }
            }
        }
        WriterChoice::Openrouter => {
            o.ui.heading("Step 3 · A hosted writer and reviewer (OpenRouter)");
            openrouter::run(o, &mut choice, true).await;
        }
        WriterChoice::ClaudeCode | WriterChoice::Codex => {
            o.ui.heading(writers::heading(writer));
            writers::run(o, writer, sandbox_ready, &mut choice).await;
        }
    }
    let models: Vec<String> = [&choice.writer_model, &choice.reviewer_model]
        .into_iter()
        .flatten()
        .filter(|model| model.provider == Provider::Ollama)
        .map(|model| model.model.clone())
        .collect();
    o.key_steps_since(from, |step| step_keys(step, writer, &models));

    // Step 4: the configuration (its heading once there is one to show).
    let Some(plan) = config_plan(o, writer, &choice, &existing) else {
        return finish_unsaved(o, &existing);
    };
    if !write_config(o, &plan, &existing) {
        return finish_unsaved(o, &existing);
    }

    // Step 5: the same check as `axocoatl doctor`.
    o.ui.heading("Step 5 · Check");
    if o.ui.plan() {
        o.ui.say("  would run the same checks as `axocoatl doctor` on the new configuration");
    } else if !check(o, &choice.contexts).await {
        return finish_not_ready(o);
    }

    // Step 6 and 7.
    let start = choose_start(o);
    let mut code = exit::READY;
    let service_started = if start == StartChoice::Service {
        match start_service(o).await {
            Some(started) => started,
            None => return finish_not_ready(o),
        }
    } else {
        false
    };
    if wants_smoke_test(o, writer) {
        o.ui.heading("Step 7 · A first run");
        if !smoke::run(
            o,
            writer,
            &choice,
            service_started,
            start == StartChoice::Dev,
        )
        .await
        {
            code = exit::NOT_READY;
        }
    }
    o.ui.say(format!("\n{}", next_command(writer, &choice)));
    if o.ui.plan() {
        if start == StartChoice::Dev {
            o.ui.say(format!(
                "  would start: axocoatl dev --config {}",
                o.paths.config_path.display()
            ));
        }
        o.ui.say("\nPlan only: nothing was changed.");
        return exit::READY;
    }
    if !o.steps.is_empty() {
        return finish_not_ready(o);
    }
    if start == StartChoice::Dev && code == exit::READY {
        o.ui.say("\nStarting `axocoatl dev` (Ctrl+C stops it).");
        start_dev(o).await;
    }
    code
}

/// `axocoatl dev` for this configuration, as its own process with the
/// terminal (and the signals' default actions, which a first run's
/// handlers in this process replaced). Ctrl+C reaches it from the
/// terminal; onboarding waits for it to stop.
async fn start_dev(o: &Onboarding) {
    use tokio::signal::unix::{signal, SignalKind};
    // Held while dev runs: Ctrl+C is dev's to answer.
    let _interrupt = signal(SignalKind::interrupt());
    let status = tokio::process::Command::new(&o.host.exe)
        .arg("dev")
        .arg("--config")
        .arg(&o.paths.config_path)
        .status()
        .await;
    if let Err(error) = status {
        o.ui.fail(format!("`axocoatl dev` could not start: {error}"));
    }
}

/// What a step from Step 3 is about: each Ollama model it names, else the
/// writer's own service.
fn step_keys(step: &str, writer: WriterChoice, models: &[String]) -> Vec<String> {
    let named: Vec<String> = models
        .iter()
        .filter(|model| step.contains(model.as_str()))
        .map(|model| format!("model:{model}"))
        .collect();
    if !named.is_empty() {
        return named;
    }
    let lower = step.to_ascii_lowercase();
    if lower.contains("openrouter") {
        vec!["openrouter".to_string()]
    } else if lower.contains("ollama") {
        vec!["ollama".to_string()]
    } else {
        vec![format!("writer:{}", writer.id())]
    }
}

/// What a check item is about, as [`step_keys`] names it.
fn item_key(section: &str, item: &crate::doctor::Item) -> Option<String> {
    match (section, item.name.as_str()) {
        ("sandbox", _) => Some("sandbox".to_string()),
        ("models", "Ollama") => Some("ollama".to_string()),
        ("models", "OpenRouter") => Some("openrouter".to_string()),
        ("models", name) => name
            .strip_prefix("Model ")
            .map(|model| format!("model:{model}")),
        ("writers", "Claude Code") => Some("writer:claude-code".to_string()),
        ("writers", "Codex") => Some("writer:codex".to_string()),
        _ => None,
    }
}

/// "Update this setup" keeps the models an existing configuration runs
/// with, unless `--writer-model` or `--reviewer` names others or they do not
/// fit the writer chosen now. At a terminal, a local model too large for
/// this computer's memory (as `doctor` judges it) is asked again instead:
/// `doctor`'s fix for one no Agent runs is `axocoatl onboard --update`.
fn keep_models(o: &mut Onboarding, writer: WriterChoice) {
    let kept = o.kept.clone();
    let writer_provider = match writer {
        WriterChoice::Ollama => Some("ollama:"),
        WriterChoice::Openrouter => Some("openrouter:"),
        WriterChoice::ClaudeCode | WriterChoice::Codex => None,
    };
    let keeps = |o: &Onboarding, role: &str, spec: &str| -> bool {
        let too_large = spec
            .strip_prefix("ollama:")
            .and_then(|model| ollama::too_large(o, model));
        match too_large {
            Some(why) if o.ui.interactive() => {
                o.ui.say(format!(
                    "  {role} model in your configuration: {spec}, {why}; choose it again below"
                ));
                false
            }
            Some(why) => {
                o.ui.warn(format!(
                    "{role} model: {spec} (kept from your configuration; {why})"
                ));
                true
            }
            None => {
                o.ui.say(format!(
                    "  {role} model: {spec} (kept from your configuration)"
                ));
                true
            }
        }
    };
    if o.args.writer_model.is_none() {
        if let (Some(model), Some(provider)) = (&kept.writer_model, writer_provider) {
            if model.starts_with(provider) && keeps(o, "Writer", model) {
                o.args.writer_model = Some(model.clone());
            }
        }
    }
    if o.args.reviewer.is_none() {
        if let Some(reviewer) = &kept.reviewer {
            let fits = writer != WriterChoice::Openrouter || reviewer.starts_with("openrouter:");
            if fits
                && o.args.writer_model.as_deref() != Some(reviewer.as_str())
                && keeps(o, "Reviewer", reviewer)
            {
                o.args.reviewer = Some(reviewer.clone());
            }
        }
    }
}

/// Print the steps left and return [`exit::NOT_READY`] when nothing was
/// written in Step 4, saying so: running onboarding again asks again.
fn finish_unsaved(o: &Onboarding, existing: &ExistingConfig) -> i32 {
    print_steps(o);
    o.ui.say(match existing {
        _ if o.ui.plan() => "Plan only: nothing was changed.".to_string(),
        ExistingConfig::None => "Nothing was saved: no configuration was written. Run `axocoatl \
                                 onboard` again once the steps are done."
            .to_string(),
        ExistingConfig::Update(_) | ExistingConfig::Replace(_) => format!(
            "Your configuration was not changed ({} is as it was). Run `axocoatl onboard` again \
             once the steps are done.",
            o.paths.config_path.display()
        ),
    });
    exit::NOT_READY
}

/// Print the steps left and return [`exit::NOT_READY`].
fn finish_not_ready(o: &Onboarding) -> i32 {
    print_steps(o);
    o.ui.say("Run `axocoatl onboard` again once they are done; it keeps what is already set up.");
    exit::NOT_READY
}

/// "Not ready yet" and the numbered steps left.
fn print_steps(o: &Onboarding) {
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
}

/// Step 1's default: Claude Code when `claude` is installed, else Codex
/// when `codex` is, else a local model with 48 GB or more, else OpenRouter.
pub(crate) fn default_writer(facts: &Facts) -> WriterChoice {
    if facts.claude.is_some() {
        WriterChoice::ClaudeCode
    } else if facts.codex.is_some() {
        WriterChoice::Codex
    } else if facts
        .memory_bytes
        .is_some_and(|bytes| axocoatl_isolation::host::at_least_gb(bytes, 48))
    {
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
    if let (Some(writer), Mode::Yes) = (o.kept.writer, o.ui.mode) {
        o.ui.say(format!(
            "  Writer: {} (kept from your configuration)",
            writer.label(&o.facts)
        ));
        return writer;
    }
    let choices = [
        WriterChoice::ClaudeCode,
        WriterChoice::Codex,
        WriterChoice::Ollama,
        WriterChoice::Openrouter,
    ];
    // Updating a setup starts from the writer it has.
    let default = o.kept.writer.unwrap_or_else(|| default_writer(&o.facts));
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
    // A step left already says why a model is missing (a declined key, a
    // model OpenRouter does not list, the same model twice); one more
    // would repeat it.
    let reviewer = choice.reviewer_model.clone();
    let Some(reviewer) = reviewer else {
        if o.steps.is_empty() {
            o.step_left(
                "Choose a reviewer model: run `axocoatl onboard` again, or pass --reviewer \
                 ollama:MODEL or --reviewer openrouter:VENDOR/MODEL",
            );
        }
        return None;
    };
    let (lead, run_writer, note) = match writer {
        WriterChoice::Ollama | WriterChoice::Openrouter => {
            let Some(model) = choice.writer_model.clone() else {
                if o.steps.is_empty() {
                    o.step_left(
                        "Choose a writer model: run `axocoatl onboard` again, or pass \
                         --writer-model",
                    );
                }
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
    o.ui.heading("Step 4 · The configuration");
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
        writer: writer.id(),
        codex_credential: choice.codex_credential,
    })
}

/// Write the configuration: a new one, the update of an existing one, or
/// its replacement, after showing the change and backing the old file up.
fn write_config(o: &mut Onboarding, plan: &ConfigPlan, existing: &ExistingConfig) -> bool {
    let path = o.paths.config_path.clone();
    let mut kept = Vec::new();
    let (old, new) = match existing {
        ExistingConfig::None => (None, config::render(plan)),
        ExistingConfig::Replace(old) => (Some(old.clone()), config::render(plan)),
        ExistingConfig::Update(old) => match config::update(old, plan, &path) {
            Ok(updated) => {
                kept = updated.kept;
                (Some(old.clone()), updated.text)
            }
            Err(error) => {
                o.step_left(format!(
                    "{error}: run `axocoatl onboard --replace` (the old file is backed up first)"
                ));
                return false;
            }
        },
    };
    // Agents of onboarding's team edited by hand stay as they are.
    if !kept.is_empty() {
        let agents: Vec<String> = kept
            .iter()
            .map(|(id, model)| format!("{id} on {model}"))
            .collect();
        o.ui.say(format!(
            "  Kept as you edited {}: {} (the rest of onboarding's team moves to the models \
             chosen; `axocoatl onboard --replace` writes the whole team again)",
            if kept.len() == 1 { "it" } else { "them" },
            agents.join(", ")
        ));
    }
    if let Some(old) = &old {
        if config::diff(old, &new).is_empty() {
            o.ui.ok(format!("{} already has this setup", path.display()));
            return true;
        }
        let backup = format!("config.yaml.bak-{}", host::stamp());
        if matches!(existing, ExistingConfig::Replace(_)) {
            // The old file is not echoed: it is kept as the backup.
            o.ui.say(format!(
                "  {} is replaced (the current file is kept as {backup}). The new file:",
                path.display()
            ));
            for line in config::redacted_lines(&new) {
                o.ui.say(format!("    + {line}"));
            }
        } else {
            o.ui.say(format!("  Changes to {}:", path.display()));
            for line in config::diff_redacted(old, &new) {
                o.ui.say(format!("    {line}"));
            }
        }
        let question =
            format!("Write these changes? The current file is kept as {backup} next to it.");
        // The person chose to update or replace this file, and every change
        // shown follows from their answers: Enter writes it.
        if o.ui.plan() {
            o.ui.consent(&question, true, false);
            return true;
        }
        if !o.ui.consent(&question, true, false) {
            o.step_left(format!(
                "The configuration was not changed: run `axocoatl onboard` again to write it ({})",
                path.display()
            ));
            return false;
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
        for line in config::redacted_lines(&new) {
            o.ui.say(format!("    + {line}"));
        }
        return true;
    } else {
        // A new file: what it sets, then it is written (nothing existing
        // changes, and the answers before chose each setting).
        o.ui.say("  The new configuration sets:");
        for line in settings(plan) {
            o.ui.say(format!("    {line}"));
        }
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

fn write_backup(paths: &UserPaths, name: &str, text: &str) -> std::io::Result<()> {
    let directory = axocoatl_core::SecureDir::open_or_create_all(&paths.config_dir)?;
    crate::admit_private_directory(&directory)?;
    directory.atomic_write(name, text.as_bytes())
}

/// What a new configuration sets, one line each, the key never shown.
fn settings(plan: &ConfigPlan) -> Vec<String> {
    let mut lines = Vec::new();
    match (&plan.run_writer, &plan.run_reviewer) {
        (Some(writer), Some(reviewer)) => lines.push(format!(
            "`axocoatl run fix`: writer {}, reviewer {}",
            writer.spec(),
            reviewer.spec()
        )),
        (None, Some(reviewer)) => lines.push(format!(
            "`axocoatl run fix-{}`: {} writes, reviewer {}{}",
            plan.writer,
            if plan.writer == "codex" {
                "Codex"
            } else {
                "Claude Code"
            },
            reviewer.spec(),
            plan.codex_credential
                .map(|credential| format!(", billed through {credential}"))
                .unwrap_or_default()
        )),
        _ => {}
    }
    lines.push(format!(
        "Workbench Sessions: Lead, Scout and the chat Assistant on {}, Reviewer on {}",
        plan.lead.spec(),
        plan.reviewer.spec()
    ));
    if let Some(url) = &plan.ollama_base_url {
        lines.push(format!("Ollama server: {url}"));
    }
    match &plan.openrouter_key {
        Some(KeyRef::Env(variable)) => lines.push(format!(
            "OpenRouter key: read from ${variable} when Axocoatl starts"
        )),
        Some(KeyRef::Value(_)) => {
            lines.push("OpenRouter key: stored in this owner-only file (never shown)".to_string())
        }
        Some(KeyRef::Secret(name)) => lines.push(format!(
            "OpenRouter key: read from Axocoatl's secret store (secret {name}) when Axocoatl \
             starts, never written in this file"
        )),
        None => {}
    }
    lines.push(format!("Workbench port: {}", plan.port));
    lines
}

/// Step 5: the same checks as `axocoatl doctor`. Prints what failed and
/// what is ready; adds each failure to the steps left. `contexts`: the
/// models Step 3 loaded, which are not loaded again.
async fn check(o: &mut Onboarding, contexts: &std::collections::BTreeMap<String, usize>) -> bool {
    let env = o.doctor_env(contexts);
    if env.data_dir.is_none() {
        if let Err(error) = crate::configure_data_dir(&o.paths.config_path) {
            o.step_left(format!("Cannot select the data directory: {error}"));
            return false;
        }
    }
    let report = crate::doctor::collect(&o.paths.config_path, &env).await;
    for (section, items) in [
        ("sandbox", &report.sections.sandbox),
        ("models", &report.sections.models),
        ("writers", &report.sections.writers),
        ("daemon", &report.sections.daemon),
    ] {
        for item in items {
            match item.status {
                crate::doctor::Status::Ok => {}
                crate::doctor::Status::Warn if SAID_BEFORE.contains(&item.name.as_str()) => {}
                crate::doctor::Status::Warn => o.ui.warn(item.line()),
                crate::doctor::Status::Fail => {
                    o.ui.fail(item.line());
                    // A problem an earlier step already left is not listed
                    // twice.
                    let covered =
                        item_key(section, item).is_some_and(|key| o.step_keys.contains(&key));
                    let step = item.step();
                    if !covered && !o.steps.contains(&step) {
                        o.steps.push(step);
                    }
                }
            }
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
    if o.ui.mode == Mode::Yes {
        return StartChoice::None;
    }
    o.ui.heading("Step 6 · Start Axocoatl");
    let unavailable = service_unavailable(o);
    let service = match &unavailable {
        Some((why, _)) => format!("Always on, starting at login (not available: {why})"),
        None => "Always on, starting at login (a launchd or systemd service)".to_string(),
    };
    let items = vec![
        "In this terminal now (axocoatl dev)".to_string(),
        service,
        "I'll start it later".to_string(),
    ];
    match o.ui.select("How should Axocoatl run?", &items, 0) {
        0 => StartChoice::Dev,
        1 if unavailable.is_none() => StartChoice::Service,
        1 => {
            if let Some((_, fix)) = unavailable {
                o.ui.say(format!("  {fix}"));
            }
            StartChoice::None
        }
        _ => StartChoice::None,
    }
}

/// Why the Always-On Service cannot run for this login, and what to do:
/// a Linux without systemd, or a login without a systemd user session
/// (`su -`, `sudo -iu`, or an SSH login that started none), where `systemctl
/// --user` reaches no manager.
fn service_unavailable(o: &Onboarding) -> Option<(String, String)> {
    match &o.facts.os {
        Os::Linux { systemd: false, .. } => Some((
            "this Linux runs without systemd".to_string(),
            "Start Axocoatl with `axocoatl dev` instead".to_string(),
        )),
        Os::Linux { .. } => {
            let systemd = o.facts.user_systemd.as_ref()?;
            if systemd.reachable {
                return None;
            }
            let item = crate::doctor::user_systemd_item(systemd, &o.facts.user, false);
            Some((
                "this login has no systemd user session".to_string(),
                format!(
                    "{}. Until then, start Axocoatl with `axocoatl dev`",
                    item.fix.unwrap_or_default()
                ),
            ))
        }
        Os::MacOs { .. } => None,
    }
}

/// Install and start the Always-On Service (asked for with Step 6):
/// `Some(true)` when it was started, `Some(false)` when that was declined,
/// `None` (with the step left) when it failed.
async fn start_service(o: &mut Onboarding) -> Option<bool> {
    if let Some((why, fix)) = service_unavailable(o) {
        o.step_left(format!(
            "The Always-On Service cannot run here: {why}. {fix}"
        ));
        return None;
    }
    let config = std::fs::read_to_string(&o.paths.config_path).unwrap_or_default();
    if let Err(error) = crate::reject_uninherited_service_environment(&config) {
        o.step_left(format!("Cannot install the Always-On Service: {error}"));
        return None;
    }
    if !o.ui.consent(
        "Install the Always-On Service (it starts Axocoatl at login and keeps it running)?",
        true,
        false,
    ) {
        return Some(false);
    }
    if o.ui.plan() {
        o.ui.say("    would run: axocoatl service install && axocoatl service start");
        return Some(true);
    }
    let config = o.paths.config_path.display().to_string();
    for args in [
        vec!["service", "install", "--config", &config],
        vec!["service", "start"],
    ] {
        let cmd = Cmd::new(&o.host.exe.display().to_string(), &args);
        if let Err(error) = o.host.run(&cmd).await {
            o.step_left(format!("{error}; see `axocoatl service status`"));
            return None;
        }
    }
    Some(true)
}

fn wants_smoke_test(o: &mut Onboarding, writer: WriterChoice) -> bool {
    match o.args.smoke_test() {
        Some(choice) => choice,
        None if o.ui.mode == Mode::Yes => false,
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
