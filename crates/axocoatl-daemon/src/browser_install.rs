//! `axocoatl browser install`: build the browser tools' image from the
//! Containerfile and lock files embedded in Axocoatl. The build downloads the
//! pinned Node base image, the locked Playwright packages and Playwright's
//! Chromium build; it is network access the user starts, on the host.
//!
//! `axocoatl recipe build` uses the same builder for Session images composed
//! from Axocoatl's pinned recipes ([`build_recipe_image`]).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use axocoatl_isolation::recipes;
use axocoatl_tools::browser_tool::{
    BROWSER_CONTAINERFILE, BROWSER_PACKAGE_JSON, BROWSER_PACKAGE_LOCK, PLAYWRIGHT_VERSION,
};
use tokio::process::Command;

pub use axocoatl_config::DEFAULT_BROWSER_IMAGE;

/// The files of the build context, by name.
pub fn build_context_files() -> [(&'static str, &'static str); 3] {
    [
        ("Containerfile", BROWSER_CONTAINERFILE),
        ("package.json", BROWSER_PACKAGE_JSON),
        ("package-lock.json", BROWSER_PACKAGE_LOCK),
    ]
}

/// The `podman build` arguments for `image` from `context` (pure).
pub fn build_args(image: &str, context: &Path) -> Vec<String> {
    vec![
        "build".into(),
        "-t".into(),
        image.into(),
        "--label".into(),
        "io.axocoatl.role=browser".into(),
        "--label".into(),
        format!("io.axocoatl.playwright={PLAYWRIGHT_VERSION}"),
        "-f".into(),
        context.join("Containerfile").to_string_lossy().into_owned(),
        context.to_string_lossy().into_owned(),
    ]
}

fn valid_image(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 512
        && !image.starts_with('-')
        && !image.chars().any(|c| c.is_whitespace() || c.is_control())
}

struct ContextDir(PathBuf);

impl Drop for ContextDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_context() -> std::io::Result<ContextDir> {
    write_context_files("browser", &build_context_files())
}

/// A private (0700) temporary build context holding exactly `files`,
/// removed when dropped.
fn write_context_files(kind: &str, files: &[(&str, &str)]) -> std::io::Result<ContextDir> {
    let directory =
        std::env::temp_dir().join(format!("axocoatl-{kind}-build-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)?;
    let context = ContextDir(directory);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&context.0, std::fs::Permissions::from_mode(0o700))?;
    }
    for (name, contents) in files {
        std::fs::write(context.0.join(name), contents)?;
    }
    Ok(context)
}

/// Whether `image` exists in Podman's local storage.
pub async fn image_present(image: &str) -> Result<bool, String> {
    let status = Command::new("podman")
        .args(["image", "exists", "--", image])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|error| format!("running podman: {error}"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("podman could not check the image".into()),
    }
}

/// Build the image, showing Podman's progress on this terminal, and return
/// its id.
pub async fn install(image: &str) -> Result<String, String> {
    if !valid_image(image) {
        return Err(format!("{image:?} is not an image reference"));
    }
    let context = write_context().map_err(|error| format!("writing the build context: {error}"))?;
    run_build(build_args(image, &context.0)).await?;
    image_id(image).await
}

async fn run_build(args: Vec<String>) -> Result<(), String> {
    let status = Command::new("podman")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .await
        .map_err(|error| format!("running podman build: {error}"))?;
    if !status.success() {
        return Err(format!("podman build failed ({status})"));
    }
    Ok(())
}

/// The full id of local image `image`.
pub async fn image_id(image: &str) -> Result<String, String> {
    image_field(image, "{{.Id}}").await
}

