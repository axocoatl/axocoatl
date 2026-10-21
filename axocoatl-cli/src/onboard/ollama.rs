//! Step 3A: a local writer and reviewer on Ollama, or the reviewer alone
//! for Claude Code and Codex.
//!
//! Install Ollama when it is missing, start it, fix a version native
//! Sessions refuse (upgrade one that is too old; restart a server older than
//! its upgraded CLI; never `brew upgrade` for one that is too new), turn
//! cloud models off where the running server reads it (the user's
//! `~/.ollama/server.json` for the brew service, a systemd drop-in for the
//! Linux service, the app's setting) and wait until the server says so,
//! choose models that fit this computer's memory, pull them with their sizes
//! shown and their exit status checked, and load each once to read its
//! context, giving the service `OLLAMA_CONTEXT_LENGTH` when it is too small.

use std::time::Duration;

use axocoatl_llm_ollama::{ollama_version_note, ollama_version_refusal};

use super::config::{ModelRef, Provider};
use super::detect::{self, OllamaServer, OllamaService, Wsl};
use super::host::Cmd;
use super::{Choice, Onboarding, WriterChoice};
use crate::ollama_setup::{
    self, fits_memory, known_size, size_text, OllamaHost, TeamModels, CONTEXT_FIX_TOKENS,
    MIN_CONTEXT_TOKENS, PINNED_LINUX_INSTALL,
};

/// How long onboarding waits for Ollama to come up or to report a change.
const WAIT: Duration = Duration::from_secs(60);

pub(crate) fn ollama_host(o: &Onboarding) -> OllamaHost {
    OllamaHost {
        service: o.facts.ollama_service,
        macos: o.facts.os.is_macos(),
        brew_env_file: Some(brew_env_file(o)),
        cli_version: o
            .facts
            .ollama_cli
            .as_ref()
            .and_then(|cli| cli.client.clone()),
    }
}

fn brew_env_file(o: &Onboarding) -> std::path::PathBuf {
    detect::homebrew_services_dir(&o.host.home, o.facts.xdg_config_home.as_deref())
        .join("ollama.env")
}

/// Read the server again (and, after an install, the CLI and the service).
async fn reprobe(o: &mut Onboarding) {
    if o.ui.plan() {
        return;
    }
    let url = o.facts.ollama_url.clone();
    o.facts.ollama_server = OllamaServer::probe(&url).await;
    if let Some(path) = o.host.find("ollama") {
        let probe = o
            .host
            .probe_env(
                &path.to_string_lossy(),
                &["--version"],
                &[("OLLAMA_HOST", url.as_str())],
            )
            .await;
        let (client, server) = probe
            .map(|probe| {
                detect::parse_ollama_version(&format!("{}\n{}", probe.stdout, probe.stderr))
            })
            .unwrap_or((None, None));
        o.facts.ollama_cli = Some(detect::OllamaCli {
            path,
            client,
            server,
        });
    }
    o.facts.ollama_service = detect::ollama_service(
        &o.host,
        o.facts.xdg_config_home.as_deref(),
        detect::url_port(&url),
    );
}

