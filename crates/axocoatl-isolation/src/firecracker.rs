//! Firecracker microVM isolation for untrusted code execution.
//! Requires Linux + KVM. Feature-gated behind `firecracker-isolation`.
//!
//! Uses firepilot 1.2: Configuration/Machine API (not MachineBuilder).
//! SDK gaps: no vCPU/memory config, no snapshot/restore at SDK level.

use std::path::PathBuf;

#[cfg(feature = "firecracker-isolation")]
use crate::error::IsolationError;

/// Configuration for the Firecracker host environment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FirecrackerConfig {
    pub binary_path: PathBuf,
    pub kernel_image: PathBuf,
    pub rootfs_image: PathBuf,
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    pub chroot_dir: PathBuf,
    pub warm_pool_size: usize,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            binary_path: PathBuf::from("/usr/local/bin/firecracker"),
            kernel_image: PathBuf::from("/opt/axocoatl/vmlinux.bin"),
            rootfs_image: PathBuf::from("/opt/axocoatl/rootfs.ext4"),
            vcpu_count: 1,
            mem_size_mib: 128,
            chroot_dir: PathBuf::from("/srv/axocoatl"),
            warm_pool_size: 5,
        }
    }
}

/// Pool of Firecracker microVMs for tool execution.
#[cfg(feature = "firecracker-isolation")]
pub struct FirecrackerPool {
    config: FirecrackerConfig,
}

#[cfg(feature = "firecracker-isolation")]
impl FirecrackerPool {
    pub fn new(config: FirecrackerConfig) -> Self {
        Self { config }
    }

    /// Acquire a VM via cold boot (<125ms).
    pub async fn acquire(&self) -> Result<VmHandle, IsolationError> {
        use firepilot::builder::{
            drive::DriveBuilder, executor::FirecrackerExecutorBuilder, kernel::KernelBuilder,
            Builder, Configuration,
        };
        use firepilot::machine::Machine;

        let vm_id = uuid::Uuid::new_v4().to_string();

        let kernel = KernelBuilder::new()
            .with_kernel_image_path(self.config.kernel_image.to_str().unwrap().to_string())
            .with_boot_args("reboot=k panic=1 pci=off".to_string())
            .try_build()
            .map_err(|e| IsolationError::VmStartFailed(format!("Kernel: {e:?}")))?;

        let drive = DriveBuilder::new()
            .with_drive_id("rootfs".to_string())
            .with_path_on_host(self.config.rootfs_image.clone())
            .as_root_device()
            .try_build()
            .map_err(|e| IsolationError::VmStartFailed(format!("Drive: {e:?}")))?;

        let executor = FirecrackerExecutorBuilder::new()
            .with_chroot(self.config.chroot_dir.to_str().unwrap().to_string())
            .with_exec_binary(self.config.binary_path.clone())
            .try_build()
            .map_err(|e| IsolationError::VmStartFailed(format!("Executor: {e:?}")))?;

        let fc_config = Configuration::new(vm_id.clone())
            .with_kernel(kernel)
            .with_executor(executor)
            .with_drive(drive);

        let mut machine = Machine::new();
        machine
            .create(fc_config)
            .await
            .map_err(|e| IsolationError::VmStartFailed(format!("{e:?}")))?;
        machine
            .start()
            .await
            .map_err(|e| IsolationError::VmStartFailed(format!("{e:?}")))?;

        tracing::info!(vm_id = %vm_id, "Firecracker microVM started");

        Ok(VmHandle { machine, vm_id })
    }

    /// Release a VM — stop and clean up.
    pub async fn release(&self, handle: VmHandle) -> Result<(), IsolationError> {
        handle
            .machine
            .stop()
            .await
            .map_err(|e| IsolationError::VmStartFailed(format!("{e:?}")))?;
        tracing::debug!(vm_id = %handle.vm_id, "Firecracker VM stopped");
        Ok(())
    }
}

/// Handle to a running Firecracker VM.
#[cfg(feature = "firecracker-isolation")]
pub struct VmHandle {
    machine: firepilot::machine::Machine,
    vm_id: String,
}

#[cfg(feature = "firecracker-isolation")]
impl VmHandle {
    /// Execute a tool inside this VM via vsock.
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        _args: serde_json::Value,
        _timeout: std::time::Duration,
    ) -> Result<serde_json::Value, IsolationError> {
        // Communication via virtio-vsock: host connects to guest on well-known port
        // Guest runs: axocoatl-tool-executor (tiny Rust binary in rootfs)
        // Protocol: JSON request/response over vsock
        tracing::debug!(vm_id = %self.vm_id, tool = %tool_name, "Executing tool in VM");

        // TODO: Implement vsock client communication
        Err(IsolationError::ExecutionFailed(format!(
            "vsock tool execution not yet implemented for tool '{tool_name}'"
        )))
    }

    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = FirecrackerConfig::default();
        assert_eq!(config.vcpu_count, 1);
        assert_eq!(config.mem_size_mib, 128);
        assert_eq!(config.warm_pool_size, 5);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = FirecrackerConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: FirecrackerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.vcpu_count, 1);
    }
}
