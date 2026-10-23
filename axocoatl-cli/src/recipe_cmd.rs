//! `axocoatl recipe build|list`: build Session images from Axocoatl's
//! pinned recipes (claude-code, codex, e2e, and the language toolchains
//! node, python, rust and go) with Podman. `axocoatl recipe clear-cache`
//! removes a Workspace's toolchain cache volume. Owner: workstream
//! `agents` (the e2e recipe file belongs to `e2e`).
//!
//! A build composes the base and the named fragments
//! (`axocoatl_isolation::recipes::compose`), builds the image under its
//! recipe name (`localhost/axocoatl-recipe-<names>:<digest>`) on this
//! computer's Podman, and records the exact image id in
//! `{data dir}/recipes/images.json`. A Session may then start from that image
//! id without `sandbox.allow_untrusted_images`; a loadout names the recipes
//! in `environment.recipes`.

use std::path::{Path, PathBuf};

use axocoatl_daemon::external_agent::recipe_images;
use axocoatl_isolation::recipes;
use clap::Subcommand;

const USAGE: i32 = 3;
const FAILURE: i32 = 5;

#[derive(Debug, Subcommand)]
pub enum RecipeCommands {
    /// Build one image from one or more recipes
    Build {
        /// Recipe names, such as `e2e`, `python` or `claude-code python`
        #[arg(required = true)]
        recipes: Vec<String>,
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
    /// List recipes and the images built from them
    List {
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
    /// Remove a repository's toolchain cache (the packages its runs'
    /// setups installed); its next run installs them again
    ClearCache {
        /// The repository whose cache to remove (default: the current directory)
        #[arg(long, value_name = "DIR", conflicts_with = "all")]
        repo: Option<PathBuf>,
        /// Remove the toolchain cache of every repository of this data directory
        #[arg(long)]
        all: bool,
        /// Path to config file (selects the data directory)
        #[arg(short, long, default_value_os_t = crate::default_config_path_for_clap())]
        config: PathBuf,
    },
}

fn data_dir(config: &Path) -> Result<PathBuf, String> {
    let data_dir = crate::configure_data_dir(config)?;
    axocoatl_daemon::AxocoatlDaemon::initialize_data_root(&data_dir)
        .map_err(|error| format!("opening the data directory: {error}"))?;
    Ok(data_dir)
}

/// The lines `recipe list` prints (pure).
pub(crate) fn list_lines(images: &[recipe_images::RecipeImageRecord]) -> Vec<String> {
    let mut lines = vec!["Recipes:".to_string()];
    for recipe in recipes::RECIPES {
        lines.push(format!("  {:<12} {}", recipe.name, recipe.description));
    }
    lines.push(
        "Language toolchains (one per image; compose with a recipe, such as `claude-code python`):"
            .into(),
    );
    for recipe in recipes::TOOLCHAIN_RECIPES {
        let base = recipes::toolchain_image(recipe.name)
            .map(|image| format!(" — {}, about {} MB", image.image, image.image_mb))
            .unwrap_or_default();
        lines.push(format!(
            "  {:<12} {}{base}",
            recipe.name, recipe.description
        ));
    }
    if images.is_empty() {
        lines.push(
            "No recipe images built. Build one with: axocoatl recipe build <recipe>...".into(),
        );
        return lines;
    }
    lines.push("Built images (trusted for Sessions by image id):".into());
    for image in images {
        lines.push(format!(
            "  {}  {}  id {}",
            image.recipes.join(" "),
            image.image,
            &image.image_id[..12.min(image.image_id.len())]
        ));
    }
    lines
}

/// `axocoatl recipe clear-cache`: remove the toolchain cache volume of the
/// repository `repo` (canonical), or with `all` every one of the daemon
/// whose data directory is `data_dir`, and say what was removed. A volume
/// a container still uses (an open Session of that repository) is left,
/// with what to do.
async fn clear_cache(data_dir: &Path, repo: Option<&Path>) -> i32 {
    let root = match axocoatl_core::SecureDir::open_existing_all(data_dir) {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "✗ the data directory {} cannot be opened: {error}",
                data_dir.display()
            );
            return FAILURE;
        }
    };
    if let Err(error) = crate::podman_preflight().await {
        eprintln!("✗ {error}");
        return FAILURE;
    }
    let authority = axocoatl_daemon::bootstrap::runtime_authority_of(&root);
    let present =
        match axocoatl_isolation::SessionSandbox::toolchain_cache_volumes(&authority).await {
            Ok(present) => present,
            Err(error) => {
                eprintln!("✗ {error}");
                return FAILURE;
            }
        };
    let targets: Vec<String> = match repo {
        Some(repo) => {
            let volume = axocoatl_daemon::bootstrap::toolchain_cache_volume_of(&root, repo);
            present.into_iter().filter(|name| *name == volume).collect()
        }
        None => present,
    };
    if targets.is_empty() {
        match repo {
            Some(repo) => println!(
                "No toolchain cache to remove for {}: no run of it has made one",
                repo.display()
            ),
            None => println!("No toolchain cache to remove"),
        }
        return 0;
    }
    let mut code = 0;
    for volume in targets {
        match axocoatl_isolation::SessionSandbox::remove_toolchain_cache_volume(&volume).await {
            Ok(()) => println!(
                "✓ Removed the toolchain cache {volume}; the next run installs its packages again"
            ),
            Err(error) => {
                let error = match error {
                    axocoatl_isolation::IsolationError::OciSetupFailed(message) => message,
                    other => other.to_string(),
                };
                if error.contains("being used") || error.contains("in use") {
                    eprintln!("✗ {}", in_use_line(&volume).await);
                } else {
                    eprintln!("✗ {error}");
                }
                code = FAILURE;
            }
        }
    }
    code
}

