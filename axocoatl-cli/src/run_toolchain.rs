//! `axocoatl run`'s toolchain image, before admission: plan the run's
//! environment the way the daemon's admission does
//! (`axocoatl_daemon::loadout::toolchain::plan`), and when its recipe image
//! is not built on this computer, build it here so Podman's progress shows
//! on this terminal's standard error, saying what it builds and about how
//! large it is. Before it builds, the daemon checks the run as `--dry-run`
//! does, so a run that cannot start (a credential that is not stored, a
//! reviewer that cannot review) is refused without building hundreds of
//! megabytes first. With `--no-build` nothing is built and the daemon
//! refuses the run, naming the `axocoatl recipe build` command. Owner:
//! workstream `toolchain`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use axocoatl_daemon::external_agent::recipe_images;
use axocoatl_daemon::loadout::api::LoadoutView;
use axocoatl_daemon::loadout::toolchain::{self as toolchain, EnvironmentRequest, PlanInput};

use super::{Console, Daemon, Failure, RunPlan};

/// The data directory the daemon of `config` uses (as `axocoatl recipe
/// build` finds it), without changing this process's environment.
fn data_dir(config: &Path) -> PathBuf {
    std::env::var_os("AXOCOATL_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::default_data_dir_for_config(config))
}

/// What a build of `recipes` downloads and takes, in words: the official
/// image it starts from and its download, the measured size on disk of the
/// whole image (the toolchain and every other recipe), and what the other
/// recipes download on top.
pub(crate) fn build_line(image: &str, recipes: &[String]) -> String {
    let what = recipes.join(" + ");
    match axocoatl_isolation::recipes::image_size(recipes) {
        Ok(size) => {
            let more = if size.also_downloads.is_empty() {
                String::new()
            } else {
                format!(" plus {}", size.also_downloads.join(" and "))
            };
            let disk = if size.unmeasured.is_empty() {
                format!("about {} MB on disk", size.disk_mb)
            } else {
                format!(
                    "at least {} MB on disk (more for {})",
                    size.disk_mb,
                    size.unmeasured.join(" and ")
                )
            };
            format!(
                "· building the run's image {image} ({what}, from {}): about {} MB to download{more}, \
                 {disk}, once on this computer; later runs reuse it",
                size.base_image, size.base_download_mb
            )
        }
        Err(_) => format!(
            "· building the run's image {image} ({what}); once on this computer, later runs \
             reuse it"
        ),
    }
}

/// Build the run's toolchain image when it is missing, building is allowed
/// and the daemon finds nothing else that stops the run (`request` is the
/// run's `POST /api/runs` body). A loadout the daemon cannot show is left
/// to admission, which decides the same way and says why.
pub(super) async fn prepare_image(
    daemon: &Daemon,
    plan: &RunPlan,
    request: &serde_json::Value,
    config: &Path,
    console: &mut Console,
) -> Result<(), Failure> {
    let Ok(view) = daemon
        .get_json::<LoadoutView>(&format!("/api/loadouts/{}", plan.loadout))
        .await
    else {
        return Ok(());
    };
    let environment = toolchain::plan(
        &plan.repo,
        PlanInput {
            file: &view.file,
            request: &plan.environment,
            check_flag: plan.check_command.as_deref(),
            setup_flag: plan.setup_command.as_deref(),
            // A loadout's own setup changes no image.
            loadout_setup: None,
        },
    )
    .map_err(|message| Failure {
        code: axocoatl_session::run_outcome::exit_code::USAGE,
        message,
    })?;
    if environment.image.is_some() || environment.recipes.is_empty() {
        return Ok(());
    }
    let data_dir = data_dir(config);
    let built = axocoatl_core::SecureDir::open_existing_all(&data_dir)
        .ok()
        .and_then(|root| recipe_images::image_for_recipes(&root, &environment.recipes).ok());
    if built.is_some() || !plan.environment.build_images {
        return Ok(());
    }
    refuse_before_build(daemon, request).await?;
    let image = environment.image_text();
    console.note(&build_line(&image, &environment.recipes));
    let started = Instant::now();
    let record =
        axocoatl_daemon::browser_install::build_recipe_image_on_stderr(&environment.recipes)
            .await
            .map_err(|error| Failure {
                code: axocoatl_session::run_outcome::exit_code::INFRASTRUCTURE,
                message: format!(
            "building the run's image {image} failed: {error}. Its Podman output is above; build \
             it again with `{}`",
            environment
                .build_command()
                .unwrap_or_else(|| "axocoatl recipe build <recipes>".into())
        ),
            })?;
    recipe_images::record_image_at(&data_dir, record.clone()).map_err(|error| Failure {
        code: axocoatl_session::run_outcome::exit_code::INFRASTRUCTURE,
        message: format!(
            "the image {} was built but could not be recorded: {error}",
            record.image
        ),
    })?;
    console.note(&format!(
        "· built {} in {:.0} s (image id {})",
        record.image,
        started.elapsed().as_secs_f64(),
        &record.image_id[..12.min(record.image_id.len())]
    ));
    Ok(())
}

/// Check the run `request` as `--dry-run` does, loading no model, before
/// its image is built: a problem that stops it is refused now, in the words
/// and with the exit code admission would use, and nothing is built. A
/// daemon without a dry run (older than 1.5) leaves it to admission.
async fn refuse_before_build(daemon: &Daemon, request: &serde_json::Value) -> Result<(), Failure> {
    let mut dry = request.clone();
    dry["load_models"] = serde_json::Value::Bool(false);
    match super::try_preflight(daemon, &dry).await? {
        super::Preflight::Checked(preflight) if !preflight.ready => Err(Failure {
            code: preflight.exit_code,
            message: axocoatl_daemon::loadout::admission::refusal_text(&preflight.problems),
        }),
        _ => Ok(()),
    }
}

/// The `environment` of `POST /api/runs` from the flags.
pub(super) fn request(
    toolchain: Option<&str>,
    image: Option<&str>,
    no_setup: bool,
    no_build: bool,
    no_probe: bool,
) -> EnvironmentRequest {
    EnvironmentRequest {
        toolchain: toolchain.map(str::to_string),
        image: image.map(str::to_string),
        no_setup,
        no_probe,
        build_images: !no_build,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_line_says_what_and_how_large() {
        let line = build_line(
            "localhost/axocoatl-recipe-python:eb24246cd089",
            &["python".to_string()],
        );
        assert!(
            line.contains("localhost/axocoatl-recipe-python:eb24246cd089"),
            "{line}"
        );
        assert!(
            line.contains("docker.io/library/python:3.13.16-slim-bookworm"),
            "{line}"
        );
        assert!(
            line.contains("about 65 MB to download, about 250 MB on disk"),
            "{line}"
        );
        // TC-NU-6: a composed image says its whole size.
        let composed = build_line("x", &["claude-code".to_string(), "python".to_string()]);
        assert!(composed.contains("claude-code + python"), "{composed}");
        assert!(composed.contains("python:3.13.16"), "{composed}");
        assert!(
            composed.contains("plus Node.js and claude-code, about 645 MB on disk"),
            "{composed}"
        );
        let codex = build_line("y", &["codex".to_string()]);
        assert!(
            codex.contains("(codex, from docker.io/library/node:"),
            "{codex}"
        );
        let e2e = build_line("z", &["e2e".to_string(), "go".to_string()]);
        assert!(e2e.contains("at least"), "{e2e}");
        assert_eq!(
            request(None, None, false, false, false),
            EnvironmentRequest::default()
        );
        let flags = request(Some("go"), None, true, true, true);
        assert_eq!(flags.toolchain.as_deref(), Some("go"));
        assert!(flags.no_setup && !flags.build_images && flags.no_probe);
    }
}
