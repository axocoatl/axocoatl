//! `run.models`: the writer and reviewer models `axocoatl onboard` chose,
//! which `axocoatl run` and `POST /api/runs` use when a run does not name its
//! own. Owner: workstream `admission`.
//!
//! The configuration is checked when it loads ([`validate_run_defaults`]),
//! and each run applies it to its loadout's parameters
//! ([`apply_run_model_defaults`]): `run.models.writer` is the default of the
//! loadout's `writer_model` parameter and `run.models.reviewer` of its
//! `reviewer_model`, only where the run gives no value and only where the
//! provider fits what the parameter feeds.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::loadout::{AgentRuntime, LoadoutFile, ModelSpec, ParamKind, ParamOr, ParamValues};
use crate::types::{RunConfigYaml, RunModelsYaml};

/// The providers a native Agent or reviewer runs on: Axocoatl's own tool
/// loop calls only these.
pub const NATIVE_PROVIDERS: [&str; 2] = ["ollama", "openrouter"];

/// The providers `run.models.writer` may name: a native writer's
/// ([`NATIVE_PROVIDERS`]), or the one whose API an external writer's
/// program calls (`anthropic` for Claude Code, `openai` for Codex).
pub const WRITER_PROVIDERS: [&str; 4] = ["ollama", "openrouter", "anthropic", "openai"];

/// One configured default a run considered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunModelDefault {
    /// The loadout parameter, such as `reviewer_model`.
    pub param: String,
    /// `provider:model`.
    pub value: String,
    /// The configuration key, such as `run.models.reviewer`.
    pub source: String,
    /// Why the run did not use it; absent when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

impl RunModelDefault {
    /// The note a run records for this default, as `axocoatl run` prints it.
    pub fn note(&self) -> String {
        match &self.skipped {
            None => format!(
                "{} = {} (from your setup: {} in the configuration)",
                self.param, self.value, self.source
            ),
            Some(reason) => format!(
                "{} {} was not used for {}: {reason}",
                self.source, self.value, self.param
            ),
        }
    }
}

fn invalid(field: &str, value: &str, reason: String, suggestion: &str) -> ConfigError {
    ConfigError::InvalidField {
        field: field.to_string(),
        value: format!("{value:?}"),
        reason,
        suggestion: suggestion.to_string(),
    }
}

/// Check `run:` when the configuration loads: each model is
/// `provider:model`, the writer on a provider a loadout writer can use
/// ([`WRITER_PROVIDERS`]), the reviewer on a native provider
/// ([`NATIVE_PROVIDERS`]), and the two are different models.
pub fn validate_run_defaults(run: &RunConfigYaml) -> Result<(), ConfigError> {
    let models = &run.models;
    let mut parsed = Vec::new();
    for (role, value) in models.roles() {
        let field = format!("run.models.{role}");
        let spec = ModelSpec::parse(value).ok_or_else(|| {
            invalid(
                &field,
                value,
                "A model is written provider:model".to_string(),
                "Write it like ollama:qwen3-coder:30b or openrouter:qwen/qwen3-coder",
            )
        })?;
        let allowed: &[&str] = if role == "reviewer" {
            &NATIVE_PROVIDERS
        } else {
            &WRITER_PROVIDERS
        };
        if !allowed.contains(&spec.provider.as_str()) {
            let reason = if role == "reviewer" {
                format!(
                    "The reviewer is a native Agent, which runs on Ollama or OpenRouter only; \
                     {} is not one of them (Claude Code and Codex can write but not review)",
                    spec.provider
                )
            } else {
                format!(
                    "A loadout writer runs on Ollama or OpenRouter, or is Claude Code \
                     (anthropic:) or Codex (openai:); {} is none of these",
                    spec.provider
                )
            };
            return Err(invalid(
                &field,
                value,
                reason,
                &format!("Use one of: {}", allowed.join(", ")),
            ));
        }
        parsed.push(spec);
    }
    if let [writer, reviewer] = parsed.as_slice() {
        if writer.same_model(reviewer) {
            return Err(invalid(
                "run.models.reviewer",
                models.reviewer.as_deref().unwrap_or_default(),
                "The reviewer must be a different model than the writer: a run's review is a \
                 second model reading the change"
                    .to_string(),
                "Choose another reviewer model, such as ollama:gpt-oss:20b for the writer \
                 ollama:qwen3-coder:30b",
            ));
        }
    }
    Ok(())
}