/// Poll until `done` holds for the server, at most [`WAIT`].
async fn wait_until(o: &mut Onboarding, done: impl Fn(&OllamaServer) -> bool) -> bool {
    if o.ui.plan() {
        return true;
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        reprobe(o).await;
        if done(&o.facts.ollama_server) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn reachable(server: &OllamaServer) -> bool {
    matches!(server, OllamaServer::Reachable { .. })
}

fn cloud_disabled(server: &OllamaServer) -> bool {
    matches!(server, OllamaServer::Reachable { check: Ok(check), .. } if check.cloud_disabled())
}

/// The whole local path: the server, a writer and a reviewer on it, unless
/// this computer is too small for a tested pair and the person chooses
/// OpenRouter for one or both, or Claude Code or Codex to write. Returns
/// the writer when it is no longer a local model: OpenRouter (set up
/// here), or Claude Code or Codex (for the caller to set up).
pub(crate) async fn writer_and_reviewer(
    o: &mut Onboarding,
    choice: &mut Choice,
) -> Option<WriterChoice> {
    let flag_writer = o
        .args
        .writer_model
        .as_deref()
        .and_then(|spec| ModelRef::parse(spec).ok());
    let flag_reviewer = o
        .args
        .reviewer
        .as_deref()
        .and_then(|spec| ModelRef::parse(spec).ok());
    let memory = o.facts.memory_bytes;
    let tested_tier = memory.and_then(ollama_setup::recommended_pair);
    let installed_pair = memory.and_then(|memory| {
        ollama_setup::suggest_fitting_team_models(o.facts.ollama_server.models(), memory)
            .filter(|team| team.lead != team.reviewer)
    });
    let mut local_reviewer =
        !matches!(&flag_reviewer, Some(model) if model.provider == Provider::OpenRouter);
    let mut local_writer = true;
    if tested_tier.is_none()
        && installed_pair.is_none()
        && flag_writer.is_none()
        && flag_reviewer.is_none()
    {
        let gb = o.memory_gb().unwrap_or_default();
        if o.ui.mode == super::ui::Mode::Yes {
            // --yes never turns a local writer someone asked for into a
            // hosted one, nor pulls an untested pair onto this computer.
            o.step_left(format!(
                "No local pair has been tested on {gb} GB of memory: name the models with \
                 --writer-model ollama:MODEL and --reviewer ollama:MODEL (or \
                 openrouter:VENDOR/MODEL), or use --writer openrouter"
            ));
            return None;
        }
        o.ui.say(format!("  {}", tested_pair_fit(memory)));
        let items = vec![
            "OpenRouter writes and reviews (hosted, paid with OpenRouter credits)".to_string(),
            "Claude Code writes (your Claude Pro or Max plan)".to_string(),
            "Codex writes (your ChatGPT plan)".to_string(),
            format!("A local writer, and OpenRouter reviews (not tested at {gb} GB)"),
            format!("Local models for both (not tested at {gb} GB)"),
        ];
        match o.ui.select(
            &format!(
                "No local pair has been tested on {gb} GB of memory. Who writes the code, and \
                 who reviews it?"
            ),
            &items,
            0,
        ) {
            0 => local_writer = false,
            1 => return Some(WriterChoice::ClaudeCode),
            2 => return Some(WriterChoice::Codex),
            3 => local_reviewer = false,
            _ => {}
        }
    }
    if !local_writer {
        o.ui.say("  The writer and the reviewer run on OpenRouter.");
        super::openrouter::run(o, choice, true).await;
        return Some(WriterChoice::Openrouter);
    }
    let base_url = server(o).await;
    choice.ollama_base_url = Some(o.facts.ollama_url.clone());
    let (writer, reviewer) = choose_models(o, base_url.is_some(), true, local_reviewer).await;
    choice.writer_model = writer.map(|model| ModelRef::new(Provider::Ollama, &model));
    if local_reviewer {
        choice.reviewer_model = reviewer.map(|model| ModelRef::new(Provider::Ollama, &model));
    } else {
        o.ui.say("  The reviewer runs on OpenRouter.");
        super::openrouter::run(o, choice, false).await;
    }
    let mut models: Vec<String> = [&choice.writer_model, &choice.reviewer_model]
        .into_iter()
        .flatten()
        .filter(|model| model.provider == Provider::Ollama)
        .map(|model| model.model.clone())
        .collect();
    models.dedup();
    if let Some(base_url) = base_url {
        if pull(o, &base_url, &models, WriterChoice::Ollama).await {
            let read = context(o, &base_url, &models, WriterChoice::Ollama).await;
            choice.contexts.extend(read);
        }
    } else {
        for model in &models {
            let step = pull_step(o, model, " once Ollama is ready", WriterChoice::Ollama);
            o.steps.push(step);
        }
    }
    None
}

/// The reviewer alone on Ollama (for Claude Code and Codex): the server and
/// one model, no writer questions.
pub(crate) async fn reviewer_only(o: &mut Onboarding, choice: &mut Choice) {
    // Claude Code or Codex, which `writers::run` recorded.
    let writer = choice.external_writer.unwrap_or(WriterChoice::ClaudeCode);
    let base_url = server(o).await;
    choice.ollama_base_url = Some(o.facts.ollama_url.clone());
    let (_, reviewer) = choose_models(o, base_url.is_some(), false, true).await;
    choice.reviewer_model = reviewer.map(|model| ModelRef::new(Provider::Ollama, &model));
    if let (Some(base_url), Some(reviewer)) = (base_url, &choice.reviewer_model) {
        let models = vec![reviewer.model.clone()];
        if pull(o, &base_url, &models, writer).await {
            let read = context(o, &base_url, &models, writer).await;
            choice.contexts.extend(read);
        }
    } else if let Some(reviewer) = &choice.reviewer_model {
        let step = pull_step(o, &reviewer.model, " once Ollama is ready", writer);
        o.steps.push(step);
    }
}

/// What the pair Axocoatl has tested needs, and which of its models fit a
/// computer with `memory` (said when no tested pair fits it).
fn tested_pair_fit(memory: Option<u64>) -> String {
    let team = TeamModels::recommended();
    let tested = format!(
        "The local pair Axocoatl has tested, {} writing and {} reviewing, runs on 48 GB of \
         memory",
        described(&team.lead),
        described(&team.reviewer)
    );
    let Some(memory) = memory else {
        return format!("{tested}.");
    };
    let gb = axocoatl_isolation::host::memory_gb(memory);
    let fits = |model: &str| known_size(model).is_none_or(|size| fits_memory(size, memory));
    match (fits(&team.lead), fits(&team.reviewer)) {
        (true, true) => format!(
            "{tested}; each of its models fits this computer's {gb} GB, but the pair has not \
             been tested at this size."
        ),
        (false, true) => format!(
            "{tested}; {} fits this computer's {gb} GB, {} is too large for it.",
            team.reviewer, team.lead
        ),
        (true, false) => format!(
            "{tested}; {} fits this computer's {gb} GB, {} is too large for it.",
            team.lead, team.reviewer
        ),
        (false, false) => format!(
            "{tested}; both are too large for this computer's {gb} GB, so onboarding suggests \
             no local model here. A model too large for the memory loads slowly or not at \
             all."
        ),
    }
}

/// `qwen3-coder:30b (18.6 GB)`.
fn described(model: &str) -> String {
    match known_size(model) {
        Some(size) => format!("{model} ({})", size_text(size)),
        None => model.to_string(),
    }
}

/// The size of `model`: as the server lists it when it is installed there,
/// else as the Ollama library lists it.
fn model_size(o: &Onboarding, model: &str) -> Option<u64> {
    o.facts
        .ollama_server
        .models()
        .iter()
        .find(|installed| installed.name() == model && installed.size_bytes() > 0)
        .map(|installed| installed.size_bytes())
        .or_else(|| known_size(model))
}

/// `too large for this computer's 12 GB` (as `doctor` says it) when
/// `model` is known not to fit this computer's memory.
pub(crate) fn too_large(o: &Onboarding, model: &str) -> Option<String> {
    let memory = o.facts.memory_bytes?;
    let size = model_size(o, model)?;
    (!fits_memory(size, memory)).then(|| {
        format!(
            "too large for this computer's {} GB",
            axocoatl_isolation::host::memory_gb(memory)
        )
    })
}

/// The step left for a model that is not pulled: pull it, or, when it is
/// too large for this computer, choose another (a pull would not help),
/// with the command that keeps `writer` ([`Onboarding::model_change`]).
pub(super) fn pull_step(o: &Onboarding, model: &str, when: &str, writer: WriterChoice) -> String {
    let Some(why) = too_large(o, model) else {
        return format!("Pull {model}{when}: ollama pull {model}");
    };
    let size = model_size(o, model)
        .map(|size| format!(" ({})", size_text(size)))
        .unwrap_or_default();
    let (mode, because) = o.model_change(&[model]);
    match writer {
        WriterChoice::ClaudeCode | WriterChoice::Codex => format!(
            "{model}{size} is {why}, so it was not pulled: choose a smaller reviewer with \
             `axocoatl onboard {mode} --writer {id}`, or OpenRouter for the reviewer with \
             `axocoatl onboard {mode} --writer {id} --reviewer openrouter:{reviewer}`{because}",
            id = writer.id(),
            reviewer = super::openrouter::DEFAULT_REVIEWER
        ),
        WriterChoice::Ollama | WriterChoice::Openrouter => format!(
            "{model}{size} is {why}, so it was not pulled: choose a smaller model with `axocoatl \
             onboard {mode} --writer ollama`, or a hosted writer and reviewer with `axocoatl \
             onboard {mode} --writer openrouter` (or `--writer claude-code`, or `--writer \
             codex`){because}"
        ),
    }
}

/// A native-ready Ollama server: installed, running, an accepted version,
/// cloud models off. Returns its URL, or `None` with the steps left said.
pub(crate) async fn server(o: &mut Onboarding) -> Option<String> {
    if o.ui.interactive() && !reachable(&o.facts.ollama_server) {
        other_server(o).await;
    }
    let url = o.facts.ollama_url.clone();
    if o.facts.windows_ollama {
        o.ui.warn(
            "ollama.exe on PATH is the Windows app: WSL in its default NAT mode cannot reach it \
             at localhost, so Ollama is installed inside this WSL distribution instead",
        );
    }
    if o.facts.ollama_cli.is_none() && !reachable(&o.facts.ollama_server) {
        if !install(o).await {
            return None;
        }
        if !o.ui.plan() && !wait_until(o, reachable).await {
            o.step_left(format!(
                "Ollama was installed but does not answer at {url}: {}",
                ollama_host(o).start()
            ));
            return None;
        }
    }
    if o.ui.plan() && o.facts.ollama_cli.is_some() && !reachable(&o.facts.ollama_server) {
        o.ui.say(format!(
            "  Ollama is installed but not running at {url}; would ask to start it: {}",
            ollama_host(o).start()
        ));
    }
    if !o.ui.plan()
        && !reachable(&o.facts.ollama_server)
        && (!start(o).await || !wait_until(o, reachable).await)
    {
        o.step_left(format!(
            "Start Ollama so it answers at {url}: {}",
            ollama_host(o).start()
        ));
        return None;
    }
    for _ in 0..3 {
        if o.ui.plan() {
            if let OllamaServer::Unreachable(_) = o.facts.ollama_server {
                o.ui.say(format!(
                    "  would check Ollama's version and cloud mode at {url}"
                ));
                return Some(url);
            }
        }
        let check = match &o.facts.ollama_server {
            OllamaServer::Reachable {
                check: Ok(check), ..
            } => check.clone(),
            OllamaServer::Reachable {
                check: Err(error), ..
            } => {
                o.step_left(format!(
                    "Could not read the version and cloud mode of Ollama at {url}: {error}"
                ));
                return None;
            }
            OllamaServer::Unreachable(error) => {
                o.step_left(format!("Ollama does not answer at {url} ({error})"));
                return None;
            }
        };
        let version = check.version.clone().unwrap_or_default();
        if let Some(refusal) = check.version_refusal() {
            if !fix_version(o, check.version.as_deref(), &refusal).await {
                return None;
            }
            if o.ui.plan() {
                return Some(url);
            }
            if !wait_until(o, |server| {
                server
                    .version()
                    .is_some_and(|v| ollama_version_refusal(v).is_none())
            })
            .await
            {
                o.step_left(format!(
                    "Ollama at {url} still reports {}; {}",
                    o.facts.ollama_server.version().unwrap_or("no version"),
                    crate::ollama_setup::version_fix(
                        o.facts.ollama_server.version(),
                        &ollama_host(o)
                    )
                ));
                return None;
            }
            continue;
        }
        if let Some(note) = ollama_version_note(&version) {
            o.ui.warn(format!("{note}."));
        }
        if !check.cloud_disabled() {
            if !cloud_off(o).await {
                return None;
            }
            if o.ui.plan() {
                return Some(url);
            }
            if !wait_until(o, cloud_disabled).await {
                o.step_left(format!(
                    "Ollama at {url} still has cloud models on: {}",
                    ollama_host(o).cloud_off().join(" ")
                ));
                return None;
            }
            continue;
        }
        o.ui.ok(format!(
            "Ollama {version} at {url}, cloud models off ({} models installed)",
            o.facts.ollama_server.models().len()
        ));
        return Some(url);
    }
    o.step_left(format!(
        "Ollama at {url} is not ready: run `axocoatl doctor`"
    ));
    None
}

/// Nothing answers at the Ollama URL. Before installing or starting Ollama
/// there, ask whether it already runs at another address on this computer
/// (`--ollama-url` without a terminal), and use that one once it answers.
async fn other_server(o: &mut Onboarding) {
    let url = o.facts.ollama_url.clone();
    let here = if o.facts.ollama_cli.is_none() {
        "Install Ollama on this computer"
    } else {
        "Start Ollama"
    };
    let items = vec![
        format!("{here} (onboarding asks before each change)"),
        "It already runs at another address on this computer (enter it)".to_string(),
    ];
    let question = format!("Ollama does not answer at {url}. What should onboarding do?");
    if o.ui.select(&question, &items, 0) != 1 {
        return;
    }
    loop {
        let entered = o.ui.input(
            "Ollama's address, for example http://127.0.0.1:11435 (Enter to go back)",
            "",
        );
        if entered.is_empty() {
            return;
        }
        let candidate = if entered.contains("://") {
            entered
        } else {
            format!("http://{entered}")
        };
        let candidate = candidate.trim_end_matches('/').to_string();
        if let Err(error) = axocoatl_llm_ollama::validate_native_ollama_endpoint(&candidate) {
            o.ui.fail(format!(
                "{candidate}: {error}; native Sessions use only an Ollama server on this computer"
            ));
            continue;
        }
        if reachable(&OllamaServer::probe(&candidate).await) {
            o.facts.ollama_url = candidate;
            reprobe(o).await;
            o.ui.ok(format!(
                "Ollama answers at {} (the configuration will name it)",
                o.facts.ollama_url
            ));
            return;
        }
        o.ui.fail(format!("Ollama does not answer at {candidate} either"));
    }
}

async fn install(o: &mut Onboarding) -> bool {
    if o.facts.os.is_macos() {
        if o.facts.brew.is_none() {
            o.step_left(
                "Install Ollama from https://ollama.com/download (or install Homebrew and run \
                 `brew install ollama && brew services start ollama`), then run `axocoatl \
                 onboard` again",
            );
            return false;
        }
        let commands = vec![
            Cmd::new("brew", &["install", "ollama"]),
            Cmd::new("brew", &["services", "start", "ollama"]),
        ];
        if !o.ui.consent(
            "Install Ollama with `brew install ollama && brew services start ollama` (about 40 \
             MB; models download separately)?",
            true,
            false,
        ) {
            o.step_left("Install Ollama: brew install ollama && brew services start ollama");
            return false;
        }
        if let Err(error) = o.ui.execute_all(&o.host, &commands).await {
            o.step_left(format!(
                "{error}; install Ollama, then run `axocoatl onboard` again"
            ));
            return false;
        }
        true
    } else {
        let zstd = o.host.find("zstd").is_none();
        let prefix = match (
            zstd,
            o.facts
                .os
                .release()
                .and_then(|release| release.package_manager()),
        ) {
            (true, Some(manager)) => format!("{} && ", manager.install(&["zstd"]).display()),
            _ => String::new(),
        };
        let script = format!("{prefix}{PINNED_LINUX_INSTALL}");
        let cmd = Cmd::shell(&script).with_sudo_inside();
        let wsl = if o.facts.os.wsl() == Some(Wsl::Two) {
            " (inside this WSL distribution)"
        } else {
            ""
        };
        if !o.ui.consent(
            &format!(
                "Install Ollama 0.40.6{wsl} with `{script}`? It sets Ollama up as a systemd \
                 service; this asks for your password."
            ),
            true,
            true,
        ) {
            o.step_left(format!("Install Ollama: {script}"));
            return false;
        }
        match o.ui.execute_showing_output(&o.host, &cmd).await {
            Ok(output) => {
                if let Some(note) = installer_gpu_note(&output) {
                    o.ui.warn(note);
                }
                true
            }
            Err(error) => {
                o.step_left(format!(
                    "{error}; install Ollama, then run `axocoatl onboard` again"
                ));
                false
            }
        }
    }
}

/// What onboarding says after Ollama's Linux installer warns that it found
/// no NVIDIA or AMD GPU.
pub(crate) const CPU_ONLY: &str =
    "Ollama found no NVIDIA or AMD GPU here, so it runs models on the CPU alone: a local \
     model answers much more slowly than on a GPU or Apple silicon, and a fix run with local \
     models can take a long time. A hosted writer and reviewer (OpenRouter, Claude Code \
     or Codex) do not use this computer's processor for the model.";

/// What onboarding says when Ollama's Linux installer could not look for a
/// GPU (it needs `lspci` or `lshw`).
pub(crate) const GPU_UNKNOWN: &str =
    "Ollama's installer could not look for a GPU here (it needs lspci or lshw). Without an \
     NVIDIA or AMD GPU, Ollama runs models on the CPU alone: a local model answers much more \
     slowly than on a GPU or Apple silicon, and a fix run with local models can take a long \
     time.";

/// What to say about what Ollama's Linux installer printed about the GPU:
/// `WARNING: No NVIDIA/AMD GPU detected. Ollama will run in CPU-only mode.`,
/// or `WARNING: Unable to detect NVIDIA/AMD GPU. Install lspci or lshw …`.
pub(crate) fn installer_gpu_note(output: &str) -> Option<&'static str> {
    if output.contains("CPU-only mode") || output.contains("No NVIDIA/AMD GPU detected") {
        Some(CPU_ONLY)
    } else if output.contains("Unable to detect NVIDIA/AMD GPU") {
        Some(GPU_UNKNOWN)
    } else {
        None
    }
}

/// Start the server the way it runs here; `false` when nothing was started
/// (declined, or no one at a terminal to start it by hand).
async fn start(o: &mut Onboarding) -> bool {
    match o.facts.ollama_service {
        OllamaService::Brew => {
            let cmd = Cmd::new("brew", &["services", "start", "ollama"]);
            o.ui.consent(
                "Ollama is not running. Start it with `brew services start ollama`?",
                true,
                false,
            ) && o.ui.execute(&o.host, &cmd).await.is_ok()
        }
        OllamaService::Systemd => {
            let cmd = Cmd::sudo("systemctl", &["start", "ollama"]);
            o.ui.consent(
                "Ollama is not running. Start it with `sudo systemctl start ollama`?",
                true,
                true,
            ) && o.ui.execute(&o.host, &cmd).await.is_ok()
        }
        OllamaService::App => o.ui.wait_for_enter("Open the Ollama app."),
        OllamaService::Manual => o.ui.wait_for_enter(&format!(
            "Start Ollama in another terminal: OLLAMA_NO_CLOUD=1 OLLAMA_HOST={} ollama serve",
            o.facts.ollama_url.trim_start_matches("http://")
        )),
    }
}

/// Fix a refused server version; `false` with the step left when it cannot.
async fn fix_version(o: &mut Onboarding, server: Option<&str>, refusal: &str) -> bool {
    let host = ollama_host(o);
    let advice = crate::ollama_setup::version_fix(server, &host);
    o.ui.fail(format!("{refusal}."));
    let restart_first = advice.starts_with("The ollama on PATH");
    let too_old = advice.starts_with("Upgrade it");
    if !restart_first && !too_old {
        // Too new, a pre-release or a development build: never upgrade.
        o.step_left(advice);
        return false;
    }
    let (question, commands, sudo): (String, Vec<Cmd>, bool) =
        match (o.facts.ollama_service, restart_first) {
            (OllamaService::Brew, true) => (
                format!("{advice} Restart it with `brew services restart ollama`?"),
                vec![Cmd::new("brew", &["services", "restart", "ollama"])],
                false,
            ),
            (OllamaService::Systemd, true) => (
                format!("{advice} Restart it with `sudo systemctl restart ollama`?"),
                vec![Cmd::sudo("systemctl", &["restart", "ollama"])],
                true,
            ),
            (OllamaService::Brew, false) => (
                format!(
                "Ollama {} is too old. Upgrade with `brew upgrade ollama && brew services restart \
                 ollama`?",
                server.unwrap_or_default()
            ),
                vec![
                    Cmd::new("brew", &["upgrade", "ollama"]),
                    Cmd::new("brew", &["services", "restart", "ollama"]),
                ],
                false,
            ),
            (OllamaService::Systemd, false) => (
                format!(
                "Ollama {} is too old. Install 0.40.6 with `{PINNED_LINUX_INSTALL}`? This asks \
                 for your password.",
                server.unwrap_or_default()
            ),
                vec![Cmd::shell(PINNED_LINUX_INSTALL).with_sudo_inside()],
                true,
            ),
            (_, _) => {
                if o.ui.wait_for_enter(&advice) {
                    return true;
                }
                o.step_left(advice);
                return false;
            }
        };
    if !o.ui.consent(&question, true, sudo) {
        o.step_left(advice);
        return false;
    }
    if let Err(error) = o.ui.execute_all(&o.host, &commands).await {
        o.step_left(format!("{error}; {advice}"));
        return false;
    }
    true
}

/// Merge `{"disable_ollama_cloud": true}` into a server.json's text,
/// keeping every other key; `None` when the text is not a JSON object.
pub(crate) fn merged_server_json(existing: Option<&str>) -> Option<String> {
    let mut object = match existing.map(str::trim).filter(|text| !text.is_empty()) {
        None => serde_json::Map::new(),
        Some(text) => match serde_json::from_str::<serde_json::Value>(text).ok()? {
            serde_json::Value::Object(object) => object,
            _ => return None,
        },
    };
    object.insert("disable_ollama_cloud".into(), serde_json::Value::Bool(true));
    Some(format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::Value::Object(object)).ok()?
    ))
}

