use serde::{Deserialize, Serialize};

/// Which isolation tier to use for a given tool execution.
///
/// # Tiered Isolation Model
///
/// | Tier | Runtime | Startup | Memory | Security | Platform |
/// |------|---------|---------|--------|----------|----------|
/// | 0 | None | 0 | 0 | In-process | Everywhere |
/// | 1 | Wasmtime | <1ms | ~1-5 MB | WASM sandbox + WASI caps | Everywhere |
/// | 2 | youki OCI | ~198ms | ~same as runc | cgroups + namespaces | Linux |
/// | 3 | Firecracker | <125ms cold / <5ms warm | <35 MiB | KVM hardware VM | Linux + KVM |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IsolationTier {
    /// Wasmtime WASM sandbox. Startup: <1ms, Memory: ~1-5 MB.
    /// Universal — works on Linux, macOS, Windows.
    Wasmtime,
    /// youki OCI container runtime. Startup: ~198ms, Memory: ~same as runc.
    /// Rust-native runc replacement — 44% faster than Docker/runc (~800ms).
    /// Requires Linux (cgroups + namespaces).
    Oci,
    /// Firecracker microVM. Cold start: <125ms, Warm: <5ms, Memory: <35 MiB.
    /// Strongest isolation — KVM hardware virtualization.
    /// Requires Linux + `/dev/kvm`.
    Firecracker,
    /// No isolation — executes in the axocoatl-daemon process.
    /// Only for trusted built-in tools.
    None,
}

/// What runtime a tool requires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeRequirement {
    /// Tool is compiled to WASM.
    WasmCompiled,
    /// Tool requires Python interpreter.
    Python,
    /// Tool runs shell commands.
    Shell,
    /// Arbitrary code (unknown requirements).
    Arbitrary,
    /// Packaged as an OCI container image.
    ContainerImage,
    /// Trusted built-in Axocoatl tool.
    NativeTrusted,
}

/// Select the ideal isolation tier for a tool based on its runtime requirement.
///
/// This returns the *preferred* tier. Use [`select_tier_with_availability`] to
/// automatically fall back when a tier isn't compiled in.
pub fn select_tier(requirement: &RuntimeRequirement) -> IsolationTier {
    match requirement {
        RuntimeRequirement::WasmCompiled => IsolationTier::Wasmtime,
        RuntimeRequirement::Python | RuntimeRequirement::Shell | RuntimeRequirement::Arbitrary => {
            IsolationTier::Firecracker
        }
        RuntimeRequirement::ContainerImage => IsolationTier::Oci,
        RuntimeRequirement::NativeTrusted => IsolationTier::None,
    }
}

/// Select the best *available* isolation tier, considering compiled features.
///
/// Falls back through the tier hierarchy when the ideal tier isn't compiled in:
/// - Firecracker → OCI → Wasmtime (for Python/Shell/Arbitrary)
/// - OCI → Firecracker → Wasmtime (for ContainerImage)
pub fn select_tier_with_availability(requirement: &RuntimeRequirement) -> IsolationTier {
    let ideal = select_tier(requirement);

    match ideal {
        IsolationTier::Firecracker => {
            if cfg!(feature = "firecracker-isolation") {
                IsolationTier::Firecracker
            } else if cfg!(feature = "oci-isolation") {
                IsolationTier::Oci
            } else {
                // Last resort: Wasmtime can run WASI-compiled tools but not arbitrary code.
                // This is a degraded mode — log a warning at runtime.
                IsolationTier::Wasmtime
            }
        }
        IsolationTier::Oci => {
            if cfg!(feature = "oci-isolation") {
                IsolationTier::Oci
            } else if cfg!(feature = "firecracker-isolation") {
                IsolationTier::Firecracker
            } else {
                IsolationTier::Wasmtime
            }
        }
        // Wasmtime and None are always available.
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_goes_to_wasmtime() {
        assert_eq!(
            select_tier(&RuntimeRequirement::WasmCompiled),
            IsolationTier::Wasmtime
        );
    }

    #[test]
    fn python_goes_to_firecracker() {
        assert_eq!(
            select_tier(&RuntimeRequirement::Python),
            IsolationTier::Firecracker
        );
    }

    #[test]
    fn shell_goes_to_firecracker() {
        assert_eq!(
            select_tier(&RuntimeRequirement::Shell),
            IsolationTier::Firecracker
        );
    }

    #[test]
    fn arbitrary_goes_to_firecracker() {
        assert_eq!(
            select_tier(&RuntimeRequirement::Arbitrary),
            IsolationTier::Firecracker
        );
    }

    #[test]
    fn container_image_goes_to_oci() {
        assert_eq!(
            select_tier(&RuntimeRequirement::ContainerImage),
            IsolationTier::Oci
        );
    }

    #[test]
    fn trusted_gets_no_isolation() {
        assert_eq!(
            select_tier(&RuntimeRequirement::NativeTrusted),
            IsolationTier::None
        );
    }

    #[test]
    fn tier_serde_roundtrip() {
        let tier = IsolationTier::Wasmtime;
        let json = serde_json::to_string(&tier).unwrap();
        let back: IsolationTier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tier);
    }

    #[test]
    fn oci_tier_serde_roundtrip() {
        let tier = IsolationTier::Oci;
        let json = serde_json::to_string(&tier).unwrap();
        let back: IsolationTier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tier);
    }

    #[test]
    fn availability_wasm_always_available() {
        assert_eq!(
            select_tier_with_availability(&RuntimeRequirement::WasmCompiled),
            IsolationTier::Wasmtime
        );
    }

    #[test]
    fn availability_trusted_always_available() {
        assert_eq!(
            select_tier_with_availability(&RuntimeRequirement::NativeTrusted),
            IsolationTier::None
        );
    }
}
