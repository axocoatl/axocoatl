//! Ollama checks and model defaults for `onboard` and `doctor`.
//!
//! Native Sessions accept only a loopback Ollama server whose version the
//! version policy accepts (Ollama 0.40.6 or later: a later 0.40 release, or a
//! later 0.x release once the contract probes pass on each model) and that
//! reports its cloud features disabled, and only GGUF models that Ollama runs
//! in llama.cpp's llama-server; `axocoatl_llm_ollama` owns that contract and
//! this module reports it. Setup also reads the models installed on the
//! chosen server to suggest the default team's models for this computer's
//! memory, and what to do about each finding depends on how Ollama runs here
//! ([`OllamaService`]).

use axocoatl_llm_ollama::{
    check_native_ollama_server, check_shown_model, ollama_version_note, ollama_version_refusal,
    show_native_ollama_model, validate_native_ollama_endpoint, NativeOllamaServerCheck,
    OllamaCloudMode, AUDITED_OLLAMA_VERSION,
};
use serde_json::Value;

use crate::onboard::detect::OllamaService;

/// The Ollama server setup offers first.
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// The coding model setup suggests for Lead when the server has no local
/// model installed (19 GB; it calls tools).
pub const RECOMMENDED_LEAD_MODEL: &str = "qwen3-coder:30b";

/// The model setup suggests for Reviewer when the server has no local model
/// installed: a different model from Lead's, so a second model checks Lead's
/// work (14 GB; it calls tools and thinks). Both fit a 48 GB Mac, one at a
/// time.
pub const RECOMMENDED_REVIEWER_MODEL: &str = "gpt-oss:20b";

/// The reviewer for a computer with 96 GB of memory or more: the measured
/// one (65.4 GB).
pub const LARGE_REVIEWER_MODEL: &str = "gpt-oss:120b";

/// The flag of `ollama pull` that selects the GGUF variant of a model whose
/// tag also has an MLX variant. Ollama does not document it.
pub const LLAMACPP_PULL_FLAG: &str = "--runner llamacpp";

/// The smallest context, in tokens, a fix run's first writer call fits in
/// (it needs about 9.7k); below it native Sessions and runs refuse the model
/// up front instead of failing after the first call.
pub const MIN_CONTEXT_TOKENS: usize = 16_384;

/// The context onboarding sets with `OLLAMA_CONTEXT_LENGTH` when Ollama
/// loads a model with less than [`MIN_CONTEXT_TOKENS`].
pub const CONTEXT_FIX_TOKENS: usize = 32_768;

/// The download size of the models setup suggests, as the Ollama library
/// lists them.
const KNOWN_SIZES: &[(&str, u64)] = &[
    (RECOMMENDED_LEAD_MODEL, 18_556_700_761),
    (RECOMMENDED_REVIEWER_MODEL, 13_780_173_839),
    (LARGE_REVIEWER_MODEL, 65_369_818_941),
];

/// The download size of `model` when setup knows it.
pub fn known_size(model: &str) -> Option<u64> {
    KNOWN_SIZES
        .iter()
        .find(|(name, _)| *name == model)
        .map(|(_, size)| *size)
}

/// A model of `size_bytes` leaves room for the rest of the computer (macOS,
/// the Podman machine and Ollama's own buffers) when it is at most about
/// 0.65 of the memory.
pub fn fits_memory(size_bytes: u64, memory_bytes: u64) -> bool {
    size_bytes as f64 <= memory_bytes as f64 * 0.65
}

/// `18.6 GB`.
pub fn size_text(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// The local pair setup recommends for a computer with `memory_bytes`, and
/// what is known about it; `None` below 48 GB, where no local pair has been
/// tested.
pub fn recommended_pair(memory_bytes: u64) -> Option<(TeamModels, &'static str)> {
    let gb = axocoatl_isolation::host::memory_gb(memory_bytes);
    if gb >= 96 {
        Some((
            TeamModels {
                lead: RECOMMENDED_LEAD_MODEL.to_string(),
                reviewer: LARGE_REVIEWER_MODEL.to_string(),
            },
            "recommended: gpt-oss 120B is the reviewer Axocoatl has measured",
        ))
    } else if gb >= 48 {
        Some((
            TeamModels::recommended(),
            "runs end to end on 48 GB; this reviewer's quality has not been measured",
        ))
    } else {
        None
    }
}

/// The models the default team runs on. Lead, Scout and the chat Assistant
/// use `lead`; Reviewer uses `reviewer`, which may be the same model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamModels {
    pub lead: String,
    pub reviewer: String,
}

impl TeamModels {
    /// The pair setup suggests when nothing suitable is installed: two
    /// different models, so review is cross-model by default.
    pub fn recommended() -> Self {
        Self {
            lead: RECOMMENDED_LEAD_MODEL.to_string(),
            reviewer: RECOMMENDED_REVIEWER_MODEL.to_string(),
        }
    }
}

/// A model `GET /api/tags` lists.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledModel {
    pub name: String,
    size_bytes: u64,
    /// `details.parameter_size` as reported, e.g. `30.5B`.
    parameter_size: String,
    families: Vec<String>,
    remote: bool,
    /// `details.format` as reported (`gguf`, `safetensors`), lower case.
    format: String,
    /// `details.runner` as reported (`ggml`, `llamacpp`, `mlx`), lower case.
    runner: String,
}

impl InstalledModel {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    fn from_tag(record: &Value) -> Option<Self> {
        let name = record.get("name").and_then(Value::as_str)?.to_string();
        let text = |pointer: &str| {
            record
                .pointer(pointer)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let mut families: Vec<String> = record
            .pointer("/details/families")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_ascii_lowercase)
            .collect();
        families.push(text("/details/family").to_ascii_lowercase());
        let remote = ["remote_host", "remote_model"].iter().any(|key| {
            record
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
        });
        Some(Self {
            remote,
            size_bytes: record.get("size").and_then(Value::as_u64).unwrap_or(0),
            parameter_size: text("/details/parameter_size"),
            families,
            format: text("/details/format").to_ascii_lowercase(),
            runner: text("/details/runner").to_ascii_lowercase(),
            name,
        })
    }