/// Why the toolchain cache `volume` was left: the Sessions whose container
/// still uses it, by id when Podman names them, and what to do.
async fn in_use_line(volume: &str) -> String {
    let sessions = axocoatl_isolation::SessionSandbox::volume_sessions(volume)
        .await
        .unwrap_or_default();
    let (whose, close) = if sessions.is_empty() {
        (
            "a container".to_string(),
            "`axocoatl session close SESSION_ID`".to_string(),
        )
    } else {
        (
            format!("the container of Session {}", sessions.join(", ")),
            sessions
                .iter()
                .map(|id| format!("`axocoatl session close {id}`"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    format!(
        "The toolchain cache {volume} was not removed: {whose} still uses it. Let the run on \
         this repository end, or close that Session (in the workbench, or {close}), then run \
         this again."
    )
}

/// Returns the process exit code.
pub async fn cmd_recipe(command: RecipeCommands) -> i32 {
    match command {
        RecipeCommands::ClearCache { repo, all, config } => {
            let repo = match (all, repo) {
                (true, _) => None,
                (false, repo) => {
                    let repo = repo.unwrap_or_else(|| PathBuf::from("."));
                    match std::fs::canonicalize(&repo) {
                        Ok(canonical) if canonical.is_dir() => Some(canonical),
                        Ok(_) => {
                            eprintln!("✗ {} is not a directory", repo.display());
                            return USAGE;
                        }
                        Err(error) => {
                            eprintln!("✗ {}: {error}", repo.display());
                            return USAGE;
                        }
                    }
                }
            };
            match data_dir(&config) {
                Ok(data_dir) => clear_cache(&data_dir, repo.as_deref()).await,
                Err(error) => {
                    eprintln!("✗ {error}");
                    FAILURE
                }
            }
        }
        RecipeCommands::Build { recipes, config } => {
            let image = match recipes::image_name(&recipes) {
                Ok(image) => image,
                Err(error) => {
                    eprintln!("✗ {error}");
                    return USAGE;
                }
            };
            let data_dir = match data_dir(&config) {
                Ok(data_dir) => data_dir,
                Err(error) => {
                    eprintln!("✗ {error}");
                    return FAILURE;
                }
            };
            // A stopped machine is started; a missing Podman or machine is
            // named with its fix instead of `podman build`'s own error.
            if let Err(error) = crate::podman_preflight().await {
                eprintln!("✗ {error}");
                return FAILURE;
            }
            eprintln!(
                "Building {image} with Podman. This downloads the pinned base image and the \
                 pinned packages of: {}",
                recipes::canonical_names(&recipes)
                    .map(|names| names.join(", "))
                    .unwrap_or_default()
            );
            let record = match axocoatl_daemon::browser_install::build_recipe_image(&recipes).await
            {
                Ok(record) => record,
                Err(error) => {
                    eprintln!("✗ {error}");
                    return FAILURE;
                }
            };
            if let Err(error) = recipe_images::record_image_at(&data_dir, record.clone()) {
                eprintln!("✗ the image was built but could not be recorded: {error}");
                return FAILURE;
            }
            println!("✓ Built {} (image id {})", record.image, record.image_id);
            println!(
                "  Sessions may start from this image id; a loadout names it with \
                 environment: {{ recipes: [{}] }}",
                record.recipes.join(", ")
            );
            0
        }
        RecipeCommands::List { config } => {
            let data_dir = match data_dir(&config) {
                Ok(data_dir) => data_dir,
                Err(error) => {
                    eprintln!("✗ {error}");
                    return FAILURE;
                }
            };
            match recipe_images::recorded_images_at(&data_dir) {
                Ok(images) => {
                    for line in list_lines(&images) {
                        println!("{line}");
                    }
                    0
                }
                Err(error) => {
                    eprintln!("✗ {error}");
                    FAILURE
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: RecipeCommands,
    }

    #[test]
    fn clear_cache_takes_one_repository_or_all() {
        let parsed = Cli::try_parse_from(["x", "clear-cache"]).unwrap();
        assert!(matches!(
            parsed.command,
            RecipeCommands::ClearCache {
                repo: None,
                all: false,
                ..
            }
        ));
        let parsed = Cli::try_parse_from(["x", "clear-cache", "--repo", "/src/app"]).unwrap();
        assert!(
            matches!(parsed.command, RecipeCommands::ClearCache { repo: Some(repo), .. }
            if repo == Path::new("/src/app"))
        );
        assert!(matches!(
            Cli::try_parse_from(["x", "clear-cache", "--all"])
                .unwrap()
                .command,
            RecipeCommands::ClearCache { all: true, .. }
        ));
        assert!(Cli::try_parse_from(["x", "clear-cache", "--all", "--repo", "."]).is_err());
    }

    #[test]
    fn build_needs_recipe_names_and_list_names_every_recipe() {
        assert!(Cli::try_parse_from(["x", "build"]).is_err());
        let parsed = Cli::try_parse_from(["x", "build", "claude-code", "e2e"]).unwrap();
        assert!(
            matches!(parsed.command, RecipeCommands::Build { recipes, .. }
            if recipes == ["claude-code", "e2e"])
        );
        let lines = list_lines(&[]);
        for recipe in recipes::RECIPES
            .iter()
            .chain(recipes::TOOLCHAIN_RECIPES.iter())
        {
            assert!(lines.iter().any(|line| line.contains(recipe.name)));
        }
        assert!(lines
            .iter()
            .any(|line| line.contains("python:3.13.16-slim-bookworm")));
        let parsed = Cli::try_parse_from(["x", "build", "claude-code", "python"]).unwrap();
        assert!(
            matches!(parsed.command, RecipeCommands::Build { recipes, .. }
            if recipes == ["claude-code", "python"])
        );
        let record =
            recipe_images::record_for(&["claude-code".to_string()], &"ab".repeat(32), 1).unwrap();
        let lines = list_lines(std::slice::from_ref(&record));
        assert!(lines
            .iter()
            .any(|line| line.contains(&record.image) && line.contains("id abababababab")));
    }

    /// `axocoatl recipe build claude-code` and `codex` with Podman: each
    /// image carries its Containerfile digest, its record makes exactly its
    /// image id trusted, and the pinned programs print their pinned
    /// versions inside it with no network.
    ///
    /// ```text
    /// CONTAINER_CONNECTION=axocoatl-ci-pr74 \
    ///   cargo test -p axocoatl-cli actual_recipe_builds -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires Podman (CONTAINER_CONNECTION) and network access for the pinned packages"]
    async fn actual_recipe_builds_record_trusted_pinned_images() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        for (recipe, command, version) in [
            ("claude-code", "claude", "2.1.292"),
            ("codex", "codex", "0.160.1"),
        ] {
            let names = vec![recipe.to_string()];
            let record = axocoatl_daemon::browser_install::build_recipe_image(&names)
                .await
                .unwrap();
            assert_eq!(record.image, recipes::image_name(&names).unwrap());
            recipe_images::record_image_at(data.path(), record.clone()).unwrap();
            let root = axocoatl_core::SecureDir::open(data.path()).unwrap();
            assert!(recipe_images::is_trusted_image(&root, &record.image_id));
            let output = std::process::Command::new("podman")
                .args([
                    "run",
                    "--rm",
                    "--pull=never",
                    "--network",
                    "none",
                    "--user",
                    "1000:1000",
                    "-e",
                    "HOME=/tmp",
                    &record.image_id,
                    command,
                    "--version",
                ])
                .output()
                .unwrap();
            let printed = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{recipe}: {output:?}");
            assert!(printed.contains(version), "{recipe}: {printed}");
            let node = std::process::Command::new("podman")
                .args([
                    "run",
                    "--rm",
                    "--pull=never",
                    "--network",
                    "none",
                    &record.image_id,
                    "node",
                    "--version",
                ])
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&node.stdout).starts_with("v24."));
        }
        assert_eq!(
            recipe_images::recorded_images_at(data.path())
                .unwrap()
                .len(),
            2
        );
    }
}
