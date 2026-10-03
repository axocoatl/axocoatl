//! `axocoatl browser install`: build the browser tools' image from the
//! Containerfile and lock files embedded in Axocoatl. The build downloads the
//! pinned Node base image, the locked Playwright packages and Playwright's
//! Chromium build; it is network access the user starts, on the host.

use std::path::{Path, PathBuf};
use std::process::Stdio;

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
    let directory =
        std::env::temp_dir().join(format!("axocoatl-browser-build-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)?;
    let context = ContextDir(directory);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&context.0, std::fs::Permissions::from_mode(0o700))?;
    }
    for (name, contents) in build_context_files() {
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
    let status = Command::new("podman")
        .args(build_args(image, &context.0))
        .stdin(Stdio::null())
        .status()
        .await
        .map_err(|error| format!("running podman build: {error}"))?;
    if !status.success() {
        return Err(format!("podman build failed ({status})"));
    }
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Id}}", "--", image])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("inspecting the image: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
}
