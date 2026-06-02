//! Virtio-vsock communication for Firecracker microVMs.
//! Host-side client that connects to the guest tool executor.
//!
//! Protocol: length-prefixed JSON over vsock.
//! - Request: 4-byte big-endian length + JSON payload
//! - Response: 4-byte big-endian length + JSON payload

use serde::{Deserialize, Serialize};

/// Default vsock port for the guest tool executor.
pub const VSOCK_TOOL_PORT: u32 = 5000;

/// CID for the host in vsock.
pub const VSOCK_HOST_CID: u32 = 2;

/// Request sent from host to guest tool executor.
#[derive(Debug, Serialize, Deserialize)]
pub struct VsockToolRequest {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub timeout_ms: u64,
}

/// Response from guest tool executor back to host.
#[derive(Debug, Serialize, Deserialize)]
pub struct VsockToolResponse {
    pub success: bool,
    pub result: serde_json::Value,
    pub error: Option<String>,
    pub execution_time_ms: u64,
}

/// Client for communicating with a Firecracker guest via vsock.
///
/// On Linux with KVM, vsock uses AF_VSOCK sockets. On other platforms,
/// this is a stub that returns errors (Firecracker requires Linux+KVM).
pub struct VsockClient {
    guest_cid: u32,
    port: u32,
}

impl VsockClient {
    /// Create a new vsock client for the given guest CID.
    pub fn new(guest_cid: u32) -> Self {
        Self {
            guest_cid,
            port: VSOCK_TOOL_PORT,
        }
    }

    /// Create with a custom port.
    pub fn with_port(guest_cid: u32, port: u32) -> Self {
        Self { guest_cid, port }
    }

    /// Send a tool execution request to the guest and receive the response.
    #[cfg(target_os = "linux")]
    pub async fn execute_tool(
        &self,
        request: VsockToolRequest,
    ) -> Result<VsockToolResponse, crate::error::IsolationError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Encode request as length-prefixed JSON
        let payload = serde_json::to_vec(&request)
            .map_err(|e| crate::error::IsolationError::ExecutionFailed(e.to_string()))?;

        // Connect via vsock
        let mut stream = tokio::net::UnixStream::connect(format!(
            "/tmp/axocoatl-vsock-{}-{}",
            self.guest_cid, self.port
        ))
        .await
        .map_err(|e| {
            crate::error::IsolationError::ExecutionFailed(format!(
                "vsock connect to CID {} port {}: {e}",
                self.guest_cid, self.port
            ))
        })?;

        // Send length prefix + payload
        let len_bytes = (payload.len() as u32).to_be_bytes();
        stream.write_all(&len_bytes).await.map_err(|e| {
            crate::error::IsolationError::ExecutionFailed(format!("vsock write: {e}"))
        })?;
        stream.write_all(&payload).await.map_err(|e| {
            crate::error::IsolationError::ExecutionFailed(format!("vsock write: {e}"))
        })?;

        // Read response length
        let mut resp_len_bytes = [0u8; 4];
        stream.read_exact(&mut resp_len_bytes).await.map_err(|e| {
            crate::error::IsolationError::ExecutionFailed(format!("vsock read length: {e}"))
        })?;
        let resp_len = u32::from_be_bytes(resp_len_bytes) as usize;

        // Read response payload
        let mut resp_buf = vec![0u8; resp_len];
        stream.read_exact(&mut resp_buf).await.map_err(|e| {
            crate::error::IsolationError::ExecutionFailed(format!("vsock read body: {e}"))
        })?;

        let response: VsockToolResponse = serde_json::from_slice(&resp_buf).map_err(|e| {
            crate::error::IsolationError::ExecutionFailed(format!("vsock parse: {e}"))
        })?;

        Ok(response)
    }

    /// Stub for non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    pub async fn execute_tool(
        &self,
        _request: VsockToolRequest,
    ) -> Result<VsockToolResponse, crate::error::IsolationError> {
        Err(crate::error::IsolationError::ExecutionFailed(
            "vsock communication requires Linux with KVM".to_string(),
        ))
    }

    pub fn guest_cid(&self) -> u32 {
        self.guest_cid
    }

    pub fn port(&self) -> u32 {
        self.port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsock_request_serde() {
        let req = VsockToolRequest {
            tool_name: "echo".to_string(),
            arguments: serde_json::json!({"text": "hello"}),
            timeout_ms: 5000,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: VsockToolRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_name, "echo");
    }

    #[test]
    fn vsock_response_serde() {
        let resp = VsockToolResponse {
            success: true,
            result: serde_json::json!({"text": "hello"}),
            error: None,
            execution_time_ms: 42,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: VsockToolResponse = serde_json::from_str(&json).unwrap();
        assert!(back.success);
        assert_eq!(back.execution_time_ms, 42);
    }

    #[test]
    fn vsock_client_creates() {
        let client = VsockClient::new(3);
        assert_eq!(client.guest_cid(), 3);
        assert_eq!(client.port(), VSOCK_TOOL_PORT);
    }

    #[test]
    fn vsock_client_custom_port() {
        let client = VsockClient::with_port(3, 6000);
        assert_eq!(client.port(), 6000);
    }
}