    /// The name without registry, namespace or tag, in lower case.
    fn base(&self) -> String {
        let without_tag = match self.name.rsplit_once(':') {
            Some((base, tag)) if !tag.contains('/') => base,
            _ => self.name.as_str(),
        };
        without_tag
            .rsplit('/')
            .next()
            .unwrap_or(without_tag)
            .to_ascii_lowercase()
    }

    fn tag(&self) -> &str {
        match self.name.rsplit_once(':') {
            Some((_, tag)) if !tag.contains('/') => tag,
            _ => "latest",
        }
    }

    /// Relative size for ranking: the parameter count, else an estimate from
    /// the file size at about four bits per weight.
    fn scale(&self) -> f64 {
        parse_parameters(&self.parameter_size).unwrap_or(self.size_bytes as f64 * 2.0)
    }

    /// Runs on Ollama's cloud rather than on this machine.
    fn is_remote(&self) -> bool {
        self.remote || self.tag() == "cloud" || self.tag().ends_with("-cloud")
    }

    fn is_embedding(&self) -> bool {
        self.base().contains("embed") || self.families.iter().any(|f| f.contains("bert"))
    }

    fn is_vision(&self) -> bool {
        let base = self.base();
        ["vl", "llava", "vision", "moondream"]
            .iter()
            .any(|marker| base.contains(marker))
            || self
                .families
                .iter()
                .any(|f| f.contains("clip") || f.contains("mllama"))
    }

    fn is_coding(&self) -> bool {
        let base = self.base();
        base.contains("code") || base.contains("devstral")
    }

    /// Why native Sessions refuse this model whatever it is used for, in
    /// words; `None` when they may run it. A server that does not report a
    /// format (an older one) is not refused here: the Session checks again.
    pub fn refusal(&self) -> Option<String> {
        if self.is_remote() {
            return Some("a cloud model, which runs on ollama.com".into());
        }
        if self.runner == "mlx" || (!self.format.is_empty() && self.format != "gguf") {
            return Some(format!(
                "{} run by Ollama's MLX runner, which does not hold num_ctx as a hard limit; \
                 native Sessions run only GGUF models (try `ollama pull {LLAMACPP_PULL_FLAG} {}`, \
                 an undocumented Ollama flag)",
                if self.format.is_empty() {
                    "a model".to_string()
                } else {
                    format!("a {} model", self.format)
                },
                self.name
            ));
        }
        None
    }

    /// May be offered as a default team model: it runs locally as GGUF and
    /// generates text. Native Sessions refuse a cloud or MLX model.
    pub fn is_candidate(&self) -> bool {
        self.refusal().is_none() && !self.is_embedding()
    }

    /// The model as a choice in a list.
    pub fn label(&self) -> String {
        let mut details = Vec::new();
        if !self.parameter_size.is_empty() {
            details.push(self.parameter_size.clone());
        }
        if self.size_bytes > 0 {
            details.push(format!("{:.1} GB", self.size_bytes as f64 / 1e9));
        }
        if details.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, details.join(", "))
        }
    }
}

/// `30.5B`, `137M` or `1.2T` as a count.
fn parse_parameters(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.len() < 2 || !text.is_ascii() {
        return None;
    }
    let (number, unit) = text.split_at(text.len() - 1);
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "K" => 1e3,
        "M" => 1e6,
        "B" => 1e9,
        "T" => 1e12,
        _ => return None,
    };
    let value: f64 = number.trim().parse().ok()?;
    (value.is_finite() && value > 0.0).then_some(value * multiplier)
}

/// The models in a `GET /api/tags` reply, in the server's order.
pub fn parse_tags(tags: &Value) -> Vec<InstalledModel> {
    tags.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(InstalledModel::from_tag)
        .collect()
}

/// The installed local models native Sessions refuse, each with why, for
/// setup to say why they are not offered. Cloud models are left out: they
/// are not on this machine.
pub fn refused_models(models: &[InstalledModel]) -> Vec<(String, String)> {
    models
        .iter()
        .filter(|model| !model.is_remote() && !model.is_embedding())
        .filter_map(|model| model.refusal().map(|why| (model.name.clone(), why)))
        .collect()
}

/// The models offered as team choices, sorted by name.
pub fn candidates(models: &[InstalledModel]) -> Vec<InstalledModel> {
    let mut candidates: Vec<_> = models
        .iter()
        .filter(|m| m.is_candidate())
        .cloned()
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    candidates
}

/// List the models installed on an Ollama server.
pub async fn installed_models(base_url: &str) -> Result<Vec<InstalledModel>, String> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?;
    let tags: Value = response.json().await.map_err(|e| e.to_string())?;
    Ok(parse_tags(&tags))
}

/// Suggest the default team's models from what the server has installed.
///
/// Lead gets a coding model when one is installed, preferring Qwen3-Coder,
/// then the largest; without one, the largest text model. Reviewer gets a
/// different model so a second model checks Lead's work: gpt-oss of any size
/// when one is installed (the largest), otherwise a different, larger model;
/// without one it shares Lead's model. Cloud, MLX and embedding models are
/// never suggested, and vision models only when nothing else fits. `None`
/// when the server has no candidate ([`TeamModels::recommended`] then).
#[cfg(test)]
pub fn suggest_team_models(models: &[InstalledModel]) -> Option<TeamModels> {
    suggest_from(candidates(models))
}

/// [`suggest_team_models`] among the installed models that fit a computer
/// with `memory_bytes` ([`fits_memory`]): a model too large for it is never
/// preselected.
pub fn suggest_fitting_team_models(
    models: &[InstalledModel],
    memory_bytes: u64,
) -> Option<TeamModels> {
    suggest_from(
        candidates(models)
            .into_iter()
            .filter(|model| model.size_bytes == 0 || fits_memory(model.size_bytes, memory_bytes))
            .collect(),
    )
}

fn suggest_from(candidates: Vec<InstalledModel>) -> Option<TeamModels> {
    let lead = candidates.iter().min_by(|a, b| {
        lead_tier(a)
            .cmp(&lead_tier(b))
            .then(b.scale().total_cmp(&a.scale()))
            .then(a.name.cmp(&b.name))
    })?;
    let reviewer = candidates
        .iter()
        .filter(|m| m.name != lead.name && (reviewer_tier(m) == 0 || m.scale() > lead.scale()))
        .min_by(|a, b| {
            reviewer_tier(a)
                .cmp(&reviewer_tier(b))
                .then(b.scale().total_cmp(&a.scale()))
                .then(a.name.cmp(&b.name))
        })
        .unwrap_or(lead);
    Some(TeamModels {
        lead: lead.name.clone(),
        reviewer: reviewer.name.clone(),
    })
}