/// Why `spec` cannot be the value of `param` in `file`: a native Agent or
/// the reviewer needs a native provider, an external writer the provider
/// whose API its program calls. `None` when it fits every use.
fn misfit(file: &LoadoutFile, param: &str, spec: &ModelSpec) -> Option<String> {
    let names = |model: &ParamOr<ModelSpec>| matches!(model, ParamOr::Param { param: name } if name == param);
    for agent in file.agents.iter().filter(|agent| names(&agent.model)) {
        let refusal = match agent.runtime {
            AgentRuntime::Native => {
                (!NATIVE_PROVIDERS.contains(&spec.provider.as_str())).then(|| {
                    format!(
                        "loadout {}'s {} runs natively, on Ollama or OpenRouter",
                        file.id, agent.id
                    )
                })
            }
            runtime => runtime.model_refusal(spec).map(|_| {
                format!(
                    "loadout {}'s {} runs {}, which calls {}'s model API",
                    file.id,
                    agent.id,
                    runtime.id(),
                    runtime.model_provider().unwrap_or("another")
                )
            }),
        };
        if refusal.is_some() {
            return refusal;
        }
    }
    let reviews = file
        .review
        .as_ref()
        .is_some_and(|review| names(&review.model));
    (reviews && !NATIVE_PROVIDERS.contains(&spec.provider.as_str())).then(|| {
        format!(
            "loadout {}'s reviewer runs natively, on Ollama or OpenRouter",
            file.id
        )
    })
}

/// Apply `models` to `values`, the parameters a run of `file` was given:
/// `run.models.<role>` becomes the value of the model parameter
/// `<role>_model` when `file` declares it, the run gives no value for it,
/// and its provider fits what the parameter feeds (a default that does not
/// fit is left out, and the loadout's own default, if any, applies). Returns
/// every default considered, applied or not, in role order.
pub fn apply_run_model_defaults(
    file: &LoadoutFile,
    models: &RunModelsYaml,
    values: &mut ParamValues,
) -> Vec<RunModelDefault> {
    let mut considered = Vec::new();
    for (role, value) in models.roles() {
        let param = format!("{role}_model");
        let declared = file
            .params
            .get(&param)
            .is_some_and(|declared| declared.kind == ParamKind::Model);
        if !declared || values.contains_key(&param) {
            continue;
        }
        let Some(spec) = ModelSpec::parse(value) else {
            continue;
        };
        let skipped = misfit(file, &param, &spec);
        if skipped.is_none() {
            values.insert(param.clone(), spec.to_string());
        }
        considered.push(RunModelDefault {
            param,
            value: spec.to_string(),
            source: format!("run.models.{role}"),
            skipped,
        });
    }
    considered
}

