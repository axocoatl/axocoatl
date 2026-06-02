#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Edge not found: {from} -> {to}")]
    EdgeNotFound { from: String, to: String },

    #[error("Cycle detected in workflow graph")]
    CycleDetected,

    #[error("Duplicate node ID: {0}")]
    DuplicateNode(String),

    #[error("No entry point defined")]
    NoEntryPoint,

    #[error("Unreachable nodes: {0:?}")]
    UnreachableNodes(Vec<String>),

    #[error("Serialization error: {0}")]
    Serialization(String),
}