fn lead_tier(model: &InstalledModel) -> u8 {
    match (model.is_vision(), model.is_coding()) {
        (false, true) if model.base().starts_with("qwen3-coder") => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, _) => 3,
    }
}

fn reviewer_tier(model: &InstalledModel) -> u8 {
    match (model.is_vision(), model.is_coding()) {
        (false, _) if model.base().starts_with("gpt-oss") => 0,
        (false, false) => 1,
        (false, true) => 2,
        (true, _) => 3,
    }
}

/// `name` is installed under that exact name, or as `name:latest` when it
/// has no tag.
pub fn is_installed(name: &str, models: &[InstalledModel]) -> bool {
    models.iter().any(|model| {
        model.name == name || (!name.contains(':') && model.name == format!("{name}:latest"))
    })
}

/// What setup learned about an Ollama server.
#[derive(Debug)]
pub enum ServerProbe {
    /// `GET /api/tags` failed.
    Unreachable(String),
    Reachable {
        models: Vec<InstalledModel>,
        /// The native requirements, or why they could not be read.
        check: Result<NativeOllamaServerCheck, String>,
    },
}

/// List the server's models, then read its native requirements.
pub async fn probe_server(base_url: &str) -> ServerProbe {
    match installed_models(base_url).await {
        Err(error) => ServerProbe::Unreachable(error),
        Ok(models) => ServerProbe::Reachable {
            models,
            check: native_check(base_url).await,
        },
    }
}

/// The native requirements of a reachable server. An endpoint native
/// execution refuses is reported without a request.
pub async fn native_check(base_url: &str) -> Result<NativeOllamaServerCheck, String> {
    check_native_ollama_server(base_url)
        .await
        .map_err(|error| error.to_string())
}

/// One line of a check: a passed requirement (possibly with a warning), or a
/// failed one with what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub ok: bool,
    /// Passed, with something to know (an unaudited Ollama version).
    pub warn: bool,
    pub label: String,
    pub hint: Vec<String>,
}

impl Finding {
    fn pass(label: String) -> Self {
        Self {
            ok: true,
            warn: false,
            label,
            hint: Vec::new(),
        }
    }

    fn warning(label: String, hint: Vec<String>) -> Self {
        Self {
            ok: true,
            warn: true,
            label,
            hint,
        }
    }

    fn fail(label: String, hint: Vec<String>) -> Self {
        Self {
            ok: false,
            warn: false,
            label,
            hint,
        }
    }
}

/// How Ollama runs here, for the commands a hint gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaHost {
    pub service: OllamaService,
    pub macos: bool,
    /// Homebrew's service environment file (`~/.homebrew/services/ollama.env`).
    pub brew_env_file: Option<std::path::PathBuf>,
    /// The `ollama` CLI's own version, when one is installed.
    pub cli_version: Option<String>,
}

impl OllamaHost {
    /// Restart the server so it reads its settings again.
    pub fn restart(&self) -> String {
        match self.service {
            OllamaService::Brew => "brew services restart ollama".to_string(),
            OllamaService::Systemd => "sudo systemctl restart ollama".to_string(),
            OllamaService::App => "quit and reopen the Ollama app".to_string(),
            OllamaService::Manual => "stop and start your `ollama serve` again".to_string(),
        }
    }

    /// Start a server that is not running.
    pub fn start(&self) -> String {
        match (self.service, self.cli_version.is_some(), self.macos) {
            (OllamaService::Brew, _, _) => "brew services start ollama".to_string(),
            (OllamaService::Systemd, _, _) => "sudo systemctl start ollama".to_string(),
            (OllamaService::App, _, _) => "open the Ollama app".to_string(),
            (OllamaService::Manual, true, _) => {
                "run `OLLAMA_NO_CLOUD=1 ollama serve` in another terminal".to_string()
            }
            (OllamaService::Manual, false, true) => {
                "brew install ollama && brew services start ollama (or install the app from \
                 https://ollama.com/download)"
                    .to_string()
            }
            (OllamaService::Manual, false, false) => PINNED_LINUX_INSTALL.to_string(),
        }
    }

    /// Install the audited release, for a version that is refused as too
    /// new or not a release (never `brew upgrade`).
    pub fn pinned_install(&self) -> String {
        if self.macos {
            format!(
                "install Ollama {AUDITED_OLLAMA_VERSION} from \
                 https://github.com/ollama/ollama/releases/tag/v{AUDITED_OLLAMA_VERSION}"
            )
        } else {
            PINNED_LINUX_INSTALL.to_string()
        }
    }

    /// Upgrade an Ollama that is too old.
    pub fn upgrade(&self) -> String {
        match (self.service, self.macos) {
            (OllamaService::Brew, _) => {
                "brew upgrade ollama && brew services restart ollama".to_string()
            }
            (OllamaService::App, _) => {
                "update the Ollama app (download it again from https://ollama.com/download), \
                 then reopen it"
                    .to_string()
            }
            (_, true) => "brew upgrade ollama (or download https://ollama.com/download), then \
                          restart `ollama serve`"
                .to_string(),
            (_, false) => PINNED_LINUX_INSTALL.to_string(),
        }
    }

    /// Turn the cloud features off.
    pub fn cloud_off(&self) -> Vec<String> {
        let json = r#"{"disable_ollama_cloud": true}"#;
        match self.service {
            OllamaService::Brew => vec![format!(
                "Run `axocoatl onboard` (it turns them off after asking), or add {json} to \
                 ~/.ollama/server.json and run `brew services restart ollama`."
            )],
            OllamaService::App => vec![
                "In the Ollama app, turn off Cloud in Settings, then quit and reopen Ollama."
                    .to_string(),
            ],
            OllamaService::Systemd => vec![
                "Run `axocoatl onboard` (it adds a systemd drop-in after asking), or run \
                 `sudo systemctl edit ollama`, add `[Service]` and \
                 `Environment=\"OLLAMA_NO_CLOUD=1\"`, then `sudo systemctl restart ollama`."
                    .to_string(),
            ],
            OllamaService::Manual => vec![format!(
                "Add {json} to ~/.ollama/server.json of the user that runs Ollama, or start it \
                 with `OLLAMA_NO_CLOUD=1 ollama serve`, then restart it."
            )],
        }
    }

