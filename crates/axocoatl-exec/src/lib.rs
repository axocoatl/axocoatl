//! Protocol shared by the sandbox's process supervisor and its owned host.
//! A decoded response alone is not runtime authority: the host also owns the
//! exact installed helper, command identity, transport and workspace lease.
pub mod egress;
pub mod protocol;

/// Exact Rust source and package-version identity, stable across Cargo packaging.
pub const SUPERVISOR_SOURCE_SHA256: &str = env!("AXOCOATL_EXEC_SOURCE_SHA256");

#[cfg(target_os = "linux")]
pub mod harden;
#[cfg(target_os = "linux")]
pub mod supervisor;
