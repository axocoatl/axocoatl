use std::collections::HashMap;

use petgraph::stable_graph::{NodeIndex, StableGraph};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::{Deserialize, Serialize};

use crate::error::GraphError;
use crate::types::{EdgeType, WorkflowEdge, WorkflowNode};

/// A workflow graph — the canonical representation of an agent workflow.
/// Built on petgraph's StableGraph (stable indices survive node removal).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowGraph {
    #[serde(skip)]
    graph: StableGraph<WorkflowNode, WorkflowEdge>,
    /// Map from node ID string to petgraph NodeIndex.
    #[serde(skip)]
    index_map: HashMap<String, NodeIndex>,
    /// Serializable representation for YAML export.
    nodes: Vec<WorkflowNode>,
    edges: Vec<SerializableEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SerializableEdge {
    source: String,
    target: String,
    #[serde(flatten)]
    edge: WorkflowEdge,
}

impl WorkflowGraph {
    pub fn new() -> Self {
        Self {
            graph: StableGraph::new(),
            index_map: HashMap::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Add a node to the graph.
    pub fn add_node(&mut self, node: WorkflowNode) -> Result<NodeIndex, GraphError> {
        if self.index_map.contains_key(&node.id) {
            return Err(GraphError::DuplicateNode(node.id.clone()));
        }
        let id = node.id.clone();
        self.nodes.push(node.clone());
        let idx = self.graph.add_node(node);
        self.index_map.insert(id, idx);
        Ok(idx)
    }

    /// Add an edge between two nodes.
    pub fn add_edge(
        &mut self,
        source_id: &str,
        target_id: &str,
        edge: WorkflowEdge,
    ) -> Result<(), GraphError> {
        let source = *self
            .index_map
            .get(source_id)
            .ok_or_else(|| GraphError::NodeNotFound(source_id.to_string()))?;
        let target = *self
            .index_map
            .get(target_id)
            .ok_or_else(|| GraphError::NodeNotFound(target_id.to_string()))?;

        self.edges.push(SerializableEdge {
            source: source_id.to_string(),
            target: target_id.to_string(),
            edge: edge.clone(),
        });
        self.graph.add_edge(source, target, edge);
        Ok(())
    }

    /// Get a node by ID.
    pub fn get_node(&self, id: &str) -> Option<&WorkflowNode> {
        self.index_map
            .get(id)
            .and_then(|idx| self.graph.node_weight(*idx))
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// Get topological ordering of nodes (for execution).
    /// Returns Err if graph has cycles.
    pub fn topological_order(&self) -> Result<Vec<String>, GraphError> {
        let topo =
            petgraph::algo::toposort(&self.graph, None).map_err(|_| GraphError::CycleDetected)?;

        Ok(topo
            .into_iter()
            .filter_map(|idx| self.graph.node_weight(idx).map(|n| n.id.clone()))
            .collect())
    }

    /// Find nodes that can execute in parallel (share the same predecessor).
    pub fn parallel_groups(&self) -> Vec<Vec<String>> {
        let mut groups: Vec<Vec<String>> = Vec::new();

        for idx in self.graph.node_indices() {
            let outgoing: Vec<_> = self
                .graph
                .edges_directed(idx, Direction::Outgoing)
                .filter(|e| e.weight().edge_type == EdgeType::Parallel)
                .map(|e| {
                    self.graph
                        .node_weight(e.target())
                        .map(|n| n.id.clone())
                        .unwrap_or_default()
                })
                .collect();

            if outgoing.len() > 1 {
                groups.push(outgoing);
            }
        }

        groups
    }

    /// Serialize to YAML.
    pub fn to_yaml(&self) -> Result<String, GraphError> {
        serde_yaml::to_string(self).map_err(|e| GraphError::Serialization(e.to_string()))
    }

    /// Deserialize from YAML.
    pub fn from_yaml(yaml: &str) -> Result<Self, GraphError> {
        let mut graph: WorkflowGraph =
            serde_yaml::from_str(yaml).map_err(|e| GraphError::Serialization(e.to_string()))?;

        // Rebuild the petgraph from serialized nodes/edges
        graph.graph = StableGraph::new();
        graph.index_map = HashMap::new();

        let nodes = std::mem::take(&mut graph.nodes);
        let edges = std::mem::take(&mut graph.edges);

        for node in &nodes {
            let idx = graph.graph.add_node(node.clone());
            graph.index_map.insert(node.id.clone(), idx);
        }
        graph.nodes = nodes;

        for se in &edges {
            if let (Some(&src), Some(&tgt)) = (
                graph.index_map.get(&se.source),
                graph.index_map.get(&se.target),
            ) {
                graph.graph.add_edge(src, tgt, se.edge.clone());
            }
        }
        graph.edges = edges;

        Ok(graph)
    }
}

impl Default for WorkflowGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

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

    fn par_edge() -> WorkflowEdge {
        WorkflowEdge {
            edge_type: EdgeType::Parallel,
            label: None,
            condition: None,
        }
    }

    #[test]
    fn build_sequential_graph() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        g.add_node(agent_node("b")).unwrap();
        g.add_node(agent_node("c")).unwrap();
        g.add_edge("a", "b", seq_edge()).unwrap();
        g.add_edge("b", "c", seq_edge()).unwrap();

        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);

        let order = g.topological_order().unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn duplicate_node_rejected() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        assert!(g.add_node(agent_node("a")).is_err());
    }

    #[test]
    fn edge_to_nonexistent_node() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        assert!(g.add_edge("a", "ghost", seq_edge()).is_err());
    }

    #[test]
    fn cycle_detection() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        g.add_node(agent_node("b")).unwrap();
        g.add_edge("a", "b", seq_edge()).unwrap();
        g.add_edge("b", "a", seq_edge()).unwrap();

        assert!(g.topological_order().is_err());
    }

    #[test]
    fn parallel_fan_out() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("root")).unwrap();
        g.add_node(agent_node("worker1")).unwrap();
        g.add_node(agent_node("worker2")).unwrap();
        g.add_node(agent_node("worker3")).unwrap();
        g.add_edge("root", "worker1", par_edge()).unwrap();
        g.add_edge("root", "worker2", par_edge()).unwrap();
        g.add_edge("root", "worker3", par_edge()).unwrap();

        let groups = g.parallel_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn get_node() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("test")).unwrap();
        let node = g.get_node("test").unwrap();
        assert_eq!(node.node_type, NodeType::Agent);
        assert!(g.get_node("missing").is_none());
    }

    #[test]
    fn yaml_roundtrip() {
        let mut g = WorkflowGraph::new();
        g.add_node(agent_node("a")).unwrap();
        g.add_node(agent_node("b")).unwrap();
        g.add_edge("a", "b", seq_edge()).unwrap();

        let yaml = g.to_yaml().unwrap();
        let restored = WorkflowGraph::from_yaml(&yaml).unwrap();

        assert_eq!(restored.node_count(), 2);
        assert_eq!(restored.edge_count(), 1);
        assert!(restored.get_node("a").is_some());
        assert!(restored.get_node("b").is_some());
    }

    #[test]
    fn empty_graph() {
        let g = WorkflowGraph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
        assert!(g.topological_order().unwrap().is_empty());
    }
}
