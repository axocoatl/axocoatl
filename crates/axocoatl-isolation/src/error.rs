#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    #[error("WASM compilation failed for tool '{tool}': {reason}")]
    CompilationFailed { tool: String, reason: String },

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("WASM instantiation failed: {0}")]
    InstantiationFailed(String),

    #[error("Tool '{tool}' missing required WASM export: '{export}'")]
    MissingExport { tool: String, export: String },

    #[error("WASM execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Fuel exhausted — tool execution exceeded computational budget")]
    FuelExhausted,

    #[error("Fuel error: {0}")]
    FuelError(String),

    #[error("Failed to read tool output")]
    OutputReadFailed,

    #[error("Firecracker VM failed to start: {0}")]
    VmStartFailed(String),

    #[error("OCI container setup failed: {0}")]
    OciSetupFailed(String),

    #[error("OCI container execution failed: {0}")]
    OciContainerFailed(String),

    #[error("Tool execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Wasmtime error: {0}")]
    Wasmtime(String),
}