/// Turn cloud models off where the running server reads it.
async fn cloud_off(o: &mut Onboarding) -> bool {
    match o.facts.ollama_service {
        OllamaService::App => {
            if o.ui
                .wait_for_enter("Turn off Cloud in Ollama > Settings, then press Enter.")
            {
                return true;
            }
            o.step_left("Turn off Cloud in Ollama > Settings (native Sessions require it)");
            false
        }
        OllamaService::Systemd => {
            let drop_in = format!("{}/axocoatl-no-cloud.conf", detect::OLLAMA_DROP_IN_DIR);
            let commands = vec![
                Cmd::sudo("install", &["-d", "-m", "0755", detect::OLLAMA_DROP_IN_DIR]),
                Cmd::sudo("tee", &[&drop_in])
                    .stdin(b"[Service]\nEnvironment=\"OLLAMA_NO_CLOUD=1\"\n"),
                Cmd::sudo("systemctl", &["daemon-reload"]),
                Cmd::sudo("systemctl", &["restart", "ollama"]),
            ];
            if !o.ui.consent(
                &format!(
                    "Native Sessions require Ollama's cloud models off. Turn them off for the \
                     Ollama service by writing {drop_in} with Environment=\"OLLAMA_NO_CLOUD=1\", \
                     then `sudo systemctl daemon-reload && sudo systemctl restart ollama`? This \
                     also turns off Ollama web search; it asks for your password."
                ),
                !o.host.at(&drop_in).exists(),
                true,
            ) {
                o.step_left(format!(
                    "Turn Ollama's cloud models off: {}",
                    ollama_host(o).cloud_off().join(" ")
                ));
                return false;
            }
            if let Err(error) = o.ui.execute_all(&o.host, &commands).await {
                o.step_left(format!("{error}; {}", ollama_host(o).cloud_off().join(" ")));
                return false;
            }
            true
        }
        OllamaService::Brew | OllamaService::Manual => {
            let path = o.host.home.join(".ollama").join("server.json");
            let existing = std::fs::read_to_string(&path).ok();
            let Some(merged) = merged_server_json(existing.as_deref()) else {
                o.step_left(format!(
                    "{} is not a JSON object; add \"disable_ollama_cloud\": true to it and restart \
                     Ollama",
                    path.display()
                ));
                return false;
            };
            let restart = if o.facts.ollama_service == OllamaService::Brew {
                "restarting Ollama (brew services restart ollama)".to_string()
            } else {
                "restarting your Ollama server".to_string()
            };
            if !o.ui.consent(
                &format!(
                    "Native Sessions require Ollama's cloud models off. Turn them off for this \
                     user by adding {{\"disable_ollama_cloud\": true}} to {} and {restart}? This \
                     also turns off Ollama web search.{}",
                    path.display(),
                    if existing.is_some() {
                        " The current file is backed up first."
                    } else {
                        ""
                    }
                ),
                // An existing server.json is an existing setup: Enter
                // leaves it alone.
                existing.is_none(),
                false,
            ) {
                o.step_left(format!(
                    "Turn Ollama's cloud models off: {}",
                    ollama_host(o).cloud_off().join(" ")
                ));
                return false;
            }
            if o.ui.plan() {
                o.ui.say(format!("    would write {}", path.display()));
                return true;
            }
            if let Err(error) = write_server_json(o, &path, existing.as_deref(), &merged) {
                o.step_left(format!("Could not write {}: {error}", path.display()));
                return false;
            }
            if o.facts.ollama_service == OllamaService::Brew {
                let restart = Cmd::new("brew", &["services", "restart", "ollama"]);
                if let Err(error) = o.ui.execute(&o.host, &restart).await {
                    o.step_left(format!(
                        "{error}; restart Ollama: brew services restart ollama"
                    ));
                    return false;
                }
                true
            } else if o.ui.wait_for_enter(&format!(
                "Restart your Ollama server (stop it and start it again) so it reads {}.",
                path.display()
            )) {
                true
            } else {
                o.step_left(format!(
                    "Restart your Ollama server so it reads {}",
                    path.display()
                ));
                false
            }
        }
    }
}

