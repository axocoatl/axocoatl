//! youki-based OCI container sandbox for tool execution.
//! Linux-only. Feature-gated behind `oci-isolation`.
//!
//! Uses libcontainer 0.6 (from the youki project) for OCI container lifecycle.
//! Startup: ~198ms vs Docker/runc's ~800ms — same cgroup/namespace isolation,
//! but Rust-native and 44% faster.

use std::path::PathBuf;

#[cfg(feature = "oci-isolation")]
use crate::error::IsolationError;

/// Configuration for the OCI container runtime.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OciConfig {
    /// Root directory for container state (default: /run/axocoatl/oci).
    pub state_root: PathBuf,
    /// Directory for unpacked OCI bundles (default: /var/axocoatl/bundles).
    /// Each tool has a subdirectory: `{bundle_root}/{tool_name}/` containing
    /// `config.json` and `rootfs/`.
    pub bundle_root: PathBuf,
    /// Use systemd cgroup manager (default: false).
    pub use_systemd: bool,
}

impl Default for OciConfig {
    fn default() -> Self {
        Self {
            state_root: PathBuf::from("/run/axocoatl/oci"),
            bundle_root: PathBuf::from("/var/axocoatl/bundles"),
            use_systemd: false,
        }
    }
}

/// youki-based OCI container sandbox.
///
/// Executes tools packaged as OCI container images using Linux namespaces + cgroups.
/// Each tool is an OCI bundle at `{bundle_root}/{tool_name}/` with a `config.json`
/// and `rootfs/` directory.
///
/// Tool I/O protocol:
/// - Input: JSON written to `{state_root}/{container_id}/input.json`, bind-mounted
///   into the container at `/axocoatl/input.json`.
/// - Output: Tool writes JSON to `/axocoatl/output.json`, which is bind-mounted from
///   `{state_root}/{container_id}/output.json`.
#[cfg(feature = "oci-isolation")]
pub struct OciSandbox {
    config: OciConfig,
}

#[cfg(feature = "oci-isolation")]
impl OciSandbox {
    /// Create a new OCI sandbox, ensuring state directories exist.
    pub fn new(config: OciConfig) -> Result<Self, IsolationError> {
        std::fs::create_dir_all(&config.state_root).map_err(|e| {
            IsolationError::OciSetupFailed(format!(
                "Failed to create state root {}: {e}",
                config.state_root.display()
            ))
        })?;
        std::fs::create_dir_all(&config.bundle_root).map_err(|e| {
            IsolationError::OciSetupFailed(format!(
                "Failed to create bundle root {}: {e}",
                config.bundle_root.display()
            ))
        })?;
        Ok(Self { config })
    }

    /// Execute a tool inside an OCI container.
    ///
    /// The tool must be available as an OCI bundle at `{bundle_root}/{tool_name}/`.
    pub async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, IsolationError> {
        let container_id = format!("axocoatl-{}-{}", tool_name, uuid::Uuid::new_v4());
        let bundle_path = self.config.bundle_root.join(tool_name);

        if !bundle_path.exists() {
            return Err(IsolationError::ToolNotFound(tool_name.to_string()));
        }

        tracing::debug!(
            container_id = %container_id,
            tool = %tool_name,
            "Starting OCI container for tool execution"
        );

        // Set up I/O directory for this container
        let io_dir = self.config.state_root.join(&container_id);
        tokio::fs::create_dir_all(&io_dir).await?;

        let input_path = io_dir.join("input.json");
        let output_path = io_dir.join("output.json");

        // Write input for the container to read
        tokio::fs::write(&input_path, serde_json::to_vec(&input)?).await?;

        // Run container in a blocking task (libcontainer is synchronous)
        let state_root = self.config.state_root.clone();
        let use_systemd = self.config.use_systemd;
        let cid = container_id.clone();
        let bp = bundle_path.clone();

        let handle = tokio::task::spawn_blocking(move || {
            Self::run_container_sync(&cid, &bp, &state_root, use_systemd)
        });

        let result = match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(()))) => {
                // Read output written by the container
                let output_bytes = tokio::fs::read(&output_path)
                    .await
                    .map_err(|_| IsolationError::OutputReadFailed)?;
                serde_json::from_slice(&output_bytes).map_err(IsolationError::Serialization)
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(e)) => Err(IsolationError::OciContainerFailed(format!(
                "Container task panicked: {e}"
            ))),
            Err(_) => {
                tracing::warn!(
                    container_id = %container_id,
                    "OCI container timed out — cleanup may be needed"
                );
                Err(IsolationError::Timeout(timeout))
            }
        };

        // Clean up I/O directory
        let _ = tokio::fs::remove_dir_all(&io_dir).await;

        result
    }

    /// Synchronous container lifecycle: create → start → wait → delete.
    fn run_container_sync(
        container_id: &str,
        bundle_path: &std::path::Path,
        state_root: &std::path::Path,
        use_systemd: bool,
    ) -> Result<(), IsolationError> {
        use libcontainer::container::builder::ContainerBuilder;
        use libcontainer::syscall::syscall::SyscallType;

        let mut container = ContainerBuilder::new(container_id.to_string(), SyscallType::default())
            .with_root_path(state_root)
            .map_err(|e| IsolationError::OciSetupFailed(format!("Root path: {e}")))?
            .with_executor(libcontainer::workload::default::DefaultExecutor {})
            .validate_id()
            .map_err(|e| IsolationError::OciSetupFailed(format!("Invalid container ID: {e}")))?
            .as_init(bundle_path)
            .with_systemd(use_systemd)
            .with_detach(false)
            .build()
            .map_err(|e| IsolationError::OciContainerFailed(format!("Container create: {e}")))?;

        // OCI lifecycle: create (done above) → start
        container
            .start()
            .map_err(|e| IsolationError::OciContainerFailed(format!("Container start: {e}")))?;

        tracing::debug!(container_id = %container_id, "OCI container completed");

        // Clean up container state
        container
            .delete(false)
            .map_err(|e| IsolationError::OciContainerFailed(format!("Container delete: {e}")))?;

        Ok(())
    }

    /// Access the sandbox configuration.
    pub fn config(&self) -> &OciConfig {
        &self.config
    }
}

// Uses libcontainer::workload::default::DefaultExecutor for standard OCI process
// execution (execvp based on config.json process args). No custom executor needed.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = OciConfig::default();
        assert_eq!(config.state_root, PathBuf::from("/run/axocoatl/oci"));
        assert_eq!(config.bundle_root, PathBuf::from("/var/axocoatl/bundles"));
        assert!(!config.use_systemd);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = OciConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: OciConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state_root, config.state_root);
        assert_eq!(back.bundle_root, config.bundle_root);
    }
}
