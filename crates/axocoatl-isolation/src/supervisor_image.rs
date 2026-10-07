//! Resolve the already-approved image before selecting an embedded executable.
//! Image metadata chooses bytes; a live no-dispatch handshake separately proves
//! that the exact installed supervisor works in the newly created container.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::session_sandbox::BoundedCommandOutput;
use crate::supervisor_program::{SupervisorProgram, SUPERVISOR_CONTAINER_PATH};
use crate::{IsolationError, SessionSandbox};

const INSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_TIMEOUT: Duration = Duration::from_secs(600);
const IMAGE_FORMAT: &str =
    r#"{"id":{{json .ID}},"os":{{json .Os}},"architecture":{{json .Architecture}}}"#;
const CONTAINER_FORMAT: &str =
    r#"{"id":{{json .ID}},"image":{{json .Image}},"mounts":{{json .Mounts}}}"#;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SupervisorImage {
    id: String,
    architecture: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageInspection {
    id: String,
    os: String,
    architecture: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContainerInspection {
    id: String,
    image: String,
    mounts: Vec<MountInspection>,
}

#[derive(Deserialize)]
struct MountInspection {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Source")]
    source: String,
    #[serde(rename = "Destination")]
    destination: String,
    #[serde(rename = "RW")]
    writable: bool,
}

fn invalid(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::OciSetupFailed(format!(
        "selecting the embedded process supervisor: {message}"
    ))
}

fn exact_id(value: &str) -> Result<&str, IsolationError> {
    let id = value.strip_prefix("sha256:").unwrap_or(value);
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "Podman omitted a complete immutable image or container identity",
        ));
    }
    Ok(id)
}