fn write_server_json(
    o: &Onboarding,
    path: &std::path::Path,
    existing: Option<&str>,
    merged: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(existing) = existing {
        let backup = path.with_file_name(format!("server.json.bak-{}", super::host::stamp()));
        std::fs::write(&backup, existing)?;
        o.ui.ok(format!("Backed up to {}", backup.display()));
    }
    std::fs::write(path, merged)?;
    o.host.log(&format!("wrote {}", path.display()));
    o.ui.ok(format!("Wrote {}", path.display()));
    Ok(())
}

/// A choice for the list, labelled with its size and whether it fits.
fn label(model: &ollama_setup::InstalledModel, memory: Option<u64>) -> String {
    let mut label = model.label();
    if let Some(memory) = memory {
        if model.size_bytes() > 0 && !fits_memory(model.size_bytes(), memory) {
            label.push_str(&format!(
                " (too large for this computer's {} GB)",
                axocoatl_isolation::host::memory_gb(memory)
            ));
        }
    }
    label
}

/// One model for `prompt`: an installed candidate (each labelled with its
/// size and whether it fits), the `suggested` model to pull, or a name
/// entered. Only a model that fits this computer is preselected, never
/// `avoid` (the writer, for the reviewer). `None` when no name is entered.
fn choose_one(
    o: &mut Onboarding,
    prompt: &str,
    candidates: &[ollama_setup::InstalledModel],
    suggested: Option<&str>,
    avoid: Option<&str>,
) -> Option<String> {
    let enter = |o: &mut Onboarding| {
        let prompt = match suggested {
            Some(_) => prompt.to_string(),
            None => format!("{prompt}: a model name, or Enter for none"),
        };
        Some(o.ui.input(&prompt, suggested.unwrap_or_default())).filter(|name| !name.is_empty())
    };
    if candidates.is_empty() {
        return enter(o);
    }
    let memory = o.facts.memory_bytes;
    let mut items: Vec<String> = candidates
        .iter()
        .map(|model| label(model, memory))
        .collect();
    let fits = |model: &ollama_setup::InstalledModel| {
        memory
            .is_none_or(|memory| model.size_bytes() == 0 || fits_memory(model.size_bytes(), memory))
    };
    let installed_index = suggested.and_then(|suggested| {
        candidates
            .iter()
            .position(|model| model.name() == suggested && fits(model))
    });
    let pull_index = match (suggested, installed_index) {
        (Some(suggested), None) => {
            items.push(format!(
                "{suggested}{} (pull it)",
                known_size(suggested)
                    .map(|size| format!(" ({})", size_text(size)))
                    .unwrap_or_default()
            ));
            Some(items.len() - 1)
        }
        _ => None,
    };
    items.push("Another model (enter its name)".to_string());
    let other_index = items.len() - 1;
    let default = installed_index
        .or(pull_index)
        .or_else(|| {
            candidates
                .iter()
                .position(|model| fits(model) && Some(model.name()) != avoid)
        })
        .unwrap_or(other_index);
    let index = o.ui.select(prompt, &items, default);
    if index < candidates.len() {
        Some(candidates[index].name().to_string())
    } else if Some(index) == pull_index {
        suggested.map(str::to_string)
    } else {
        enter(o)
    }
}

