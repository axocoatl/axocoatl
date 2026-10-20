//! The next step `axocoatl connect` prints once a sign-in is stored: the run
//! that sign-in is for, as `axocoatl run` would take it now. Owner:
//! workstream `admission`.
//!
//! It names the built-in loadout (`fix-claude-code` or `fix-codex`), says to
//! build the recipe image only when none is recorded, and names the reviewer:
//! the configured `run.models.reviewer` when there is one (the command then
//! needs no `--model`), else the `--model reviewer=…` the run needs. The
//! command is [`crate::run_cmd::run_command`]'s, so it is the one
//! `axocoatl loadouts show` prints.

use std::path::Path;

use axocoatl_config::loadout::{builtin_loadouts, ParamValues};
use axocoatl_config::run_defaults::apply_run_model_defaults;
use axocoatl_config::RunModelsYaml;

/// `run.models` of the configuration at `config`; none when it cannot be
/// read.
pub(crate) fn configured_run_models(config: &Path) -> RunModelsYaml {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|text| axocoatl_config::parse_config(&text, config).ok())
        .map(|config| config.run.models)
        .unwrap_or_default()
}

/// Whether the image built from `recipe` alone is recorded in `data_dir`,
/// as `axocoatl recipe build` records it.
pub(crate) fn image_recorded(data_dir: &Path, recipe: &str) -> bool {
    let Ok(image) = axocoatl_isolation::recipes::image_name(&[recipe.to_string()]) else {
        return false;
    };
    axocoatl_daemon::external_agent::recipe_images::recorded_images_at(data_dir)
        .map(|records| records.iter().any(|record| record.image == image))
        .unwrap_or(false)
}

/// The next step after a sign-in for the built-in `loadout`, whose writer
/// runs in the image of `recipe`: build the image when `image_built` is
/// false, start the daemon, then the exact `axocoatl run` command, with the
/// reviewer `models` gives it or the `--model reviewer=…` it needs.
pub(crate) fn next_step(
    loadout: &str,
    recipe: &str,
    image_built: bool,
    models: &RunModelsYaml,
) -> String {
    let Some(found) = builtin_loadouts()
        .into_iter()
        .filter_map(Result::ok)
        .find(|found| found.file.id == loadout)
    else {
        return String::new();
    };
    let mut given = ParamValues::new();
    let defaults = apply_run_model_defaults(&found.file, models, &mut given);
    let mut summary = axocoatl_daemon::bootstrap::loadout_runs::loadout_summary(&found);
    // A parameter the configuration gives is not needed on the command line.
    for param in &mut summary.params {
        if given.contains_key(&param.name) {
            param.required = false;
        }
    }
    let command = crate::run_cmd::run_command(&summary);
    let build = if image_built {
        String::new()
    } else {
        format!("build the Session image once with `axocoatl recipe build {recipe}`, then ")
    };
    let reviewer = match defaults
        .iter()
        .find(|default| default.param == "reviewer_model" && default.skipped.is_none())
    {
        Some(default) => format!(
            " Its reviewer is {}, from run.models.reviewer in your configuration.",
            default.value
        ),
        None => " Name the reviewer, a different model that only reads the change, on \
                  Ollama (such as ollama:gpt-oss:20b) or OpenRouter; `axocoatl onboard` can \
                  save one as run.models.reviewer, and the command then needs no --model."
            .to_string(),
    };
    format!(
        "Next: {build}start the daemon if it is not running (`axocoatl dev`), then run in your \
         repository: `{command}`.{reviewer}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviewer(model: &str) -> RunModelsYaml {
        RunModelsYaml {
            writer: None,
            reviewer: Some(model.to_string()),
        }
    }

    /// Before the image is built and with no reviewer configured, the step
    /// builds the image and names the reviewer the run needs; once both
    /// exist, it is one command that runs as written.
    #[test]
    fn the_next_step_names_the_loadout_the_image_and_the_reviewer() {
        let step = next_step(
            "fix-claude-code",
            "claude-code",
            false,
            &RunModelsYaml::default(),
        );
        assert_eq!(
            step,
            "Next: build the Session image once with `axocoatl recipe build claude-code`, then \
             start the daemon if it is not running (`axocoatl dev`), then run in your \
             repository: `axocoatl run fix-claude-code --task \"...\" --model \
             reviewer=provider:model`. Name the reviewer, a different model that only reads the \
             change, on Ollama (such as ollama:gpt-oss:20b) or OpenRouter; `axocoatl onboard` \
             can save one as run.models.reviewer, and the command then needs no --model."
        );
        let step = next_step("fix-codex", "codex", true, &reviewer("ollama:gpt-oss:20b"));
        assert_eq!(
            step,
            "Next: start the daemon if it is not running (`axocoatl dev`), then run in your \
             repository: `axocoatl run fix-codex --task \"...\"`. Its reviewer is \
             ollama:gpt-oss:20b, from run.models.reviewer in your configuration."
        );
    }

    #[test]
    fn the_configuration_and_the_image_record_are_read_where_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.yaml");
        assert_eq!(configured_run_models(&config), RunModelsYaml::default());
        std::fs::write(
            &config,
            "run:\n  models:\n    reviewer: \"ollama:gpt-oss:20b\"\n",
        )
        .unwrap();
        assert_eq!(
            configured_run_models(&config),
            reviewer("ollama:gpt-oss:20b")
        );
        // No data root, or no record: not built.
        assert!(!image_recorded(&dir.path().join("data"), "claude-code"));
    }
}
