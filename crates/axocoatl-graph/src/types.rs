use serde::{Deserialize, Serialize};

/// A node in the workflow graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNode {
    pub id: String,
    pub node_type: NodeType,
    pub label: String,
    pub config: serde_json::Value,
}

/// Types of nodes in the workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeType {
    /// An LLM agent that processes input.
    Agent,
    /// A tool execution node.
    Tool,
    /// A conditional branch point.
    Decision,
    /// A join point that waits for all incoming edges.
    Join,
    /// Entry point of the workflow.
    Entry,
    /// Exit point of the workflow.
    Exit,
}

/// An edge connecting two nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowEdge {
    pub edge_type: EdgeType,
    pub label: Option<String>,
    /// Optional condition expression (for Decision edges).
    pub condition: Option<String>,
}

/// Types of edges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EdgeType {
    /// Sequential: target runs after source completes.
    Sequential,
    /// Parallel: target can run concurrently with other parallel edges from the same source.
    Parallel,
    /// Conditional: target runs only if condition is true.
    Conditional,
}

/// Result of graph validation.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationError {
    pub node_id: Option<String>,
    pub edge_id: Option<String>,
    pub message: String,
}

impl ValidationResult {
    pub fn ok() -> Self {
        Self {
            valid: true,
            errors: vec![],
            warnings: vec![],
        }
    }
}