    /// Give models a context of [`CONTEXT_FIX_TOKENS`].
    pub fn context(&self) -> String {
        match self.service {
            OllamaService::Brew => format!(
                "add OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} to {} and run `brew services \
                 restart ollama` (`axocoatl onboard` does this after asking)",
                self.brew_env_file
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "~/.homebrew/services/ollama.env".to_string())
            ),
            OllamaService::Systemd => format!(
                "add a drop-in with `Environment=\"OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS}\"` \
                 (`sudo systemctl edit ollama`), then `sudo systemctl restart ollama` (`axocoatl \
                 onboard` does this after asking)"
            ),
            OllamaService::App => format!(
                "in the Ollama app's Settings, set the context length to {}k, then quit and \
                 reopen Ollama",
                CONTEXT_FIX_TOKENS / 1024
            ),
            OllamaService::Manual => format!(
                "start Ollama with OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} (for example \
                 `OLLAMA_CONTEXT_LENGTH={CONTEXT_FIX_TOKENS} OLLAMA_NO_CLOUD=1 ollama serve`)"
            ),
        }
    }
}

/// Ollama's Linux installer, pinned to the audited release. It asks for the
/// password itself and needs `zstd`.
pub const PINNED_LINUX_INSTALL: &str =
    "curl -fsSL https://ollama.com/install.sh | OLLAMA_VERSION=0.40.6 sh";

/// Whether native Sessions can use the Ollama server at `base_url`, one
/// finding per requirement. `alternative` tells the person how to use a
/// different server from where they are (the wizard or the configuration).
pub fn native_findings(
    base_url: &str,
    check: &Result<NativeOllamaServerCheck, String>,
    alternative: &str,
    host: &OllamaHost,
) -> Vec<Finding> {
    if let Err(error) = validate_native_ollama_endpoint(base_url) {
        return vec![Finding::fail(
            format!("Native Sessions cannot use Ollama at {base_url}"),
            vec![
                format!("{error}."),
                "Native Sessions use only an Ollama server on this machine, at a loopback address such as http://localhost:11434.".to_string(),
                alternative.to_string(),
            ],
        )];
    }
    let check = match check {
        Ok(check) => check,
        Err(error) => {
            return vec![Finding::fail(
                format!("Could not read the version and cloud mode of Ollama at {base_url}"),
                vec![error.clone()],
            )]
        }
    };

    let mut findings = Vec::new();
    let version = check.version.as_deref().unwrap_or_default();
    findings.push(match check.version_refusal() {
        None => match ollama_version_note(version) {
            None => Finding::pass(format!(
                "Ollama {version} at {base_url}, a version native Sessions accept"
            )),
            Some(note) => Finding::warning(
                format!("Ollama {version} at {base_url}, newer than the audited {AUDITED_OLLAMA_VERSION}"),
                vec![format!("{note}.")],
            ),
        },
        Some(refusal) => Finding::fail(
            format!(
                "Ollama at {base_url} reports version {}; native Sessions refuse it",
                check.version.as_deref().unwrap_or("(none)")
            ),
            vec![
                format!("{refusal}."),
                version_fix(check.version.as_deref(), host),
                alternative.to_string(),
            ],
        ),
    });
    findings.push(match &check.cloud {
        OllamaCloudMode::Disabled { source } => Finding::pass(format!(
            "Ollama cloud models disabled at {base_url}{}",
            match source.as_str() {
                "env" => " (OLLAMA_NO_CLOUD)",
                "config" => " (~/.ollama/server.json)",
                "both" => " (OLLAMA_NO_CLOUD and ~/.ollama/server.json)",
                _ => "",
            }
        )),
        OllamaCloudMode::Enabled => Finding::fail(
            format!("Ollama at {base_url} has cloud models enabled; native Sessions refuse it"),
            [
                vec!["Native Sessions run only on a server whose cloud features are off.".to_string()],
                host.cloud_off(),
                vec![
                    format!(
                        "Then `curl -s {}/api/status` shows \"disabled\":true.",
                        base_url.trim_end_matches('/')
                    ),
                    alternative.to_string(),
                ],
            ]
            .concat(),
        ),
        OllamaCloudMode::Unreported => Finding::fail(
            format!(
                "Ollama at {base_url} does not report its cloud mode (GET /api/status); native Sessions refuse it"
            ),
            [
                vec![format!(
                    "Native Sessions need Ollama {AUDITED_OLLAMA_VERSION} or later with its cloud features off."
                )],
                host.cloud_off(),
                vec![alternative.to_string()],
            ]
            .concat(),
        ),
    });
    findings
}

/// What to do about a refused server version: restart a server older than
/// the CLI that is installed (the CLI was upgraded, the server was not),
/// upgrade one that is too old, and install the audited release in place of
/// a pre-release, a development build or a new major release.
pub fn version_fix(server: Option<&str>, host: &OllamaHost) -> String {
    let cli_accepted = host
        .cli_version
        .as_deref()
        .is_some_and(|cli| ollama_version_refusal(cli).is_none());
    let too_old = server.is_some_and(|server| {
        axocoatl_isolation::podman_report::version_numbers(server)
            .is_some_and(|numbers| numbers < (0, 40, 6))
            && !server.contains('-')
            && server != "0.0.0"
    });
    if too_old && cli_accepted && host.cli_version.as_deref() != server {
        format!(
            "The ollama on PATH is {} but the server still runs {}: {}.",
            host.cli_version.as_deref().unwrap_or_default(),
            server.unwrap_or_default(),
            host.restart()
        )
    } else if too_old {
        format!("Upgrade it: {}.", host.upgrade())
    } else {
        format!(
            "Use a released Ollama {AUDITED_OLLAMA_VERSION} or a later 0.x release: {}.",
            host.pinned_install()
        )
    }
}