/// Choose the writer (when `want_writer`) and the reviewer (when
/// `want_reviewer`): from `--writer-model` and `--reviewer`, else from the
/// installed models that fit this computer, else the pair recommended for
/// its memory, each only when it fits this computer (`doctor` calls a larger
/// one too large). The reviewer must differ from the writer. A role left
/// without a model is a step left.
async fn choose_models(
    o: &mut Onboarding,
    server_ready: bool,
    want_writer: bool,
    want_reviewer: bool,
) -> (Option<String>, Option<String>) {
    let installed: Vec<ollama_setup::InstalledModel> = if server_ready {
        o.facts.ollama_server.models().to_vec()
    } else {
        Vec::new()
    };
    let candidates = ollama_setup::candidates(&installed);
    for (name, why) in ollama_setup::refused_models(&installed) {
        o.ui.say(format!("  Not offered: {name}: {why}."));
    }
    let memory = o.facts.memory_bytes;
    let flag = |spec: &Option<String>| {
        spec.as_deref()
            .and_then(|spec| ModelRef::parse(spec).ok())
            .filter(|model| model.provider == Provider::Ollama)
            .map(|model| model.model)
    };
    let flag_writer = flag(&o.args.writer_model);
    let flag_reviewer = flag(&o.args.reviewer);
    let fitting = |model: &str| {
        let size = installed
            .iter()
            .find(|installed| installed.name() == model && installed.size_bytes() > 0)
            .map(|installed| installed.size_bytes())
            .or_else(|| known_size(model));
        match (size, memory) {
            (Some(size), Some(memory)) => fits_memory(size, memory),
            _ => true,
        }
    };
    let recommended = TeamModels::recommended();
    let (lead, reviewer_suggestion, why) = match memory
        .and_then(|memory| ollama_setup::suggest_fitting_team_models(&installed, memory))
        .filter(|team| team.lead != team.reviewer)
    {
        Some(team) => (
            Some(team.lead),
            Some(team.reviewer),
            "from the models on this server that fit this computer",
        ),
        None => match memory.and_then(ollama_setup::recommended_pair) {
            Some((team, why)) => (Some(team.lead), Some(team.reviewer), why),
            // Below the tested sizes: only a model that fits is suggested.
            None => (
                fitting(&recommended.lead).then(|| recommended.lead.clone()),
                fitting(&recommended.reviewer).then(|| recommended.reviewer.clone()),
                "not tested at this memory size",
            ),
        },
    };
    let asks_writer = want_writer && flag_writer.is_none();
    let asks_reviewer = want_reviewer && flag_reviewer.is_none();
    let mut suggested = Vec::new();
    let mut unfit = Vec::new();
    for (asks, role, model, tested) in [
        (asks_writer, "writer", &lead, &recommended.lead),
        (
            asks_reviewer,
            "reviewer",
            &reviewer_suggestion,
            &recommended.reviewer,
        ),
    ] {
        match (asks, model) {
            (true, Some(model)) => suggested.push(format!("{role} {model}")),
            (true, None) => unfit.push(described(tested)),
            (false, _) => {}
        }
    }
    if !suggested.is_empty() {
        o.ui.say(format!("  Suggested: {} ({why}).", suggested.join(", ")));
    }
    if !unfit.is_empty() {
        let gb = memory
            .map(axocoatl_isolation::host::memory_gb)
            .unwrap_or_default();
        o.ui.say(format!(
            "  Not suggested: {} {} too large for this computer's {gb} GB. Enter a smaller \
             model's name to try one (not tested here), or leave the name empty and choose a \
             hosted writer and reviewer later (`axocoatl onboard --writer openrouter`, or \
             claude-code, or codex).",
            unfit.join(" and "),
            if unfit.len() == 1 { "is" } else { "are" }
        ));
    }
    let writer = if want_writer {
        match flag_writer {
            Some(model) => Some(model),
            None => {
                let chosen = choose_one(
                    o,
                    "Model for the writer (Lead; Scout and the chat Assistant use it too)",
                    &candidates,
                    lead.as_deref(),
                    None,
                );
                if chosen.is_none() {
                    o.step_left(
                        "No writer model was chosen: name one with `axocoatl onboard \
                         --writer-model ollama:MODEL`, or choose a hosted writer with `axocoatl \
                         onboard --writer openrouter` (or claude-code, or codex)",
                    );
                    return (None, None);
                }
                chosen
            }
        }
    } else {
        None
    };
    if !want_reviewer {
        return (writer, None);
    }
    let reviewer_default = if writer.is_some() && writer == reviewer_suggestion {
        lead.clone()
    } else {
        reviewer_suggestion.clone()
    };
    let reviewer = match flag_reviewer {
        Some(model) => Some(model),
        None => choose_one(
            o,
            "Model for the reviewer (a different model that only reads)",
            &candidates,
            reviewer_default.as_deref(),
            writer.as_deref(),
        ),
    };
    let Some(mut reviewer) = reviewer else {
        o.step_left(
            "No reviewer model was chosen: name one with `axocoatl onboard --reviewer \
             ollama:MODEL`, or `--reviewer openrouter:VENDOR/MODEL` for a hosted one",
        );
        return (writer, None);
    };
    if writer.as_deref() == Some(reviewer.as_str()) {
        o.ui.warn(format!(
            "The reviewer must be a different model from the writer ({reviewer}): a second model \
             checks the writer's change"
        ));
        if o.ui.interactive() {
            reviewer = choose_one(
                o,
                "Model for the reviewer (a different model)",
                &candidates,
                reviewer_default.as_deref(),
                writer.as_deref(),
            )
            .unwrap_or_default();
        }
        if writer.as_deref() == Some(reviewer.as_str()) || reviewer.is_empty() {
            o.step_left(
                "Choose a reviewer model different from the writer: `axocoatl onboard --reviewer \
                 ollama:MODEL`",
            );
            return (writer, None);
        }
    }
    (writer, Some(reviewer))
}

