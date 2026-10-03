//! The two first-party Linux programs are carried inside the host executable.
//! Selection follows the inspected container image, including when the host
//! runs another OS or architecture. No build-time or runtime download occurs.

use axocoatl_exec::protocol::{PROTOCOL_VERSION, SUPERVISOR_VERSION};

use crate::IsolationError;

const EMBEDDED_PROTOCOL_VERSION: u32 = 3;
const EMBEDDED_PACKAGE_VERSION: &str = "1.1.2";
const EMBEDDED_SOURCE_SHA256: &str =
    "7e0b62acbd137bed12d264065ad6b7e0fc2eb889f205f3587fc651cc129514b2";

#[derive(Clone, Copy)]
pub(crate) struct EmbeddedSupervisor {
    pub(crate) architecture: &'static str,
    pub(crate) bytes: &'static [u8],
    pub(crate) sha256: &'static str,
    protocol_version: u32,
    package_version: &'static str,
    source_sha256: &'static str,
}

impl EmbeddedSupervisor {
    pub(crate) fn verify_protocol(&self) -> Result<(), IsolationError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(invalid(
                "bundled program protocol differs from the host protocol",
            ));
        }
        if self.package_version != SUPERVISOR_VERSION {
            return Err(invalid(
                "bundled program version differs from the execution library",
            ));
        }
        if self.source_sha256 != axocoatl_exec::SUPERVISOR_SOURCE_SHA256 {
            return Err(invalid(
                "bundled program must be rebuilt from the current execution source",
            ));
        }
        Ok(())
    }
}

const X86_64: EmbeddedSupervisor = EmbeddedSupervisor {
    architecture: "x86_64",
    bytes: include_bytes!("../assets/exec-supervisor/axocoatl-exec-supervisor-linux-x86_64"),
    sha256: "7dbc6312003ac8c81cb3b16f74ce0d8a527790b045bbbb3ec2f65758ae8e6f7b",
    protocol_version: EMBEDDED_PROTOCOL_VERSION,
    package_version: EMBEDDED_PACKAGE_VERSION,
    source_sha256: EMBEDDED_SOURCE_SHA256,
};

const AARCH64: EmbeddedSupervisor = EmbeddedSupervisor {
    architecture: "aarch64",
    bytes: include_bytes!("../assets/exec-supervisor/axocoatl-exec-supervisor-linux-aarch64"),
    sha256: "b0a0b59a2682163940f94ffb7288b46e70e7818e44bd0f1bf1c027299a230a14",
    protocol_version: EMBEDDED_PROTOCOL_VERSION,
    package_version: EMBEDDED_PACKAGE_VERSION,
    source_sha256: EMBEDDED_SOURCE_SHA256,
};

fn invalid(message: &str) -> IsolationError {
    IsolationError::OciSetupFailed(format!("embedded execution supervisor: {message}"))
}

pub(crate) fn payload(architecture: &str) -> Result<&'static EmbeddedSupervisor, IsolationError> {
    match architecture {
        "x86_64" | "amd64" => Ok(&X86_64),
        "aarch64" | "arm64" => Ok(&AARCH64),
        _ => Err(invalid(
            "selected Linux runtime architecture is unsupported",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn embedded_bytes_and_identity_match_the_retained_build_manifests() {
        for (embedded, manifest) in [
            (&X86_64, include_str!("../assets/exec-supervisor/axocoatl-exec-supervisor-linux-x86_64.manifest.json")),
            (&AARCH64, include_str!("../assets/exec-supervisor/axocoatl-exec-supervisor-linux-aarch64.manifest.json")),
        ] {
            assert!(manifest.len() <= 512 * 1024);
            let manifest: serde_json::Value = serde_json::from_str(manifest).unwrap();
            assert_eq!(manifest["schema_version"], 2);
            assert_eq!(manifest["package"], "axocoatl-exec");
            assert_eq!(manifest["binary"], "axocoatl-exec-supervisor");
            assert_eq!(manifest["package_version"], embedded.package_version);
            assert_eq!(manifest["protocol_version"], embedded.protocol_version);
            assert_eq!(manifest["helper_source"]["sha256"], embedded.source_sha256);
            assert_eq!(manifest["payload"]["bytes"], embedded.bytes.len());
            assert_eq!(manifest["payload"]["sha256"], embedded.sha256);
            assert_eq!(manifest["payload"]["elf"]["architecture"], embedded.architecture);
            assert_eq!(manifest["payload"]["elf"]["external_interpreter"], false);
            assert_eq!(manifest["payload"]["elf"]["dynamic_dependencies"], false);
            assert_eq!(format!("{:x}", Sha256::digest(embedded.bytes)), embedded.sha256);
            // These manifests record packaging provenance. Executing the real
            // protocol in the selected image remains a separate host check.
            assert_eq!(manifest["verification"]["container_execution"], false);
            embedded.verify_protocol().unwrap();
        }
    }

    #[test]
    fn stale_protocol_version_or_source_identity_is_refused() {
        for changed in [
            EmbeddedSupervisor {
                protocol_version: EMBEDDED_PROTOCOL_VERSION + 1,
                ..X86_64
            },
            EmbeddedSupervisor {
                package_version: "0.0.0",
                ..X86_64
            },
            EmbeddedSupervisor {
                source_sha256: "0000000000000000000000000000000000000000000000000000000000000000",
                ..X86_64
            },
        ] {
            assert!(changed.verify_protocol().is_err());
        }
    }
}