/// The context Ollama loads `model` with on the server at `base_url`, read
/// the way native admission reads it: the model is loaded without
/// generating, its context read from `GET /api/ps`, and the contract probes
/// run on it. Returns the context and the server version.
pub async fn observed_context(base_url: &str, model: &str) -> Result<(usize, String), String> {
    axocoatl_llm_ollama::observe_native_ollama_context(base_url, model)
        .await
        .map(|observation| (observation.context_tokens, observation.server_version))
        .map_err(|error| error.to_string())
}

/// Whether a native Agent can run `model` on the server at `base_url`, read
/// from `POST /api/show` the way native admission reads it: a local GGUF
/// model that completes text, and, for an Agent with tools, calls tools.
pub async fn model_finding(base_url: &str, model: &str, needs_tools: bool) -> Finding {
    match show_native_ollama_model(base_url, model).await {
        Ok(shown) => shown_model_finding(model, &shown, needs_tools),
        Err(error) => Finding::fail(
            format!("Could not read model '{model}' from Ollama at {base_url}"),
            vec![error.to_string()],
        ),
    }
}

/// [`model_finding`] of a `POST /api/show` answer.
pub fn shown_model_finding(model: &str, shown: &Value, needs_tools: bool) -> Finding {
    match check_shown_model(model, shown) {
        Err(error) => {
            let mut hint = vec![error.to_string()];
            if !matches!(
                shown.pointer("/details/format").and_then(Value::as_str),
                Some("gguf")
            ) || shown.pointer("/details/runner").and_then(Value::as_str) == Some("mlx")
            {
                hint.push(format!(
                    "Pull its GGUF variant with `ollama pull {LLAMACPP_PULL_FLAG} {model}` (an undocumented Ollama flag), or choose another model."
                ));
            }
            Finding::fail(format!("Model '{model}' cannot run in native Sessions"), hint)
        }
        Ok(capabilities) if needs_tools && !capabilities.tools => Finding::fail(
            format!("Model '{model}' does not report tool calling, and an Agent that uses it lists tools"),
            vec!["Choose a model whose `ollama show` lists the tools capability, or remove the Agent's tools.".to_string()],
        ),
        Ok(capabilities) => Finding::pass(format!(
            "Model '{model}' is a local GGUF model native Sessions can run ({})",
            [
                Some("completion"),
                capabilities.tools.then_some("tools"),
                capabilities.thinking.then_some("thinking"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tag(name: &str, parameter_size: &str, size: u64, family: &str) -> Value {
        json!({
            "name": name,
            "model": name,
            "size": size,
            "digest": "0".repeat(64),
            "details": {"format": "gguf", "family": family, "families": [family], "parameter_size": parameter_size},
        })
    }

    fn models(tags: Vec<Value>) -> Vec<InstalledModel> {
        parse_tags(&json!({ "models": tags }))
    }

    /// The models on the macOS machine where this was reported, as its Ollama
    /// listed them.
    fn reported_machine() -> Vec<InstalledModel> {
        models(vec![
            tag("gpt-oss:axocoatl-review", "116.8B", 65369818957, "gptoss"),
            tag("qwen3-vl:32b-instruct", "33.4B", 20910297896, "qwen3vl"),
            tag(
                "qwen3-coder:axocoatl-launch",
                "30.5B",
                18556700706,
                "qwen3moe",
            ),
            tag("llama3.2:latest", "3.2B", 2019393189, "llama"),
            tag("qwen2.5vl:7b", "8.3B", 5969245856, "qwen25vl"),
            tag("gpt-oss:120b", "116.8B", 65369818941, "gptoss"),
            tag("qwen3-coder:30b", "30.5B", 18556700761, "qwen3moe"),
            tag("qwen2.5-coder:14b", "14.8B", 8988124298, "qwen2"),
            tag("qwen3:32b", "32.8B", 20201253829, "qwen3"),
            tag("qwen3:8b", "8.2B", 5225388164, "qwen3"),
        ])
    }

    fn suggested(models: &[InstalledModel]) -> (String, String) {
        let team = suggest_team_models(models).expect("a candidate is installed");
        (team.lead, team.reviewer)
    }

    #[test]
    fn the_reported_machine_gets_a_coding_lead_and_a_larger_gpt_oss_reviewer() {
        assert_eq!(
            suggested(&reported_machine()),
            ("qwen3-coder:30b".into(), "gpt-oss:120b".into())
        );
    }

    #[test]
    fn llama3_2_is_never_preferred_over_a_stronger_installed_model() {
        let installed = models(vec![
            tag("llama3.2:latest", "3.2B", 2019393189, "llama"),
            tag("qwen3:8b", "8.2B", 5225388164, "qwen3"),
        ]);
        assert_eq!(
            suggested(&installed),
            ("qwen3:8b".into(), "qwen3:8b".into())
        );
    }

    #[test]
    fn a_coding_lead_beats_a_larger_general_model_which_then_reviews() {
        let installed = models(vec![
            tag("qwen3:32b", "32.8B", 20201253829, "qwen3"),
            tag("qwen2.5-coder:14b", "14.8B", 8988124298, "qwen2"),
            tag("deepseek-coder:6.7b", "7B", 3827834503, "llama"),
        ]);
        assert_eq!(
            suggested(&installed),
            ("qwen2.5-coder:14b".into(), "qwen3:32b".into())
        );
    }

    #[test]
    fn gpt_oss_of_any_size_reviews_so_the_default_pair_is_cross_model() {
        let installed = models(vec![
            tag("qwen3-coder:30b", "30.5B", 18556700761, "qwen3moe"),
            tag("gpt-oss:20b", "20.9B", 13780173839, "gptoss"),
        ]);
        assert_eq!(
            suggested(&installed),
            ("qwen3-coder:30b".into(), "gpt-oss:20b".into())
        );
    }

    #[test]
    fn another_reviewer_must_be_larger_than_the_lead_or_it_shares_the_lead() {
        let installed = models(vec![
            tag("qwen3-coder:30b", "30.5B", 18556700761, "qwen3moe"),
            tag("qwen3:8b", "8.2B", 5225388164, "qwen3"),
        ]);
        assert_eq!(
            suggested(&installed),
            ("qwen3-coder:30b".into(), "qwen3-coder:30b".into())
        );
    }

    #[test]
    fn nothing_installed_recommends_a_cross_model_pair_that_fits_48_gb() {
        assert_eq!(
            TeamModels::recommended(),
            TeamModels {
                lead: "qwen3-coder:30b".into(),
                reviewer: "gpt-oss:20b".into(),
            }
        );
    }

    /// Ollama 0.40 lists a model's format and runner. On Apple silicon a
    /// tag that also has an MLX variant pulls it: such a model is never
    /// offered, and setup says why. GGUF of either runner is offered.
    #[test]
    fn mlx_and_non_gguf_models_are_not_offered_and_setup_says_why() {
        let mut mlx = tag("qwen3.5:27b", "27B", 17_000_000_000, "qwen3_5");
        mlx["details"]["format"] = json!("safetensors");
        mlx["details"]["runner"] = json!("mlx");
        let mut mlx_gguf = tag("gemma4:12b", "12B", 8_000_000_000, "gemma4");
        mlx_gguf["details"]["runner"] = json!("mlx");
        let mut legacy = tag("qwen3-coder:30b", "30.5B", 18556700761, "qwen3moe");
        legacy["details"]["runner"] = json!("ggml");
        let mut llamacpp = tag("gpt-oss:20b", "20.9B", 13780173839, "gptoss");
        llamacpp["details"]["runner"] = json!("llamacpp");
        let installed = models(vec![mlx, mlx_gguf, legacy, llamacpp]);
        let offered: Vec<_> = candidates(&installed).into_iter().map(|m| m.name).collect();
        assert_eq!(offered, ["gpt-oss:20b", "qwen3-coder:30b"]);
        let refused = refused_models(&installed);
        assert_eq!(
            refused
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["qwen3.5:27b", "gemma4:12b"]
        );
        for (name, why) in &refused {
            assert!(why.contains("MLX runner"), "{why}");
            assert!(
                why.contains(&format!("ollama pull --runner llamacpp {name}")),
                "{why}"
            );
            assert!(why.contains("undocumented"), "{why}");
        }
        assert_eq!(
            suggested(&installed),
            ("qwen3-coder:30b".into(), "gpt-oss:20b".into())
        );
    }

    #[test]
    fn a_model_is_checked_the_way_native_admission_checks_it() {
        let gguf = json!({"details":{"format":"gguf","runner":"llamacpp"},
            "capabilities":["completion","tools","thinking"]});
        let finding = shown_model_finding("gpt-oss:20b", &gguf, true);
        assert!(finding.ok, "{finding:?}");
        assert!(
            finding.label.contains("completion, tools, thinking"),
            "{finding:?}"
        );
        let no_tools = json!({"details":{"format":"gguf"},"capabilities":["completion"]});
        assert!(shown_model_finding("plain:latest", &no_tools, false).ok);
        let finding = shown_model_finding("plain:latest", &no_tools, true);
        assert!(
            !finding.ok && finding.label.contains("tool calling"),
            "{finding:?}"
        );
        let mlx = json!({"details":{"format":"safetensors","runner":"mlx"},
            "capabilities":["completion","tools"]});
        let finding = shown_model_finding("qwen3.5:27b", &mlx, false);
        assert!(!finding.ok);
        let hint = finding.hint.join("\n");
        assert!(hint.contains("MLX"), "{hint}");
        assert!(
            hint.contains("ollama pull --runner llamacpp qwen3.5:27b"),
            "{hint}"
        );
        let remote = json!({"details":{"format":"gguf"},"capabilities":["completion"],
            "remote_host":"https://ollama.com:443"});
        let finding = shown_model_finding("gpt-oss:20b-cloud", &remote, false);
        assert!(!finding.ok);
        assert!(finding.hint[0].contains("cloud model"), "{finding:?}");
        assert!(!finding.hint.join(" ").contains("--runner"), "{finding:?}");
    }

    #[test]
    fn cloud_and_embedding_models_are_never_suggested() {
        let mut cloud = tag("gpt-oss:120b-cloud", "116.8B", 384, "gptoss");
        cloud["remote_host"] = json!("https://ollama.com:443");
        cloud["remote_model"] = json!("gpt-oss:120b");
        let mut renamed_cloud = tag("big-reviewer:latest", "671B", 384, "deepseek2");
        renamed_cloud["remote_model"] = json!("deepseek-v3.1:671b");
        let installed = models(vec![
            cloud,
            renamed_cloud,
            tag("qwen3-coder:480b-cloud", "480B", 384, "qwen3moe"),
            tag("nomic-embed-text:latest", "137M", 274302450, "nomic-bert"),
            tag("bge-m3:latest", "566.70M", 1157672605, "bert"),
            tag("qwen3-coder:30b", "30.5B", 18556700761, "qwen3moe"),
        ]);
        assert_eq!(
            suggested(&installed),
            ("qwen3-coder:30b".into(), "qwen3-coder:30b".into())
        );
        let offered: Vec<_> = candidates(&installed).into_iter().map(|m| m.name).collect();
        assert_eq!(offered, ["qwen3-coder:30b"]);
    }

    #[test]
    fn vision_models_are_a_last_resort_for_lead() {
        let installed = models(vec![
            tag("qwen3-vl:32b-instruct", "33.4B", 20910297896, "qwen3vl"),
            tag("qwen3:8b", "8.2B", 5225388164, "qwen3"),
        ]);
        assert_eq!(
            suggested(&installed),
            ("qwen3:8b".into(), "qwen3-vl:32b-instruct".into())
        );
        let only_vision = models(vec![tag("llava:13b", "13B", 8000000000, "llama")]);
        assert_eq!(suggested(&only_vision).0, "llava:13b");
    }

    #[test]
    fn nothing_suitable_installed_suggests_nothing() {
        assert_eq!(suggest_team_models(&[]), None);
        let embedding_only = models(vec![tag(
            "nomic-embed-text:latest",
            "137M",
            1,
            "nomic-bert",
        )]);
        assert_eq!(suggest_team_models(&embedding_only), None);
    }

    #[test]
    fn a_missing_parameter_size_falls_back_to_the_file_size() {
        let installed = models(vec![
            json!({"name": "hf.co/org/Qwen3-Coder-30B-A3B-Instruct-GGUF:Q4_K_M", "size": 18_000_000_000u64}),
            json!({"name": "registry.local:5000/team/general", "size": 4_000_000_000u64}),
        ]);
        assert_eq!(installed[1].base(), "general");
        assert_eq!(installed[1].tag(), "latest");
        assert_eq!(
            suggested(&installed).0,
            "hf.co/org/Qwen3-Coder-30B-A3B-Instruct-GGUF:Q4_K_M"
        );
    }

    #[test]
    fn parameter_sizes_parse_with_their_unit() {
        assert_eq!(parse_parameters("30.5B"), Some(30.5e9));
        assert_eq!(parse_parameters("137M"), Some(137e6));
        assert_eq!(parse_parameters(" 1T "), Some(1e12));
        for invalid in ["", "B", "30.5", "-1B", "NaNB", "3О.5B"] {
            assert_eq!(parse_parameters(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn installed_matches_exact_names_and_the_latest_tag() {
        let installed = reported_machine();
        assert!(is_installed("qwen3-coder:30b", &installed));
        assert!(is_installed("llama3.2", &installed));
        assert!(!is_installed("qwen3-coder", &installed));
        assert!(!is_installed("qwen3-coder:480b", &installed));
    }

    #[test]
    fn labels_show_parameters_and_size() {
        assert_eq!(
            reported_machine()[6].label(),
            "qwen3-coder:30b (30.5B, 18.6 GB)"
        );
    }

    const ALTERNATIVE: &str = "Or set providers.ollama.base_url to another local Ollama server.";

    fn check(version: &str, cloud: OllamaCloudMode) -> Result<NativeOllamaServerCheck, String> {
        Ok(NativeOllamaServerCheck {
            version: Some(version.to_string()),
            cloud,
        })
    }

    fn host(service: OllamaService, macos: bool) -> OllamaHost {
        OllamaHost {
            service,
            macos,
            brew_env_file: Some("/Users/a/.homebrew/services/ollama.env".into()),
            cli_version: Some(AUDITED_OLLAMA_VERSION.to_string()),
        }
    }

    fn manual() -> OllamaHost {
        host(OllamaService::Manual, true)
    }

    /// The cloud fix depends on how Ollama runs: the brew service reads the
    /// user's server.json, the systemd service (User=ollama) does not, and
    /// the app has a setting. `launchctl setenv` is never suggested: it
    /// does not survive a reboot.
    #[test]
    fn cloud_enabled_fails_with_the_fix_for_how_ollama_runs() {
        for (service, expected, absent) in [
            (
                OllamaService::Brew,
                r#"{"disable_ollama_cloud": true} to ~/.ollama/server.json and run `brew services restart ollama`"#,
                "systemctl",
            ),
            (
                OllamaService::Systemd,
                r#"Environment="OLLAMA_NO_CLOUD=1"`, then `sudo systemctl restart ollama`"#,
                "~/.ollama/server.json",
            ),
            (OllamaService::App, "turn off Cloud in Settings", "brew"),
            (
                OllamaService::Manual,
                "`OLLAMA_NO_CLOUD=1 ollama serve`",
                "systemctl",
            ),
        ] {
            let findings = native_findings(
                "http://localhost:11434",
                &check(AUDITED_OLLAMA_VERSION, OllamaCloudMode::Enabled),
                ALTERNATIVE,
                &host(service, service != OllamaService::Systemd),
            );
            assert_eq!(findings.len(), 2);
            assert!(findings[0].ok, "{findings:?}");
            let cloud = &findings[1];
            assert!(!cloud.ok);
            assert_eq!(
                cloud.label,
                "Ollama at http://localhost:11434 has cloud models enabled; native Sessions refuse it"
            );
            let hint = cloud.hint.join("\n");
            assert!(hint.contains(expected), "{service:?}: {hint}");
            assert!(!hint.contains(absent), "{service:?}: {hint}");
            assert!(!hint.contains("launchctl"), "{hint}");
            assert!(hint.ends_with(ALTERNATIVE));
        }
    }

    #[test]
    fn a_ready_server_passes_and_names_where_cloud_was_disabled() {
        let findings = native_findings(
            "http://127.0.0.1:11436",
            &check(
                "0.40.7",
                OllamaCloudMode::Disabled {
                    source: "config".into(),
                },
            ),
            ALTERNATIVE,
            &manual(),
        );
        assert!(
            findings.iter().all(|finding| finding.ok && !finding.warn),
            "{findings:?}"
        );
        assert_eq!(
            findings[1].label,
            "Ollama cloud models disabled at http://127.0.0.1:11436 (~/.ollama/server.json)"
        );
    }

    #[test]
    fn an_old_version_and_missing_status_both_fail() {
        let findings = native_findings(
            "http://localhost:11434",
            &check("0.12.3", OllamaCloudMode::Unreported),
            ALTERNATIVE,
            &OllamaHost {
                cli_version: None,
                ..host(OllamaService::Brew, true)
            },
        );
        assert!(findings.iter().all(|finding| !finding.ok), "{findings:?}");
        assert!(findings[0].label.contains("reports version 0.12.3"));
        let hint = findings[0].hint.join("\n");
        assert!(hint.contains("upgrade Ollama to 0.40.6 or later"), "{hint}");
        assert!(
            hint.contains("brew upgrade ollama && brew services restart ollama"),
            "{hint}"
        );
        assert!(findings[1].label.contains("does not report its cloud mode"));
    }

    /// `brew upgrade` leaves the old server running until it restarts: when
    /// the CLI is accepted and the server is older, the fix is the restart.
    #[test]
    fn a_server_older_than_its_cli_is_restarted_not_upgraded() {
        let findings = native_findings(
            "http://localhost:11434",
            &check(
                "0.20.6",
                OllamaCloudMode::Disabled {
                    source: "config".into(),
                },
            ),
            ALTERNATIVE,
            &host(OllamaService::Brew, true),
        );
        assert!(!findings[0].ok);
        assert!(findings[0].hint[0].contains("0.20.6 is no longer supported"));
        assert_eq!(
            findings[0].hint[1],
            "The ollama on PATH is 0.40.6 but the server still runs 0.20.6: brew services restart ollama."
        );
        let linux = native_findings(
            "http://localhost:11434",
            &check("0.20.6", OllamaCloudMode::Enabled),
            ALTERNATIVE,
            &host(OllamaService::Systemd, false),
        );
        assert!(linux[0].hint[1].contains("sudo systemctl restart ollama"));
    }

    /// A newer minor passes with a warning; a pre-release or a new major
    /// release gets the pinned install, never `brew upgrade`.
    #[test]
    fn a_newer_minor_warns_and_a_pre_release_gets_the_pinned_install() {
        let findings = native_findings(
            "http://localhost:11434",
            &check(
                "0.41.0",
                OllamaCloudMode::Disabled {
                    source: "config".into(),
                },
            ),
            ALTERNATIVE,
            &host(OllamaService::Brew, true),
        );
        assert!(findings[0].ok && findings[0].warn, "{findings:?}");
        assert!(findings[0].label.contains("newer than the audited 0.40.6"));
        assert!(findings[0].hint[0].contains("contract probes pass"));
        for version in ["0.40.7-rc0", "1.0.0", "0.0.0"] {
            for (macos, expected) in [
                (
                    true,
                    "https://github.com/ollama/ollama/releases/tag/v0.40.6",
                ),
                (false, PINNED_LINUX_INSTALL),
            ] {
                let findings = native_findings(
                    "http://localhost:11434",
                    &check(version, OllamaCloudMode::Enabled),
                    ALTERNATIVE,
                    &host(OllamaService::Brew, macos),
                );
                let hint = findings[0].hint.join("\n");
                assert!(!findings[0].ok);
                assert!(hint.contains(expected), "{version}: {hint}");
                assert!(!hint.contains("brew upgrade"), "{version}: {hint}");
            }
        }
    }

    #[test]
    fn a_remote_endpoint_fails_before_any_request() {
        let findings = native_findings(
            "http://192.0.2.10:11434",
            &Err("not requested".into()),
            ALTERNATIVE,
            &manual(),
        );
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].ok);
        assert!(findings[0].hint.join(" ").contains("loopback"));
    }

    /// The hints for each way Ollama runs on Linux name systemctl and never
    /// launchctl; the macOS ones never name systemctl.
    #[test]
    fn hints_follow_the_operating_system_and_the_service() {
        let systemd = host(OllamaService::Systemd, false);
        assert_eq!(systemd.start(), "sudo systemctl start ollama");
        assert_eq!(systemd.restart(), "sudo systemctl restart ollama");
        assert_eq!(systemd.upgrade(), PINNED_LINUX_INSTALL);
        assert!(systemd.context().contains("OLLAMA_CONTEXT_LENGTH=32768"));
        let linux_manual = OllamaHost {
            cli_version: None,
            ..host(OllamaService::Manual, false)
        };
        assert_eq!(linux_manual.start(), PINNED_LINUX_INSTALL);
        for hint in [
            systemd.start(),
            systemd.restart(),
            systemd.context(),
            systemd.cloud_off().join(" "),
            linux_manual.start(),
        ] {
            assert!(
                !hint.contains("launchctl") && !hint.contains("brew"),
                "{hint}"
            );
        }
        let brew = host(OllamaService::Brew, true);
        assert_eq!(brew.start(), "brew services start ollama");
        assert!(brew
            .context()
            .contains("/Users/a/.homebrew/services/ollama.env"));
        for hint in [brew.start(), brew.restart(), brew.context(), brew.upgrade()] {
            assert!(!hint.contains("systemctl"), "{hint}");
        }
        let mac_manual = OllamaHost {
            cli_version: None,
            ..host(OllamaService::Manual, true)
        };
        assert!(mac_manual.start().contains("brew install ollama"));
    }

    /// Models follow memory: nothing larger than about 0.65 of it is
    /// preselected, and the pair recommended without installed models
    /// depends on the tier (none below 48 GB).
    #[test]
    fn model_choice_follows_memory() {
        const GIB: u64 = 1 << 30;
        let installed = reported_machine();
        let at = |gb: u64| suggest_fitting_team_models(&installed, gb * GIB).unwrap();
        assert_eq!(at(128), suggest_team_models(&installed).unwrap());
        assert_eq!(at(128).reviewer, "gpt-oss:120b");
        // 48 GB: gpt-oss:120b (65.4 GB) does not fit; the largest that
        // does reviews.
        let at48 = at(48);
        assert_eq!(at48.lead, "qwen3-coder:30b");
        assert_ne!(at48.reviewer, "gpt-oss:120b");
        assert!(!fits_memory(known_size("gpt-oss:120b").unwrap(), 48 * GIB));
        assert!(fits_memory(
            known_size("qwen3-coder:30b").unwrap(),
            48 * GIB
        ));
        assert!(fits_memory(known_size("gpt-oss:20b").unwrap(), 24 * GIB));
        assert!(!fits_memory(
            known_size("qwen3-coder:30b").unwrap(),
            24 * GIB
        ));
        assert!(!fits_memory(known_size("gpt-oss:20b").unwrap(), 16 * GIB));
        let at16 = suggest_fitting_team_models(&installed, 16 * GIB).unwrap();
        assert!(
            installed
                .iter()
                .filter(|model| [at16.lead.as_str(), at16.reviewer.as_str()]
                    .contains(&model.name()))
                .all(|model| fits_memory(model.size_bytes(), 16 * GIB)),
            "{at16:?}"
        );

        assert_eq!(
            recommended_pair(128 * GIB).unwrap().0,
            TeamModels {
                lead: "qwen3-coder:30b".into(),
                reviewer: "gpt-oss:120b".into()
            }
        );
        assert_eq!(
            recommended_pair(96 * GIB).unwrap().0.reviewer,
            "gpt-oss:120b"
        );
        assert_eq!(
            recommended_pair(48 * GIB).unwrap().0,
            TeamModels::recommended()
        );
        assert!(recommended_pair(48 * GIB)
            .unwrap()
            .1
            .contains("not been measured"));
        assert_eq!(recommended_pair(24 * GIB), None);
        assert_eq!(recommended_pair(16 * GIB), None);
        assert_eq!(size_text(18_556_700_761), "18.6 GB");
    }
}
