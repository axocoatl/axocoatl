//! Built-in tools that run in-process (no isolation needed).

use crate::error::ToolError;

/// Trait for built-in tools. Executed in the axocoatl-daemon process directly.
#[async_trait::async_trait]
pub trait BuiltinTool: Send + Sync + 'static {
    /// Human-readable description for LLM tool calling.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given arguments.
    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError>;
}

// --- Built-in tool implementations ---

/// Echo tool — returns the input unchanged. Useful for testing.
pub struct EchoTool;

#[async_trait::async_trait]
impl BuiltinTool for EchoTool {
    fn description(&self) -> &str {
        "Echo the input back unchanged"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo" }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(arguments)
    }
}

/// JSON keys extraction tool — returns the top-level keys of a JSON object.
pub struct JsonKeysTool;

#[async_trait::async_trait]
impl BuiltinTool for JsonKeysTool {
    fn description(&self) -> &str {
        "Extract the top-level keys from a JSON object"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "json": { "type": "object", "description": "JSON object to extract keys from" }
            },
            "required": ["json"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let obj = arguments
            .get("json")
            .and_then(|v| v.as_object())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "json_keys".to_string(),
                reason: "Expected 'json' field with an object value".to_string(),
            })?;

        let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        Ok(serde_json::json!({"keys": keys}))
    }
}

/// Text split tool — splits text by a delimiter.
pub struct TextSplitTool;

#[async_trait::async_trait]
impl BuiltinTool for TextSplitTool {
    fn description(&self) -> &str {
        "Split text by a delimiter and return the parts"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to split" },
                "delimiter": { "type": "string", "description": "Delimiter (default: newline)" }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let text = arguments
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "text_split".to_string(),
                reason: "Expected 'text' string field".to_string(),
            })?;

        let delimiter = arguments
            .get("delimiter")
            .and_then(|v| v.as_str())
            .unwrap_or("\n");

        let parts: Vec<&str> = text.split(delimiter).collect();
        Ok(serde_json::json!({"parts": parts, "count": parts.len()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_returns_input() {
        let tool = EchoTool;
        let result = tool
            .execute(serde_json::json!({"text": "hello", "extra": 42}))
            .await
            .unwrap();
        assert_eq!(result["text"], "hello");
        assert_eq!(result["extra"], 42);
    }

    #[tokio::test]
    async fn json_keys_extracts_keys() {
        let tool = JsonKeysTool;
        let result = tool
            .execute(serde_json::json!({"json": {"name": "Alice", "age": 30, "city": "NYC"}}))
            .await
            .unwrap();
        let keys = result["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 3);
    }

    #[tokio::test]
    async fn json_keys_invalid_input() {
        let tool = JsonKeysTool;
        let result = tool
            .execute(serde_json::json!({"json": "not an object"}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgs { .. })));
    }

    #[tokio::test]
    async fn text_split_default_newline() {
        let tool = TextSplitTool;
        let result = tool
            .execute(serde_json::json!({"text": "line1\nline2\nline3"}))
            .await
            .unwrap();
        assert_eq!(result["count"], 3);
        assert_eq!(result["parts"][0], "line1");
    }

    #[tokio::test]
    async fn text_split_custom_delimiter() {
        let tool = TextSplitTool;
        let result = tool
            .execute(serde_json::json!({"text": "a,b,c", "delimiter": ","}))
            .await
            .unwrap();
        assert_eq!(result["count"], 3);
    }
}