async fn image_field(image: &str, format: &str) -> Result<String, String> {
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", format, "--", image])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("inspecting the image: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The `podman build` arguments for the recipe image of `names` from
/// `context` (pure): tagged with its recipe image name and labeled with its
/// recipes and Containerfile digest. The base is pulled when missing; the
/// Containerfile pins it by digest.
pub fn recipe_build_args(names: &[String], context: &Path) -> Result<Vec<String>, String> {
    let image = recipes::image_name(names).map_err(|error| error.to_string())?;
    let mut args = vec!["build".into(), "--pull=missing".into(), "-t".into(), image];
    for (key, value) in recipes::labels(names).map_err(|error| error.to_string())? {
        args.push("--label".into());
        args.push(format!("{key}={value}"));
    }
    args.push("-f".into());
    args.push(context.join("Containerfile").to_string_lossy().into_owned());
    args.push(context.to_string_lossy().into_owned());
    Ok(args)
}

/// Build the Session image of `names` from Axocoatl's pinned recipes,
/// showing Podman's progress on this terminal, check that the image Podman
/// tagged carries this exact Containerfile's digest, and return its record
/// (to keep with `recipe_images::record_image`).
pub async fn build_recipe_image(
    names: &[String],
) -> Result<crate::external_agent::recipe_images::RecipeImageRecord, String> {
    let containerfile = recipes::compose(names).map_err(|error| error.to_string())?;
    let image = recipes::image_name(names).map_err(|error| error.to_string())?;
    let digest = recipes::digest(names).map_err(|error| error.to_string())?;
    let context = write_context_files("recipe", &[("Containerfile", containerfile.as_str())])
        .map_err(|error| format!("writing the build context: {error}"))?;
    run_build(recipe_build_args(names, &context.0)?).await?;
    let labeled = image_field(
        &image,
        &format!("{{{{ index .Labels {:?} }}}}", recipes::RECIPE_DIGEST_LABEL),
    )
    .await?;
    if labeled != digest {
        return Err(format!(
            "{image} does not carry this Containerfile's digest after the build"
        ));
    }
    let id = image_id(&image).await?;
    let built_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    crate::external_agent::recipe_images::record_for(names, &id, built_at_ms)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_uses_the_embedded_pinned_context() {
        let args = build_args(DEFAULT_BROWSER_IMAGE, Path::new("/tmp/ctx"));
        assert_eq!(
            &args[..3],
            ["build", "-t", "localhost/axocoatl-browser:pw1.60.0"]
        );
        assert!(args.contains(&"io.axocoatl.role=browser".to_string()));
        assert!(args.contains(&"io.axocoatl.playwright=1.60.0".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("/tmp/ctx"));
        let files = build_context_files();
        assert!(files[0]
            .1
            .contains("FROM docker.io/library/node:22-bookworm-slim@sha256:"));
        assert!(files[0].1.contains("USER node"));
        assert!(files[2].1.contains("\"lockfileVersion\": 3"));
        assert!(!valid_image(""));
        assert!(!valid_image("--rm"));
        assert!(!valid_image("a b"));
        assert!(valid_image(DEFAULT_BROWSER_IMAGE));
        let context = write_context().unwrap();
        for (name, contents) in build_context_files() {
            assert_eq!(
                std::fs::read_to_string(context.0.join(name)).unwrap(),
                contents
            );
        }
        let path = context.0.clone();
        drop(context);
        assert!(!path.exists());
    }

    #[test]
    fn a_recipe_build_tags_and_labels_the_composed_containerfile() {
        let names = vec!["codex".to_string(), "claude-code".to_string()];
        let args = recipe_build_args(&names, Path::new("/tmp/ctx")).unwrap();
        let image = recipes::image_name(&names).unwrap();
        assert_eq!(
            &args[..4],
            ["build", "--pull=missing", "-t", image.as_str()]
        );
        assert!(args.contains(&"io.axocoatl.role=recipe".to_string()));
        assert!(args.contains(&"io.axocoatl.recipes=claude-code,codex".to_string()));
        assert!(args.contains(&format!(
            "io.axocoatl.recipe-digest={}",
            recipes::digest(&names).unwrap()
        )));
        assert_eq!(args.last().map(String::as_str), Some("/tmp/ctx"));
        assert!(recipe_build_args(&["nope".to_string()], Path::new("/tmp/ctx")).is_err());
        let containerfile = recipes::compose(&names).unwrap();
        let context =
            write_context_files("recipe", &[("Containerfile", containerfile.as_str())]).unwrap();
        assert_eq!(
            std::fs::read_to_string(context.0.join("Containerfile")).unwrap(),
            containerfile
        );
    }
}
