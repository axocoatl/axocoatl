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
            provider_metadata: Default::default(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call_123");
        assert_eq!(back.arguments["location"], "NYC");
    }

    /// A number argument is the exact f64 it names after every parse a call
    /// goes through (the provider's response, its re-encoded arguments, the
    /// actor's assembled call and the stored record). serde_json's default
    /// parser lands 1 ULP off for about a third of random bit patterns, such
    /// as -5.220838365510166e38; the workspace enables `float_roundtrip`.
    #[test]
    fn a_tool_calls_number_arguments_parse_to_the_exact_f64() {
        let known = -5.220838365510166e38_f64;
        let parsed: serde_json::Value =
            serde_json::from_str(&format!("{{\"x\":{}}}", serde_json::json!(known))).unwrap();
        assert_eq!(
            parsed["x"].as_f64().map(f64::to_bits),
            Some(known.to_bits())
        );
        proptest::proptest!(proptest::prelude::ProptestConfig::with_cases(4096), |(
            bits in proptest::prelude::any::<u64>()
        )| {
            let value = f64::from_bits(bits);
            proptest::prop_assume!(value.is_finite());
            let call = ToolCall {
                id: "call_1".to_string(),
                name: "measure".to_string(),
                arguments: serde_json::json!({ "value": value, "nested": [value] }),
                provider_metadata: Default::default(),
            };
            // The arguments as a provider sends them, a JSON string.
            let text = serde_json::to_string(&call.arguments).unwrap();
            let arguments: serde_json::Value = serde_json::from_str(&text).unwrap();
            proptest::prop_assert_eq!(arguments["value"].as_f64().map(f64::to_bits), Some(bits));
            proptest::prop_assert_eq!(arguments["nested"][0].as_f64().map(f64::to_bits), Some(bits));
            // And the call as a record stores it.
            let stored: ToolCall = serde_json::from_slice(&serde_json::to_vec(&call).unwrap()).unwrap();
            proptest::prop_assert_eq!(stored.arguments["value"].as_f64().map(f64::to_bits), Some(bits));
        });
    }
}
