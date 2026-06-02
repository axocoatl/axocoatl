use crate::types::{ValidationError, ValidationResult};
use crate::workflow::WorkflowGraph;

/// Validate a workflow graph for structural correctness.
pub fn validate_graph(graph: &WorkflowGraph) -> ValidationResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Check for cycles
    if graph.topological_order().is_err() {
        errors.push(ValidationError {
            node_id: None,
            edge_id: None,
            message: "Graph contains a cycle — workflows must be acyclic".to_string(),
        });
    }

    // Check node count
    if graph.node_count() == 0 {
        warnings.push("Graph is empty — no nodes defined".to_string());
    }

    ValidationResult {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use crate::workflow::WorkflowGraph;

    fn agent_node(id: &str) -> WorkflowNode {
        WorkflowNode {
            id: id.to_string(),
            node_type: NodeType::Agent,
            label: id.to_string(),
            config: serde_json::json!({}),
        }
    }

    fn seq_edge() -> WorkflowEdge {
        WorkflowEdge {
            edge_type: EdgeType::Sequential,
            label: None,
            condition: None,
        }
    }

    #[test]
    fn valid_graph_passes() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        g.add_node(agent_node("b")).unwrap();
        g.add_edge("a", "b", seq_edge()).unwrap();

        let result = validate_graph(&g);
        assert!(result.valid);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn cyclic_graph_fails() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        g.add_node(agent_node("b")).unwrap();
        g.add_edge("a", "b", seq_edge()).unwrap();
        g.add_edge("b", "a", seq_edge()).unwrap();

        let result = validate_graph(&g);
        assert!(!result.valid);
        assert!(result.errors[0].message.contains("cycle"));
    }

    #[test]
    fn empty_graph_warns() {
        let g = WorkflowGraph::new();
        let result = validate_graph(&g);
        assert!(result.valid);
        assert!(!result.warnings.is_empty());
    }
}
