//! The egress sidecar image: `FROM scratch` with only the static execution
//! supervisor. It is never the Session image, it has no shell, and the
//! sidecar running it never resolves names or executes anything.

use std::time::Duration;

use axocoatl_core::SecureDir;
use tokio::process::Command;

use crate::{IsolationError, SessionSandbox};

const BUILD_TIMEOUT: Duration = Duration::from_secs(300);
const INSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const PROGRAM_LABEL: &str = "io.axocoatl.program-sha256";
const CONTAINERFILE: &str =
    "FROM scratch\nCOPY rootfs/ /\nCOPY axocoatl-exec-supervisor /axocoatl-exec-supervisor\n";

fn failed(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::OciSetupFailed(format!("egress image: {message}"))
}

/// `localhost/axocoatl-egress:{first 16 hex of the payload sha256}-{arch}`.
pub fn egress_image_tag(program_sha256: &str, architecture: &str) -> String {
    let short: String = program_sha256.chars().take(16).collect();
    format!("localhost/axocoatl-egress:{short}-{architecture}")
}

fn platform(architecture: &str) -> Result<&'static str, IsolationError> {
    match architecture {
        "x86_64" => Ok("linux/amd64"),
        "aarch64" => Ok("linux/arm64"),
        other => Err(failed(format!("no bundled supervisor for {other}"))),
    }
}

/// The Linux architecture of the machine Podman runs containers on, in the
/// supervisor's naming (`x86_64` or `aarch64`).
pub async fn podman_architecture() -> Result<String, IsolationError> {
    let mut info = Command::new("podman");
    info.args(["info", "--format", "{{.Host.Arch}}"]);
    let output = SessionSandbox::run_bounded_command(info, INSPECT_TIMEOUT).await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "reading Podman's architecture: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "amd64" | "x86_64" => Ok("x86_64".into()),
        "arm64" | "aarch64" => Ok("aarch64".into()),
        other => Err(failed(format!("no bundled supervisor for {other}"))),
    }
}

/// The `podman build` arguments for the image (pure).
pub fn build_args(
    tag: &str,
    program_sha256: &str,
    architecture: &str,
    context: &str,
) -> Result<Vec<String>, IsolationError> {
    Ok(vec![
        "build".into(),
        "--pull=never".into(),
        "--platform".into(),
        platform(architecture)?.into(),
        "--label".into(),
        "io.axocoatl.role=egress".into(),
        "--label".into(),
        format!("{PROGRAM_LABEL}={program_sha256}"),
        "-t".into(),
        tag.into(),
        "-f".into(),
        format!("{context}/Containerfile"),
        context.into(),
    ])
}

async fn labelled_program(tag: &str) -> Result<Option<String>, IsolationError> {
    let mut inspect = Command::new("podman");
    inspect.args([
        "image",
        "inspect",
        "--format",
        &format!("{{{{index .Labels \"{PROGRAM_LABEL}\"}}}}"),
        "--",
        tag,
    ]);
    let output = SessionSandbox::run_bounded_command(inspect, INSPECT_TIMEOUT).await?;
    if output.timed_out {
        return Err(failed("inspecting the image timed out"));
    }
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Build the sidecar image for one supervisor payload unless an image with
/// that tag and payload label already exists. `context` is a private
/// directory the build context is written to. Returns the tag.
pub async fn ensure_egress_image(
    architecture: &str,
    context: &SecureDir,
) -> Result<String, IsolationError> {
    let embedded = crate::supervisor_embedded::payload(architecture)?;
    embedded.verify_protocol()?;
    let tag = egress_image_tag(embedded.sha256, architecture);
    if labelled_program(&tag).await?.as_deref() == Some(embedded.sha256) {
        return Ok(tag);
    }
    let directory = context.child(format!("{}-{architecture}", &embedded.sha256[..16]))?;
    directory.atomic_write_with_mode("axocoatl-exec-supervisor", embedded.bytes, 0o555)?;
    directory.atomic_write_with_mode("Containerfile", CONTAINERFILE.as_bytes(), 0o644)?;
    // Scratch has no shell: the mount points a read-only root needs must
    // ship in the image.
    for mount in ["rootfs/run/axocoatl-egress", "rootfs/tmp"] {
        directory
            .child(mount)?
            .atomic_write_with_mode(".keep", b"", 0o644)?;
    }
    let mut build = Command::new("podman");
    build.args(build_args(
        &tag,
        embedded.sha256,
        architecture,
        &directory.path().to_string_lossy(),
    )?);
    let output = SessionSandbox::run_bounded_command(build, BUILD_TIMEOUT).await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "podman build {}: {}",
            if output.timed_out {
                "timed out"
            } else {
                "failed"
            },
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    match labelled_program(&tag).await? {
        Some(label) if label == embedded.sha256 => Ok(tag),
        other => Err(failed(format!(
            "the built image carries program label {other:?}, not the bundled payload"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_and_build_arguments_name_the_exact_payload() {
        let sha = "866a01dbe536610719d57a76c24b808016dcec05200cce194b9d8ec5035b8c94";
        let tag = egress_image_tag(sha, "aarch64");
        assert_eq!(tag, "localhost/axocoatl-egress:866a01dbe5366107-aarch64");
        let args = build_args(&tag, sha, "aarch64", "/private/ctx").unwrap();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--platform", "linux/arm64"]));
        assert!(args.contains(&"--pull=never".to_string()));
        assert!(args.contains(&format!("io.axocoatl.program-sha256={sha}")));
        assert_eq!(args.last().map(String::as_str), Some("/private/ctx"));
        assert!(build_args(&tag, sha, "riscv64", "/ctx").is_err());
        assert!(CONTAINERFILE.starts_with("FROM scratch\n"));
        assert!(!CONTAINERFILE.contains("RUN"));
    }
}