fn checked_output(output: &BoundedCommandOutput, operation: &str) -> Result<(), IsolationError> {
    if output.timed_out {
        return Err(invalid(format!("{operation} timed out")));
    }
    if !output.status.success() {
        return Err(invalid(format!(
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if output.stdout_truncated {
        return Err(invalid(format!(
            "{operation} exceeded the inspection output bound"
        )));
    }
    Ok(())
}

/// `podman pull` for an image `podman image exists` just reported absent.
///
/// No `--policy`: Podman before 5.0 has none (Ubuntu 24.04 ships 4.9), and
/// the existence check already makes this "pull only if missing". A name
/// pinned by digest pulls exactly that manifest. Whatever the pull leaves
/// under the name is never trusted by it: the bounded inspection after it
/// names the immutable image id, the container is created from that id with
/// `--pull=never`, and [`SupervisorImage::verify_container`] requires the
/// created container to run exactly it.
fn pull_args(image: &str) -> [&str; 3] {
    ["pull", "--", image]
}

impl SupervisorImage {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn architecture(&self) -> &str {
        self.architecture
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, IsolationError> {
        let image: ImageInspection = serde_json::from_slice(bytes).map_err(invalid)?;
        if image.os != "linux" {
            return Err(invalid(format!(
                "the selected image targets {}, not Linux",
                image.os
            )));
        }
        let architecture = match image.architecture.as_str() {
            "amd64" | "x86_64" => "x86_64",
            "arm64" | "aarch64" => "aarch64",
            other => {
                return Err(invalid(format!(
                    "the selected image architecture {other:?} has no bundled supervisor"
                )))
            }
        };
        Ok(Self {
            id: exact_id(&image.id)?.to_owned(),
            architecture,
        })
    }

    /// Call only after the existing curated/custom-image trust gate and Podman
    /// readiness. Preserve local-image-first behavior and pull only if absent.
    pub(crate) async fn resolve(image: &str) -> Result<Self, IsolationError> {
        let mut exists = Command::new("podman");
        exists.args(["image", "exists", "--", image]);
        let exists = SessionSandbox::run_bounded_command(exists, INSPECT_TIMEOUT).await?;
        if exists.timed_out {
            return Err(invalid("checking the selected image timed out"));
        }
        match exists.status.code() {
            Some(0) => (),
            Some(1) => {
                let mut pull = Command::new("podman");
                pull.args(pull_args(image));
                let pulled = SessionSandbox::run_bounded_command(pull, PULL_TIMEOUT).await?;
                // Pull progress may legitimately exceed retained diagnostics;
                // the later bounded inspection is the source of identity.
                if pulled.timed_out || !pulled.status.success() {
                    return Err(invalid(format!(
                        "pulling the selected image {}: {}",
                        if pulled.timed_out {
                            "timed out"
                        } else {
                            "failed"
                        },
                        String::from_utf8_lossy(&pulled.stderr).trim()
                    )));
                }
            }
            _ => {
                return Err(invalid(format!(
                    "checking the selected image: {}",
                    String::from_utf8_lossy(&exists.stderr).trim()
                )))
            }
        }
        let mut inspect = Command::new("podman");
        inspect.args(["image", "inspect", "--format", IMAGE_FORMAT, "--", image]);
        let output = SessionSandbox::run_bounded_command(inspect, INSPECT_TIMEOUT).await?;
        checked_output(&output, "inspecting the selected image")?;
        Self::parse(&output.stdout)
    }

    fn verify_container_inspection(
        &self,
        container_id: &str,
        program_path: &Path,
        bytes: &[u8],
    ) -> Result<(), IsolationError> {
        let actual: ContainerInspection = serde_json::from_slice(bytes).map_err(invalid)?;
        if exact_id(&actual.id)? != exact_id(container_id)? || exact_id(&actual.image)? != self.id {
            return Err(invalid(
                "the created container does not match its inspected immutable image and identity",
            ));
        }
        let mut supervisor_mounts = actual
            .mounts
            .iter()
            .filter(|mount| mount.destination == SUPERVISOR_CONTAINER_PATH);
        let mount = supervisor_mounts
            .next()
            .ok_or_else(|| invalid("the exact supervisor mount is missing"))?;
        if supervisor_mounts.next().is_some()
            || mount.kind != "bind"
            || mount.writable
            || Path::new(&mount.source) != program_path
        {
            return Err(invalid(
                "the supervisor is not the exact read-only installed bind mount",
            ));
        }
        Ok(())
    }

    /// Match the actual container and read-only mount before trusting its live
    /// supervisor protocol. This never substitutes for that handshake.
    pub(crate) async fn verify_container(
        &self,
        container_id: &str,
        program: &SupervisorProgram,
    ) -> Result<(), IsolationError> {
        program.verify()?;
        if program.architecture() != self.architecture {
            return Err(invalid(
                "the installed supervisor architecture differs from the selected image",
            ));
        }
        let mut inspect = Command::new("podman");
        inspect.args([
            "container",
            "inspect",
            "--format",
            CONTAINER_FORMAT,
            "--",
            container_id,
        ]);
        let output = SessionSandbox::run_bounded_command(inspect, INSPECT_TIMEOUT).await?;
        checked_output(&output, "inspecting the created container")?;
        self.verify_container_inspection(container_id, program.path(), &output.stdout)?;
        program.verify()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str, os: &str, architecture: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "id": id, "os": os, "architecture": architecture }))
            .unwrap()
    }

    #[test]
    fn image_architecture_comes_from_linux_image_and_identity_is_complete() {
        let id = "a".repeat(64);
        for (input, expected) in [
            ("amd64", "x86_64"),
            ("x86_64", "x86_64"),
            ("arm64", "aarch64"),
            ("aarch64", "aarch64"),
        ] {
            let inspected =
                SupervisorImage::parse(&image(&format!("sha256:{id}"), "linux", input)).unwrap();
            assert_eq!(inspected.id(), id);
            assert_eq!(inspected.architecture(), expected);
        }
        for os in ["windows", "darwin", "", "Linux"] {
            assert!(SupervisorImage::parse(&image(&id, os, "amd64")).is_err());
        }
        for architecture in ["386", "arm", "ppc64le", "riscv64", "", "amd64\n"] {
            assert!(SupervisorImage::parse(&image(&id, "linux", architecture)).is_err());
        }
        for malformed in [
            "latest".into(),
            "a12345".into(),
            "sha256:".into(),
            "A".repeat(64),
            "a".repeat(65),
        ] {
            assert!(SupervisorImage::parse(&image(&malformed, "linux", "amd64")).is_err());
        }
    }

    /// The pull of an absent image runs on every Podman Axocoatl supports:
    /// Podman 4.9 (Ubuntu 24.04, the Linux CI runner) has no `pull --policy`
    /// and refused every hardened Session start with "unknown flag". The
    /// argv is the subcommand, the end of options and the name, nothing else.
    #[test]
    fn the_pull_of_an_absent_image_uses_no_flag_older_podman_lacks() {
        for image in [
            "docker.io/library/alpine:3.20",
            "localhost/axocoatl-recipe-e2e:fba88c485c0f",
            "docker.io/library/alpine@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "-looks-like-a-flag",
        ] {
            assert_eq!(pull_args(image), ["pull", "--", image]);
        }
    }

    /// The real resolution on this computer's Podman: an absent image is
    /// pulled once and resolved to its immutable id and architecture; a
    /// present one resolves without a pull. Removes only an image it pulled.
    /// Needs Podman (`CONTAINER_CONNECTION`) and network access to Docker Hub.
    #[tokio::test]
    #[ignore = "requires Podman (CONTAINER_CONNECTION) and network access to docker.io"]
    async fn actual_absent_image_is_pulled_and_resolved_to_its_identity() {
        const IMAGE: &str = "docker.io/library/busybox:1.37.0-musl";
        let exists = || {
            std::process::Command::new("podman")
                .args(["image", "exists", "--", IMAGE])
                .status()
                .unwrap()
                .success()
        };
        let pulled_here = !exists();
        struct Remove(bool);
        impl Drop for Remove {
            fn drop(&mut self) {
                if self.0 {
                    let _ = std::process::Command::new("podman")
                        .args(["rmi", "--", IMAGE])
                        .output();
                }
            }
        }
        let _remove = Remove(pulled_here);
        let first = SupervisorImage::resolve(IMAGE).await.unwrap();
        assert!(exists(), "the absent image was pulled");
        let host_arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            other => panic!("unexpected test architecture {other}"),
        };
        // A Podman machine runs the host's architecture.
        assert_eq!(first.architecture(), host_arch);
        let inspected = std::process::Command::new("podman")
            .args(["image", "inspect", "--format", "{{.ID}}", "--", IMAGE])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&inspected.stdout).trim(),
            first.id()
        );
        // Present now: resolved again, to the same identity.
        assert_eq!(SupervisorImage::resolve(IMAGE).await.unwrap(), first);
    }

    #[test]
    fn image_inspection_rejects_ambiguous_or_incomplete_records() {
        for bytes in [b"[]".as_slice(), b"{}", b"{\"id\":null}", b"{}{}"] {
            assert!(SupervisorImage::parse(bytes).is_err());
        }
        let mut value =
            serde_json::json!({ "id": "a".repeat(64), "os": "linux", "architecture": "amd64" });
        value["other"] = true.into();
        assert!(SupervisorImage::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    fn container_record(container: &str, image: &str) -> serde_json::Value {
        serde_json::json!({ "id": container, "image": image, "mounts": [{
            "Type": "bind", "Source": "/private/supervisor", "Destination": SUPERVISOR_CONTAINER_PATH, "RW": false,
            "Options": ["ro", "rbind"]
        }] })
    }

    #[test]
    fn container_must_use_exact_image_and_read_only_program_source() {
        let image_id = "a".repeat(64);
        let container_id = "b".repeat(64);
        let inspected = SupervisorImage::parse(&image(&image_id, "linux", "amd64")).unwrap();
        let base = container_record(&container_id, &image_id);
        let verify = |value: &serde_json::Value| {
            inspected.verify_container_inspection(
                &container_id,
                Path::new("/private/supervisor"),
                &serde_json::to_vec(value).unwrap(),
            )
        };
        verify(&base).unwrap();
        for field in ["id", "image"] {
            let mut changed = base.clone();
            changed[field] = "c".repeat(64).into();
            assert!(verify(&changed).is_err());
        }
        for (field, replacement) in [
            ("Type", "volume"),
            ("Source", "/workspace/forged"),
            ("Destination", "/somewhere-else"),
        ] {
            let mut changed = base.clone();
            changed["mounts"][0][field] = replacement.into();
            assert!(verify(&changed).is_err());
        }
        let mut changed = base.clone();
        changed["mounts"][0]["RW"] = true.into();
        assert!(verify(&changed).is_err());
        changed["mounts"] = serde_json::json!([]);
        assert!(verify(&changed).is_err());
        changed["mounts"] = serde_json::json!([base["mounts"][0], base["mounts"][0]]);
        assert!(verify(&changed).is_err());
    }
}
