//! Ollama checks and model defaults for `onboard` and `doctor`.
//!
//! Native Sessions accept only a loopback Ollama server whose version the
//! version policy accepts (Ollama 0.40.6 or a later 0.40 release) and that
//! reports its cloud features disabled, and only GGUF models that Ollama runs
//! in llama.cpp's llama-server; `axocoatl_llm_ollama` owns that contract and
//! this module reports it. Setup also reads the models installed on the
//! chosen server to suggest the default team's models.

use axocoatl_llm_ollama::{
    check_native_ollama_server, check_shown_model, show_native_ollama_model,
    validate_native_ollama_endpoint, NativeOllamaServerCheck, OllamaCloudMode,
    AUDITED_OLLAMA_VERSION,
};
use serde_json::Value;

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

/// The flag of `ollama pull` that selects the GGUF variant of a model whose
/// tag also has an MLX variant. Ollama does not document it.
pub const LLAMACPP_PULL_FLAG: &str = "--runner llamacpp";

/// The models the default team runs on. Lead, Scout and the chat Assistant
/// use `lead`; Reviewer uses `reviewer`, which may be the same model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamModels {
    pub lead: String,
    pub reviewer: String,
}

impl TeamModels {
    #[cfg(test)]
    pub fn same(model: &str) -> Self {
        Self {
            lead: model.to_string(),
            reviewer: model.to_string(),
        }
    }

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
pub fn suggest_team_models(models: &[InstalledModel]) -> Option<TeamModels> {
    let candidates = candidates(models);
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

/// What `doctor` says to do about `name`, which is not installed under that
/// exact name: pull it, or, when another tag of the same model is installed
/// (`qwen3-coder:30b` for `qwen3-coder`, which Ollama reads as
/// `qwen3-coder:latest`), name that tag exactly.
pub fn not_pulled_hint(name: &str, models: &[InstalledModel]) -> String {
    let base = name.split_once(':').map_or(name, |(base, _)| base);
    let tags: Vec<&str> = models
        .iter()
        .map(|model| model.name.as_str())
        .filter(|installed| {
            installed
                .split_once(':')
                .map_or(*installed, |(base, _)| base)
                == base
                && *installed != name
        })
        .collect();
    match tags.as_slice() {
        [] => format!("Run: ollama pull {name}"),
        tags => format!(
            "Run: ollama pull {name}, or use the installed {} by its exact name (Ollama reads \
             {name} as {})",
            tags.join(", "),
            if name.contains(':') {
                name.to_string()
            } else {
                format!("{name}:latest")
            }
        ),
    }
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

impl ServerProbe {
    /// Native Sessions can use this server.
    pub fn ready(&self) -> bool {
        matches!(self, Self::Reachable { check: Ok(check), .. } if check.ready())
    }

    pub fn models(&self) -> &[InstalledModel] {
        match self {
            Self::Unreachable(_) => &[],
            Self::Reachable { models, .. } => models,
        }
    }
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

/// One line of a check: a passed requirement, or a failed one with what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub ok: bool,
    pub label: String,
    pub hint: Vec<String>,
}

impl Finding {
    fn pass(label: String) -> Self {
        Self {
            ok: true,
            label,
            hint: Vec::new(),
        }
    }

    fn fail(label: String, hint: Vec<String>) -> Self {
        Self {
            ok: false,
            label,
            hint,
        }
    }
}

/// How to turn Ollama's cloud features off. Verified against Ollama 0.20.6 and
/// 0.40.6 (the same code in both): `envconfig.NoCloud` is true when
/// `OLLAMA_NO_CLOUD` parses as true or the server user's `~/.ollama/server.json`
/// sets `disable_ollama_cloud`, and the server reads both once at start.
pub fn disable_cloud_steps(base_url: &str) -> Vec<String> {
    vec![
        "To disable Ollama's cloud features, do one of these and restart Ollama:".to_string(),
        "  - add {\"disable_ollama_cloud\": true} to ~/.ollama/server.json of the user that runs Ollama".to_string(),
        "  - start the server with OLLAMA_NO_CLOUD=1, e.g. `OLLAMA_NO_CLOUD=1 ollama serve`".to_string(),
        "    (macOS app: `launchctl setenv OLLAMA_NO_CLOUD 1`, then quit and reopen Ollama)".to_string(),
        format!(
            "Then `curl -s {}/api/status` shows \"disabled\":true.",
            base_url.trim_end_matches('/')
        ),
    ]
}

/// Whether native Sessions can use the Ollama server at `base_url`, one
/// finding per requirement. `alternative` tells the person how to use a
/// different server from where they are (the wizard or the configuration).
pub fn native_findings(
    base_url: &str,
    check: &Result<NativeOllamaServerCheck, String>,
    alternative: &str,
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
    findings.push(match check.version_refusal() {
        None => Finding::pass(format!(
            "Ollama {} at {base_url}, a version native Sessions accept ({AUDITED_OLLAMA_VERSION} or a later 0.40 release)",
            check.version.as_deref().unwrap_or_default()
        )),
        Some(refusal) => Finding::fail(
            format!(
                "Ollama at {base_url} reports version {}; native Sessions refuse it",
                check.version.as_deref().unwrap_or("(none)")
            ),
            vec![
                format!("{refusal}."),
                format!(
                    "Install Ollama {AUDITED_OLLAMA_VERSION} or a later 0.40 release on this machine (https://ollama.com/download, or `brew upgrade ollama`) and restart it."
                ),
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
                disable_cloud_steps(base_url),
                vec![alternative.to_string()],
            ]
            .concat(),
        ),
        OllamaCloudMode::Unreported => Finding::fail(
            format!(
                "Ollama at {base_url} does not report its cloud mode (GET /api/status); native Sessions refuse it"
            ),
            [
                vec![format!(
                    "Native Sessions need Ollama {AUDITED_OLLAMA_VERSION} or a later 0.40 release with its cloud features off."
                )],
                disable_cloud_steps(base_url),
                vec![alternative.to_string()],
            ]
            .concat(),
        ),
    });
    findings
}

/// Whether a native Agent can run `model` on the server at `base_url`, read
/// from `POST /api/show` the way native admission reads it: a local GGUF
/// model that completes text, and, for an Agent with tools, calls tools.
pub async fn model_finding(base_url: &str, model: &str, needs_tools: bool) -> Finding {
    match show_native_ollama_model(base_url, model).await {
        Ok(shown) => shown_model_finding(model, &shown, needs_tools),
        Err(error) => Finding::fail(
            format!("Could not read model '{model}' from Ollama at {base_url}"),
            vec![axocoatl_llm_ollama::describe_native_ollama_error(&error)],
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
        // Not installed under that name: pull it, or name an installed tag.
        assert_eq!(
            not_pulled_hint("qwen3-coder", &installed),
            "Run: ollama pull qwen3-coder, or use the installed qwen3-coder:axocoatl-launch, \
             qwen3-coder:30b by its exact name (Ollama reads qwen3-coder as \
             qwen3-coder:latest)"
        );
        assert_eq!(
            not_pulled_hint("mistral:7b", &installed),
            "Run: ollama pull mistral:7b"
        );
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

    #[test]
    fn cloud_enabled_fails_with_the_documented_ollama_settings() {
        let findings = native_findings(
            "http://localhost:11434",
            &check(AUDITED_OLLAMA_VERSION, OllamaCloudMode::Enabled),
            ALTERNATIVE,
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
        assert!(hint.contains(r#"{"disable_ollama_cloud": true} to ~/.ollama/server.json"#));
        assert!(hint.contains("OLLAMA_NO_CLOUD=1 ollama serve"));
        assert!(hint.contains("launchctl setenv OLLAMA_NO_CLOUD 1"));
        assert!(hint.contains("curl -s http://localhost:11434/api/status"));
        assert!(hint.ends_with(ALTERNATIVE));
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
        );
        assert!(findings.iter().all(|finding| finding.ok), "{findings:?}");
        assert_eq!(
            findings[1].label,
            "Ollama cloud models disabled at http://127.0.0.1:11436 (~/.ollama/server.json)"
        );
    }

    #[test]
    fn an_unaudited_version_and_missing_status_both_fail() {
        let findings = native_findings(
            "http://localhost:11434",
            &check("0.12.3", OllamaCloudMode::Unreported),
            ALTERNATIVE,
        );
        assert!(findings.iter().all(|finding| !finding.ok), "{findings:?}");
        assert!(findings[0].label.contains("reports version 0.12.3"));
        let hint = findings[0].hint.join("\n");
        assert!(hint.contains("upgrade Ollama to 0.40.6 or later"), "{hint}");
        assert!(hint.contains("brew upgrade ollama"), "{hint}");
        assert!(findings[1].label.contains("does not report its cloud mode"));
    }

    #[test]
    fn the_previously_supported_and_a_newer_minor_version_say_what_to_do() {
        let findings = native_findings(
            "http://localhost:11434",
            &check(
                "0.20.6",
                OllamaCloudMode::Disabled {
                    source: "config".into(),
                },
            ),
            ALTERNATIVE,
        );
        assert!(!findings[0].ok);
        assert!(findings[0].hint[0].contains("0.20.6 is no longer supported"));
        let findings = native_findings(
            "http://localhost:11434",
            &check(
                "0.41.0",
                OllamaCloudMode::Disabled {
                    source: "config".into(),
                },
            ),
            ALTERNATIVE,
        );
        assert!(!findings[0].ok);
        assert!(findings[0].hint[0].contains("upgrade Axocoatl"));
    }

    #[test]
    fn a_remote_endpoint_fails_before_any_request() {
        let findings = native_findings(
            "http://192.0.2.10:11434",
            &Err("not requested".into()),
            ALTERNATIVE,
        );
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].ok);
        assert!(findings[0].hint.join(" ").contains("loopback"));
    }
}
