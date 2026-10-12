//! A workflow written as YAML by Axocoatl before 1.3.1 (serde_yaml 0.9) reads
//! back to the same graph, and what Axocoatl writes now reads back too.
//!
//! `tests/yaml_golden/workflow.yaml` is serde_yaml's output for the graph
//! below and `workflow.json` that graph as JSON, both written with
//! `AXOCOATL_YAML_GOLDEN=write` while serde_yaml was still the writer.

use std::path::{Path, PathBuf};

use axocoatl_graph::{EdgeType, NodeType, WorkflowEdge, WorkflowGraph, WorkflowNode};
use serde_json::json;

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/yaml_golden")
        .join(name)
}

fn node(id: &str, node_type: NodeType, label: &str, config: serde_json::Value) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        node_type,
        label: label.into(),
        config,
    }
}

fn edge(edge_type: EdgeType, label: Option<&str>, condition: Option<&str>) -> WorkflowEdge {
    WorkflowEdge {
        edge_type,
        label: label.map(Into::into),
        condition: condition.map(Into::into),
    }
}

/// A graph with every node and edge type and configuration values YAML
/// could mistake for something else.
fn sample() -> WorkflowGraph {
    let mut graph = WorkflowGraph::new();
    let tricky = json!({
        "model": "gpt-4o",
        "temperature": 0.7,
        "max_tokens": 4096,
        "negative": -3,
        "enabled": true,
        "nothing": null,
        "strings": ["yes", "no", "on", "off", "true", "null", "~", "", "017", "0x1F", "1e3",
                    ".inf", "1_000", "12:30", "- dash", "a: b", "# hash", "'quoted'", "\"dq\"",
                    " lead", "trail ", "!tag", "&anchor", "*alias", "%pct", "@at", "`tick",
                    "{brace}", "[bracket]", "multi\nline\n", "tab\there", "é ü ö", "😀"],
        "nested": {"list": [1, 2.5, false, {"deep": ["x"]}], "empty_list": [], "empty_map": {}},
        "prompt": "You are a reviewer.\nCheck the diff.\n\nReply with findings.\n",
    });
    graph
        .add_node(node("start", NodeType::Entry, "Start", json!({})))
        .unwrap();
    graph
        .add_node(node("writer", NodeType::Agent, "Writer: drafts", tricky))
        .unwrap();
    graph
        .add_node(node(
            "lint",
            NodeType::Tool,
            "lint",
            json!({"command": "cargo clippy -- -D warnings"}),
        ))
        .unwrap();
    graph
        .add_node(node("route", NodeType::Decision, "yes", json!(null)))
        .unwrap();
    graph
        .add_node(node("join", NodeType::Join, "017", json!([1, "two", null])))
        .unwrap();
    graph
        .add_node(node("end", NodeType::Exit, "", json!("done")))
        .unwrap();
    graph
        .add_edge("start", "writer", edge(EdgeType::Sequential, None, None))
        .unwrap();
    graph
        .add_edge(
            "writer",
            "lint",
            edge(EdgeType::Parallel, Some("check"), None),
        )
        .unwrap();
    graph
        .add_edge(
            "writer",
            "route",
            edge(EdgeType::Parallel, Some("on"), None),
        )
        .unwrap();
    graph
        .add_edge(
            "route",
            "join",
            edge(EdgeType::Conditional, Some("ok"), Some("result == 'pass'")),
        )
        .unwrap();
    graph
        .add_edge("lint", "join", edge(EdgeType::Sequential, None, Some("")))
        .unwrap();
    graph
        .add_edge(
            "join",
            "end",
            edge(EdgeType::Sequential, Some("null"), None),
        )
        .unwrap();
    graph
}

fn as_json(graph: &WorkflowGraph) -> serde_json::Value {
    serde_json::to_value(graph).unwrap()
}

#[test]
fn workflow_yaml_written_before_reads_back_and_new_yaml_round_trips() {
    let graph = sample();
    if std::env::var("AXOCOATL_YAML_GOLDEN").as_deref() == Ok("write") {
        std::fs::create_dir_all(golden("")).unwrap();
        std::fs::write(golden("workflow.yaml"), graph.to_yaml().unwrap()).unwrap();
        let mut text = serde_json::to_string_pretty(&as_json(&graph)).unwrap();
        text.push('\n');
        std::fs::write(golden("workflow.json"), text).unwrap();
        return;
    }
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(golden("workflow.json")).unwrap()).unwrap();
    assert_eq!(as_json(&graph), expected, "the sample graph changed");

    // What serde_yaml wrote reads back to the same graph.
    let old_text = std::fs::read_to_string(golden("workflow.yaml")).unwrap();
    let restored = WorkflowGraph::from_yaml(&old_text).unwrap();
    assert_eq!(as_json(&restored), expected);
    assert_eq!(restored.node_count(), 6);
    assert_eq!(restored.edge_count(), 6);
    assert_eq!(
        restored.topological_order().unwrap(),
        graph.topological_order().unwrap()
    );

    // What is written now reads back to the same graph.
    let new_text = graph.to_yaml().unwrap();
    let round_tripped = WorkflowGraph::from_yaml(&new_text).unwrap();
    assert_eq!(as_json(&round_tripped), expected, "{new_text}");
    assert_eq!(round_tripped.parallel_groups(), graph.parallel_groups());
}
