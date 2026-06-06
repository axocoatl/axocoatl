use serde::{Deserialize, Serialize};

/// The canonical tool-call request shape. Defined in `axocoatl-core` (because
/// `ChatMessage` carries it) and re-exported here so `axocoatl_llm::ToolCall`
/// keeps resolving for every existing call site.
pub use axocoatl_core::ToolCall;

/// How a tool may be executed concurrently with other tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConcurrencyPolicy {
    /// Tool is safe to run in parallel with any other Safe tool (e.g., read_file, search).
    #[default]
    Safe,
    /// Tool must run exclusively — no other tools run while it executes (e.g., write_file, shell).
    Exclusive,
    /// Tool runs in submission order within its group, but the group runs in parallel with Safe tools.
    Ordered,
}

/// Definition of a tool that can be called by an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
    /// Concurrency policy for parallel tool execution.
    #[serde(default)]
    pub concurrency: ConcurrencyPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_serde_roundtrip() {
        let tool = ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                },
                "required": ["location"]
            }),
            concurrency: ConcurrencyPolicy::Safe,
        };
        let json = serde_json::to_string(&tool).unwrap();
        let back: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "get_weather");
    }

    #[test]
    fn tool_call_serde_roundtrip() {
        let call = ToolCall {
            id: "call_123".to_string(),
            name: "get_weather".to_string(),
            arguments: serde_json::json!({"location": "NYC"}),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call_123");
        assert_eq!(back.arguments["location"], "NYC");
    }
}