/// `run.models` in the text of a configuration file, read alone: what the
/// file says now, without the other checks of the configuration, so a
/// running daemon can tell that `axocoatl onboard` or an edit changed them
/// since it started. `None` when the text does not parse.
pub fn run_models_in(text: &str) -> Option<RunModelsYaml> {
    #[derive(Deserialize)]
    struct OnlyRun {
        #[serde(default)]
        run: RunConfigYaml,
    }
    crate::yaml::from_str::<OnlyRun>(text)
        .ok()
        .map(|only| only.run.models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loadout::builtin_loadouts;

    fn file(id: &str) -> LoadoutFile {
        builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == id)
            .unwrap()
            .file
    }

    fn models(writer: Option<&str>, reviewer: Option<&str>) -> RunModelsYaml {
        RunModelsYaml {
            writer: writer.map(str::to_string),
            reviewer: reviewer.map(str::to_string),
        }
    }

    fn run(writer: Option<&str>, reviewer: Option<&str>) -> RunConfigYaml {
        RunConfigYaml {
            models: models(writer, reviewer),
        }
    }

    /// The file's `run.models` alone, whatever else the file holds; none in
    /// a file without them, and `None` for text that does not parse.
    #[test]
    fn run_models_are_read_alone_from_a_configuration_file() {
        let text = "agents: []\nproviders:\n  ollama:\n    base_url: http://127.0.0.1:11434\n\
                    run:\n  models:\n    reviewer: \"ollama:gpt-oss:20b\"\n";
        assert_eq!(
            run_models_in(text),
            Some(models(None, Some("ollama:gpt-oss:20b")))
        );
        assert_eq!(
            run_models_in("agents: []\n"),
            Some(RunModelsYaml::default())
        );
        assert_eq!(run_models_in("run: [unclosed"), None);
    }

    #[test]
    fn the_onboarded_pair_is_valid_and_each_role_is_checked() {
        validate_run_defaults(&RunConfigYaml::default()).unwrap();
        validate_run_defaults(&run(
            Some("ollama:qwen3-coder:30b"),
            Some("ollama:gpt-oss:20b"),
        ))
        .unwrap();
        validate_run_defaults(&run(Some("anthropic:sonnet"), Some("ollama:gpt-oss:20b"))).unwrap();
        validate_run_defaults(&run(None, Some("openrouter:openai/gpt-oss-120b"))).unwrap();
        for (config, field, reason) in [
            (
                run(Some("qwen3-coder"), None),
                "run.models.writer",
                "provider:model",
            ),
            (
                run(None, Some("anthropic:opus")),
                "run.models.reviewer",
                "Ollama or OpenRouter only",
            ),
            (
                run(None, Some("claude-code:opus")),
                "run.models.reviewer",
                "can write but not review",
            ),
            (
                run(Some("gemini:gemini-3"), None),
                "run.models.writer",
                "none of these",
            ),
            (
                run(
                    Some("ollama:qwen3-coder:30b"),
                    Some("ollama:qwen3-coder:30b"),
                ),
                "run.models.reviewer",
                "different model",
            ),
        ] {
            let error = validate_run_defaults(&config).unwrap_err();
            let ConfigError::InvalidField {
                field: got,
                reason: why,
                ..
            } = &error
            else {
                panic!("{error}");
            };
            assert_eq!(got, field, "{error}");
            assert!(why.contains(reason), "{error}");
        }
    }

    #[test]
    fn a_configuration_with_run_models_parses_and_unknown_keys_are_refused() {
        let config = crate::parse_config(
            "run:\n  models:\n    writer: \"ollama:qwen3-coder:30b\"\n    reviewer: \"ollama:gpt-oss:20b\"\n",
            std::path::Path::new("config.yaml"),
        )
        .unwrap();
        assert_eq!(
            config.run.models,
            models(Some("ollama:qwen3-coder:30b"), Some("ollama:gpt-oss:20b"))
        );
        assert!(crate::parse_config(
            "run:\n  models:\n    reviwer: \"ollama:gpt-oss:20b\"\n",
            std::path::Path::new("config.yaml"),
        )
        .is_err());
        let error = crate::parse_config(
            "run:\n  models:\n    reviewer: \"anthropic:opus\"\n",
            std::path::Path::new("config.yaml"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("run.models.reviewer"), "{error}");
        // An empty section is not written back.
        let text = crate::yaml::to_string(&crate::AxocoatlConfig::default()).unwrap();
        assert!(!text.contains("run:"), "{text}");
    }

    #[test]
    fn defaults_fill_only_the_parameters_the_run_left_out() {
        let onboarded = models(Some("ollama:qwen3-coder:30b"), Some("ollama:gpt-oss:20b"));
        let mut values = ParamValues::new();
        let used = apply_run_model_defaults(&file("fix"), &onboarded, &mut values);
        assert_eq!(values["writer_model"], "ollama:qwen3-coder:30b");
        assert_eq!(values["reviewer_model"], "ollama:gpt-oss:20b");
        assert_eq!(
            used.iter().map(RunModelDefault::note).collect::<Vec<_>>(),
            [
                "writer_model = ollama:qwen3-coder:30b (from your setup: run.models.writer in \
                 the configuration)",
                "reviewer_model = ollama:gpt-oss:20b (from your setup: run.models.reviewer in \
                 the configuration)"
            ]
        );
        // `--model reviewer=…` wins over the configured reviewer.
        let mut values = ParamValues::new();
        values.insert(
            "reviewer_model".into(),
            "openrouter:openai/gpt-oss-120b".into(),
        );
        let used = apply_run_model_defaults(&file("fix"), &onboarded, &mut values);
        assert_eq!(values["reviewer_model"], "openrouter:openai/gpt-oss-120b");
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].param, "writer_model");
        // A loadout without the parameter takes nothing.
        let mut values = ParamValues::new();
        assert!(apply_run_model_defaults(&file("qa"), &onboarded, &mut values).is_empty());
        assert!(values.is_empty());
    }

    #[test]
    fn a_default_that_does_not_fit_the_loadout_is_left_out_and_says_why() {
        // A local writer is not Claude Code's model: fix-claude-code keeps
        // its own writer default and takes the reviewer.
        let onboarded = models(Some("ollama:qwen3-coder:30b"), Some("ollama:gpt-oss:20b"));
        let mut values = ParamValues::new();
        let used = apply_run_model_defaults(&file("fix-claude-code"), &onboarded, &mut values);
        assert!(!values.contains_key("writer_model"));
        assert_eq!(values["reviewer_model"], "ollama:gpt-oss:20b");
        assert_eq!(
            used[0].note(),
            "run.models.writer ollama:qwen3-coder:30b was not used for writer_model: loadout \
             fix-claude-code's writer runs claude-code, which calls anthropic's model API"
        );
        assert!(used[1].skipped.is_none());
        // Claude Code's model as the writer applies to fix-claude-code, and
        // not to fix's native writer.
        let claude = models(Some("anthropic:opus"), Some("ollama:gpt-oss:20b"));
        let mut values = ParamValues::new();
        apply_run_model_defaults(&file("fix-claude-code"), &claude, &mut values);
        assert_eq!(values["writer_model"], "anthropic:opus");
        let mut values = ParamValues::new();
        let used = apply_run_model_defaults(&file("fix"), &claude, &mut values);
        assert!(!values.contains_key("writer_model"));
        assert_eq!(
            used[0].skipped.as_deref(),
            Some("loadout fix's writer runs natively, on Ollama or OpenRouter")
        );
        let mut values = ParamValues::new();
        apply_run_model_defaults(&file("fix-codex"), &claude, &mut values);
        assert!(!values.contains_key("writer_model"));
        assert_eq!(values["reviewer_model"], "ollama:gpt-oss:20b");
    }
}
