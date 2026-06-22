pub mod error;
pub mod podman;
pub mod pty;
pub mod session_sandbox;

// Experimental, opt-in isolation tiers. The shipped boundary is the rootless
// Podman session sandbox above; these microVM / OCI tiers are gated out of the
// default build so it carries no unfinished isolation code.
#[cfg(all(feature = "firecracker-isolation", target_os = "linux"))]
pub mod firecracker;
#[cfg(all(feature = "oci-isolation", target_os = "linux"))]
pub mod oci_sandbox;
#[cfg(all(
    any(feature = "firecracker-isolation", feature = "oci-isolation"),
    target_os = "linux"
))]
pub mod tier;
#[cfg(all(feature = "firecracker-isolation", target_os = "linux"))]
pub mod vsock;

#[cfg(feature = "wasmtime-sandbox")]
pub mod wasmtime_sandbox;

pub use error::*;
pub use session_sandbox::*;

#[cfg(all(feature = "firecracker-isolation", target_os = "linux"))]
pub use firecracker::*;
#[cfg(all(feature = "oci-isolation", target_os = "linux"))]
pub use oci_sandbox::*;
#[cfg(all(
    any(feature = "firecracker-isolation", feature = "oci-isolation"),
    target_os = "linux"
))]
pub use tier::*;
#[cfg(all(feature = "firecracker-isolation", target_os = "linux"))]
pub use vsock::*;

#[cfg(feature = "wasmtime-sandbox")]
pub use wasmtime_sandbox::*;
