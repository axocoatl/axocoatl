//! The egress sidecar's image: `FROM scratch` with only the verified execution
//! supervisor. It is never the Session image, has no shell and no resolver,
//! and ships the empty directories that `--read-only` needs as mount points.

use std::time::Duration;

use tokio::process::Command;

use crate::error::IsolationError;
use crate::supervisor_program::SupervisorProgram;
use crate::SessionSandbox;

/// Repository of the egress image; the tag carries the payload identity.
pub const EGRESS_IMAGE_REPOSITORY: &str = "localhost/axocoatl-egress";
/// Role label on every egress object.
pub const ROLE_LABEL: &str = "io.axocoatl.role";
/// Payload identity label on the egress image.
pub const PROGRAM_SHA256_LABEL: &str = "io.axocoatl.program-sha256";

const BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const INSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const CONTAINERFILE: &str =
    "FROM scratch\nCOPY rootfs/ /\nCOPY axocoatl-exec-supervisor /axocoatl-exec-supervisor\n";

fn failed(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::OciSetupFailed(format!("preparing the egress proxy image: {message}"))
}

/// `linux/arm64` or `linux/amd64` for an embedded payload architecture.
pub fn platform(architecture: &str) -> Result<&'static str, IsolationError> {
    match architecture {
        "aarch64" => Ok("linux/arm64"),
        "x86_64" => Ok("linux/amd64"),
        other => Err(failed(format!(
            "no egress image for architecture {other:?}"
        ))),
    }
}

/// `localhost/axocoatl-egress:{first 16 hex of the payload sha256}-{arch}`.
pub fn egress_image_tag(program_sha256: &str, architecture: &str) -> String {
    let short: String = program_sha256.chars().take(16).collect();
    format!("{EGRESS_IMAGE_REPOSITORY}:{short}-{architecture}")
}

/// The `podman build` arguments for the context at `context` (pure).
pub fn build_args(
    tag: &str,
    program_sha256: &str,
    architecture: &str,
    context: &str,
) -> Vec<String> {
    vec![
        "build".into(),
        "--pull=never".into(),
        "--quiet".into(),
        "--platform".into(),
        platform(architecture).unwrap_or("linux/arm64").into(),
        "--label".into(),
        format!("{ROLE_LABEL}=egress"),
        "--label".into(),
        format!("{PROGRAM_SHA256_LABEL}={program_sha256}"),
        "--tag".into(),
        tag.into(),
        "--file".into(),
        format!("{context}/Containerfile"),
        context.into(),
    ]
}

fn image_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Whether `tag` exists and carries exactly this payload's labels.
async fn labelled(tag: &str, program_sha256: &str) -> Result<bool, IsolationError> {
    let mut inspect = Command::new("podman");
    inspect.args([
        "image",
        "inspect",
        "--format",
        &format!(
            "{{{{ index .Labels {PROGRAM_SHA256_LABEL:?} }}}} {{{{ index .Labels {ROLE_LABEL:?} }}}}"
        ),
        "--",
        tag,
    ]);
    let output = SessionSandbox::run_bounded_command(inspect, INSPECT_TIMEOUT).await?;
    if output.timed_out {
        return Err(failed("inspecting the image timed out"));
    }
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == format!("{program_sha256} egress"))
}

#[cfg(unix)]
fn write_context(
    program: &SupervisorProgram,
    context: &axocoatl_core::SecureDir,
) -> Result<(), IsolationError> {
    use std::os::unix::fs::PermissionsExt;
    program.verify()?;
    let bytes = std::fs::read(program.path()).map_err(failed)?;
    {
        use sha2::Digest;
        if format!("{:x}", sha2::Sha256::digest(&bytes)) != program.sha256() {
            return Err(failed(
                "the installed supervisor changed while it was copied",
            ));
        }
    }
    context
        .atomic_write_with_mode("Containerfile", CONTAINERFILE.as_bytes(), 0o644)
        .map_err(failed)?;
    context
        .atomic_write_with_mode("axocoatl-exec-supervisor", &bytes, 0o555)
        .map_err(failed)?;
    for (directory, mode) in [
        ("rootfs", 0o755),
        ("rootfs/run", 0o755),
        ("rootfs/run/axocoatl-egress", 0o755),
        ("rootfs/tmp", 0o1777),
    ] {
        let child = context.child(directory).map_err(failed)?;
        std::fs::set_permissions(child.path(), std::fs::Permissions::from_mode(mode))
            .map_err(failed)?;
    }
    for keep in ["rootfs/run/axocoatl-egress/.keep", "rootfs/tmp/.keep"] {
        context
            .atomic_write_with_mode(keep, b"", 0o644)
            .map_err(failed)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_context(
    _program: &SupervisorProgram,
    _context: &axocoatl_core::SecureDir,
) -> Result<(), IsolationError> {
    Err(failed("the egress image needs a Unix host"))
}

/// Build (once) and return the egress image for this exact payload. The
/// build context is a private directory under the supervisor installation;
/// it holds only the verified payload and empty mount points.
pub async fn ensure_egress_image(program: &SupervisorProgram) -> Result<String, IsolationError> {
    let tag = egress_image_tag(program.sha256(), program.architecture());
    let _build = image_lock().lock().await;
    if labelled(&tag, program.sha256()).await? {
        return Ok(tag);
    }
    let name = format!("egress-image-{}", uuid::Uuid::new_v4().simple());
    let context = program.private_dir().create_child(&name).map_err(failed)?;
    let built = async {
        write_context(program, &context)?;
        let mut build = Command::new("podman");
        build.args(build_args(
            &tag,
            program.sha256(),
            program.architecture(),
            &context.path().to_string_lossy(),
        ));
        let output = SessionSandbox::run_bounded_command(build, BUILD_TIMEOUT).await?;
        if output.timed_out {
            return Err(failed("podman build timed out"));
        }
        if !output.status.success() {
            return Err(failed(format!(
                "podman build failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
    .await;
    if let Err(error) = program.private_dir().remove_dir_all(&name) {
        tracing::warn!(%error, "removing the egress image build context failed");
    }
    built?;
    if !labelled(&tag, program.sha256()).await? {
        return Err(failed(
            "the built image does not carry the payload's identity labels",
        ));
    }
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_and_build_arguments_name_the_exact_payload() {
        let sha = "866a01dbe536610719d57a76c24b808016dcec05200cce194b9d8ec5035b8c94";
        let tag = egress_image_tag(sha, "aarch64");
        assert_eq!(tag, "localhost/axocoatl-egress:866a01dbe5366107-aarch64");
        let args = build_args(&tag, sha, "aarch64", "/data/ctx");
        assert_eq!(&args[..3], ["build", "--pull=never", "--quiet"]);
        let joined = args.join(" ");
        assert!(joined.contains("--platform linux/arm64"), "{joined}");
        assert!(joined.contains(&format!("--label {PROGRAM_SHA256_LABEL}={sha}")));
        assert!(joined.contains("--label io.axocoatl.role=egress"));
        assert!(joined.ends_with("--file /data/ctx/Containerfile /data/ctx"));
        assert_eq!(platform("x86_64").unwrap(), "linux/amd64");
        assert!(platform("riscv64").is_err());
    }

    #[test]
    fn the_image_has_no_base_layer_and_nothing_but_the_payload() {
        assert!(CONTAINERFILE.starts_with("FROM scratch\n"));
        assert_eq!(CONTAINERFILE.lines().count(), 3);
        assert!(!CONTAINERFILE.contains("RUN"));
    }
}