/// Pull the models that are not installed, with their sizes and this
/// computer's free disk shown, checking each pull's exit status and that it
/// gave a GGUF model. `false` when a model is still missing.
async fn pull(o: &mut Onboarding, base_url: &str, models: &[String], writer: WriterChoice) -> bool {
    let installed = o.facts.ollama_server.models().to_vec();
    let missing: Vec<String> = models
        .iter()
        .filter(|model| !ollama_setup::is_installed(model, &installed))
        .cloned()
        .collect();
    if missing.is_empty() {
        for model in models {
            o.ui.ok(format!("{model} is installed"));
        }
        return true;
    }
    let sizes: Vec<Option<u64>> = missing.iter().map(|model| known_size(model)).collect();
    let described: Vec<String> = missing
        .iter()
        .zip(&sizes)
        .map(|(model, size)| match size {
            Some(size) => format!("{model} ({})", size_text(*size)),
            None => format!("{model} (size not known before the download)"),
        })
        .collect();
    let total: u64 = sizes.iter().flatten().sum();
    let all_known = sizes.iter().all(Option::is_some);
    let free = o.facts.disk_free_bytes;
    // The download plus a tenth for Ollama's temporary files.
    let needed = total + total / 10;
    let disk = match (all_known, free) {
        (true, Some(free)) => format!(
            " Needs {} free; {} free here.",
            size_text(needed),
            size_text(free)
        ),
        (true, None) => format!(" Needs {} free.", size_text(needed)),
        (false, _) => String::new(),
    };
    let fits_disk = free.is_none_or(|free| free >= needed);
    // As `doctor` judges a model's size against this computer's memory.
    let unfit: Vec<&String> = missing
        .iter()
        .filter(|model| too_large(o, model).is_some())
        .collect();
    let fits_ram = unfit.is_empty();
    let ram = match (unfit.as_slice(), o.facts.memory_bytes) {
        ([], _) | (_, None) => String::new(),
        (models, Some(memory)) => format!(
            " {} {} too large for this computer's {} GB of memory: a model that does not fit \
             loads slowly or not at all.",
            models
                .iter()
                .map(|model| model.as_str())
                .collect::<Vec<_>>()
                .join(" and "),
            if models.len() == 1 { "is" } else { "are" },
            axocoatl_isolation::host::memory_gb(memory)
        ),
    };
    if !fits_disk {
        o.ui.warn(format!(
            "There is not enough free disk for these models.{disk}"
        ));
    }
    let question = format!("Pull {}?{ram}{disk}", described.join(" and "));
    if !fits_disk && !o.ui.interactive() && o.args.pull() != Some(false) {
        // Only a person at the terminal may start a download that does not
        // fit this disk.
        o.ui.say(format!("  {question} no: it does not fit this disk"));
        for model in &missing {
            let step = match too_large(o, model) {
                Some(_) => pull_step(o, model, "", writer),
                None => format!("Free disk space for {model}, then pull it: ollama pull {model}"),
            };
            o.step_left(step);
        }
        return false;
    }
    if !fits_ram && !o.ui.interactive() && o.args.pull().is_none() {
        // Nor one too large for this computer's memory, unless --pull says
        // to.
        o.ui.say(format!(
            "  {question} no: it does not fit this computer's memory (--pull pulls it anyway)"
        ));
        for model in &missing {
            let step = pull_step(o, model, "", writer);
            o.step_left(step);
        }
        return false;
    }
    let pull_now = match o.args.pull() {
        Some(choice) => {
            o.ui.say(format!(
                "  {question} {}",
                if choice {
                    "yes (--pull)"
                } else {
                    "no (--no-pull)"
                }
            ));
            choice
        }
        None => {
            o.ui.consent(&question, all_known && fits_disk && fits_ram, false)
        }
    };
    if !pull_now {
        for model in &missing {
            let step = pull_step(o, model, "", writer);
            o.step_left(step);
        }
        return false;
    }
    let mut ok = true;
    for model in &missing {
        let cmd = Cmd::new("ollama", &["pull", model]).env("OLLAMA_HOST", base_url);
        if o.ui.plan() {
            o.ui.say(format!("    would run: {}", cmd.display()));
            continue;
        }
        loop {
            o.ui.say(format!("  $ ollama pull {model}"));
            match o.host.run_keeping_tail(&cmd).await {
                Ok(()) => break,
                Err((error, why)) => {
                    o.ui.fail(format!(
                        "ollama pull {model} failed: {error}{}",
                        if why.is_empty() {
                            String::new()
                        } else {
                            format!(": {why}")
                        }
                    ));
                    let retry = o.ui.interactive()
                        && o.ui.select(
                            "What next?",
                            &["Retry the pull".to_string(), "Skip it".to_string()],
                            0,
                        ) == 0;
                    if !retry {
                        o.steps.push(format!("Pull {model}: ollama pull {model}"));
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok {
            continue;
        }
        let finding = ollama_setup::model_finding(base_url, model, false).await;
        if finding.ok {
            o.ui.ok(format!(
                "{model} is a local GGUF model native Sessions can run"
            ));
            continue;
        }
        o.ui.fail(&finding.label);
        let gguf = Cmd::new("ollama", &["pull", "--runner", "llamacpp", model])
            .env("OLLAMA_HOST", base_url);
        if o.ui.consent(
            &format!(
                "Ollama pulled a variant native Sessions refuse. Pull its GGUF variant with \
                 `ollama pull --runner llamacpp {model}` (an undocumented Ollama flag)?"
            ),
            true,
            false,
        ) && o.host.run(&gguf).await.is_ok()
            && ollama_setup::model_finding(base_url, model, false).await.ok
        {
            o.ui.ok(format!("{model} is now the GGUF variant"));
        } else {
            o.step_left(format!(
                "{model} is not a GGUF model native Sessions can run: ollama pull --runner \
                 llamacpp {model}, or choose another model"
            ));
            ok = false;
        }
    }
    if !o.ui.plan() {
        let url = base_url.to_string();
        o.facts.ollama_server = OllamaServer::probe(&url).await;
    }
    ok
}

/// Load each model once and read its context; give the service
/// `OLLAMA_CONTEXT_LENGTH` when one is below [`MIN_CONTEXT_TOKENS`].
/// Returns the models read with enough context and their context, which
/// Step 5's check uses instead of loading them again.
async fn context(
    o: &mut Onboarding,
    base_url: &str,
    models: &[String],
    writer: WriterChoice,
) -> Vec<(String, usize)> {
    let mut read = Vec::new();
    if o.ui.plan() {
        o.ui.say("  would load each model once to read its context and run the contract probes");
        return read;
    }
    let mut low = Vec::new();
    for model in models {
        o.ui.say(format!("  Loading {model} once to read its context…"));
        match ollama_setup::observed_context(base_url, model).await {
            Ok((tokens, version)) if tokens >= MIN_CONTEXT_TOKENS => {
                let unaudited = if axocoatl_llm_ollama::ollama_version_unaudited(&version) {
                    format!("; Ollama {version} is newer than the audited 0.40.6, and the contract probes passed on {model}")
                } else {
                    ", contract probes passed".to_string()
                };
                o.ui.ok(format!("{model}: {tokens}-token context{unaudited}"));
                read.push((model.clone(), tokens));
            }
            Ok((tokens, _)) => low.push((model.clone(), tokens)),
            Err(error) => o.step_left(format!("{model} cannot run in native Sessions: {error}")),
        }
    }
    if low.is_empty() {
        return read;
    }
    let described: Vec<String> = low
        .iter()
        .map(|(model, tokens)| format!("{model} with a {tokens}-token context"))
        .collect();
    let problem = format!(
        "Ollama loads {} on this computer; a fix run needs at least {MIN_CONTEXT_TOKENS}.",
        described.join(" and ")
    );
    if set_context(o, &problem).await {
        let mut still = Vec::new();
        for (model, _) in &low {
            match ollama_setup::observed_context(base_url, model).await {
                Ok((tokens, _)) if tokens >= MIN_CONTEXT_TOKENS => {
                    o.ui.ok(format!(
                        "{model}: {tokens}-token context, contract probes passed"
                    ));
                    read.push((model.clone(), tokens));
                }
                Ok((tokens, _)) => still.push(format!("{model} still loads with {tokens} tokens")),
                Err(error) => still.push(format!("{model}: {error}")),
            }
        }
        if still.is_empty() {
            return read;
        }
        let hosted = hosted_instead(o, &low, writer);
        o.step_left(format!(
            "{}: Ollama may not have the memory for a {CONTEXT_FIX_TOKENS}-token context here; \
             use OpenRouter {hosted}",
            still.join("; ")
        ));
    } else {
        let hosted = hosted_instead(o, &low, writer);
        o.step_left(format!(
            "{problem} Give Ollama a {CONTEXT_FIX_TOKENS}-token context: {}; or use OpenRouter \
             {hosted}",
            ollama_host(o).context()
        ));
    }
    read
}

/// `for these models (`axocoatl onboard … --writer openrouter`)`, or for
/// the reviewer of Claude Code or Codex: the command that moves the models
/// in `low` to OpenRouter and keeps `writer` ([`Onboarding::model_change`]).
pub(super) fn hosted_instead(
    o: &Onboarding,
    low: &[(String, usize)],
    writer: WriterChoice,
) -> String {
    let models: Vec<&str> = low.iter().map(|(model, _)| model.as_str()).collect();
    let (mode, because) = o.model_change(&models);
    match writer {
        WriterChoice::ClaudeCode | WriterChoice::Codex => format!(
            "for the reviewer (`axocoatl onboard {mode} --writer {} --reviewer openrouter:{}`){because}",
            writer.id(),
            super::openrouter::DEFAULT_REVIEWER
        ),
        WriterChoice::Ollama | WriterChoice::Openrouter => format!(
            "for these models (`axocoatl onboard {mode} --writer openrouter`){because}"
        ),
    }
}

/// Homebrew's `brew services --help` says it reads a per-formula
/// environment file (`$HOMEBREW_USER_CONFIG_HOME/services/formula.env`,
/// with `formula` underlined at a terminal; older releases wrote
/// `<formula>`). Releases without that sentence ignore the file.
pub(crate) fn brew_services_read_env_files(help: &str) -> bool {
    let plain = strip_ansi(help);
    plain.split_whitespace().any(|word| {
        let word = word.trim_matches(|c: char| matches!(c, '`' | '(' | ')' | ',' | '.' | ';'));
        word.contains("services/") && word.contains("formula") && word.ends_with(".env")
    })
}

/// `text` without terminal escape sequences (`ESC [ … letter`).
fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        plain.push(c);
    }
    plain
}

/// `OLLAMA_CONTEXT_LENGTH` in an env file's text, replaced or added.
pub(crate) fn with_context_length(existing: Option<&str>) -> String {
    let mut lines: Vec<String> = existing
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim_start().starts_with("OLLAMA_CONTEXT_LENGTH="))
        .map(str::to_string)
        .collect();
    lines.push(format!("OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS}"));
    format!("{}\n", lines.join("\n"))
}

/// Ask to give the service a larger context and restart it.
async fn set_context(o: &mut Onboarding, problem: &str) -> bool {
    match o.facts.ollama_service {
        OllamaService::Brew => {
            let supported = o
                .host
                .probe("brew", &["services", "--help"])
                .await
                .is_some_and(|probe| brew_services_read_env_files(&probe.stdout));
            if !supported {
                return false;
            }
            let path = brew_env_file(o);
            let existing = std::fs::read_to_string(&path).ok();
            if !o.ui.consent(
                &format!(
                    "{problem} Set OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} for the Ollama \
                     service in {} and restart it (`brew services restart ollama`)?",
                    path.display()
                ),
                // An existing service environment is an existing setup.
                existing.is_none(),
                false,
            ) {
                return false;
            }
            let text = with_context_length(existing.as_deref());
            let written = path
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|()| std::fs::write(&path, text))
                .and_then(|()| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    }
                    #[cfg(not(unix))]
                    {
                        Ok(())
                    }
                });
            if let Err(error) = written {
                o.ui.fail(format!("Could not write {}: {error}", path.display()));
                return false;
            }
            o.host.log(&format!("wrote {}", path.display()));
            let restart = Cmd::new("brew", &["services", "restart", "ollama"]);
            o.ui.execute(&o.host, &restart).await.is_ok() && wait_until(o, cloud_disabled).await
        }
        OllamaService::Systemd => {
            let drop_in = format!("{}/axocoatl-context.conf", detect::OLLAMA_DROP_IN_DIR);
            let content =
                format!("[Service]\nEnvironment=\"OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS}\"\n");
            let commands = vec![
                Cmd::sudo("install", &["-d", "-m", "0755", detect::OLLAMA_DROP_IN_DIR]),
                Cmd::sudo("tee", &[&drop_in]).stdin(content.as_bytes()),
                Cmd::sudo("systemctl", &["daemon-reload"]),
                Cmd::sudo("systemctl", &["restart", "ollama"]),
            ];
            if !o.ui.consent(
                &format!(
                    "{problem} Set OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} for the Ollama \
                     service in {drop_in} and restart it? This asks for your password."
                ),
                !o.host.at(&drop_in).exists(),
                true,
            ) {
                return false;
            }
            o.ui.execute_all(&o.host, &commands).await.is_ok()
                && wait_until(o, cloud_disabled).await
        }
        OllamaService::App | OllamaService::Manual => {
            let instruction = format!("{problem} To fix it, {}.", ollama_host(o).context());
            o.ui.wait_for_enter(&instruction) && wait_until(o, reachable).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_json_keeps_other_keys() {
        assert_eq!(
            merged_server_json(None).unwrap(),
            "{\n  \"disable_ollama_cloud\": true\n}\n"
        );
        let merged =
            merged_server_json(Some(r#"{"other": 1, "disable_ollama_cloud": false}"#)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["other"], 1);
        assert_eq!(value["disable_ollama_cloud"], true);
        assert_eq!(merged_server_json(Some("[1]")), None);
        assert_eq!(merged_server_json(Some("not json")), None);
    }

    /// The sentence of Homebrew 7.0.10's `brew services --help`, verbatim,
    /// piped and at a terminal (where `formula` is underlined), and the
    /// `<formula>` form of older 4.x releases. A Homebrew without it does
    /// not read the file.
    #[test]
    fn homebrew_service_environment_files_are_recognised_in_real_help_text() {
        let piped = "Environment variables can be added or overridden for a service by creating\n\
                     $HOMEBREW_USER_CONFIG_HOME/services/formula.env (defaults to\n\
                     ~/.homebrew/services/formula.env). The file uses KEY=value format, one per\n\
                     line; lines starting with # are comments. Changes take effect on the next\n\
                     brew services restart and persist across upgrades.\n";
        assert!(brew_services_read_env_files(piped));
        let terminal =
            "creating\n$HOMEBREW_USER_CONFIG_HOME/services/\u{1b}[4mformula\u{1b}[0m.env \
                        (defaults to\n~/.homebrew/services/\u{1b}[4mformula\u{1b}[0m.env).";
        assert!(brew_services_read_env_files(terminal));
        assert!(brew_services_read_env_files(
            "Environment variables can be added in `$HOMEBREW_USER_CONFIG_HOME/services/<formula>.env`"
        ));
        assert!(!brew_services_read_env_files(
            "Usage: brew services [subcommand]\n\nManage background services with macOS' \
             launchctl(1) daemon manager or Linux's\nsystemctl(1) service manager.\n"
        ));
    }

    #[test]
    fn the_context_length_is_set_once_in_the_service_environment() {
        assert_eq!(with_context_length(None), "OLLAMA_CONTEXT_LENGTH=32768\n");
        assert_eq!(
            with_context_length(Some(
                "OLLAMA_HOST=127.0.0.1:11440\nOLLAMA_CONTEXT_LENGTH=4096\n"
            )),
            "OLLAMA_HOST=127.0.0.1:11440\nOLLAMA_CONTEXT_LENGTH=32768\n"
        );
    }
}
