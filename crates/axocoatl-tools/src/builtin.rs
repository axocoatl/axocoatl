//! Built-in tools that run in-process (no isolation needed).

use crate::error::ToolError;
use crate::limits::{ensure_json_input, SIMPLE_TOOL_INPUT_MAX_BYTES};

const ECHO_TEXT_MAX_BYTES: usize = 256 * 1024;
const JSON_KEYS_MAX_RETURNED: usize = 1024;
const JSON_KEYS_MAX_KEY_BYTES: usize = 4 * 1024;
const JSON_KEYS_OUTPUT_TEXT_MAX_BYTES: usize = 64 * 1024;
const TEXT_SPLIT_TEXT_MAX_BYTES: usize = 256 * 1024;
const TEXT_SPLIT_DELIMITER_MAX_BYTES: usize = 4 * 1024;
const TEXT_SPLIT_MAX_PARTS: usize = 4096;

fn validate_text_bytes(
    value: &str,
    field: &str,
    tool: &str,
    max_bytes: usize,
) -> Result<(), ToolError> {
    if value.len() <= max_bytes {
        return Ok(());
    }
    Err(ToolError::InvalidArgs {
        tool: tool.to_string(),
        reason: format!(
            "field '{field}' is {} bytes; the limit is {max_bytes} bytes. Narrow or split the operation.",
            value.len()
        ),
    })
}

/// Trait for built-in tools. Executed in the axocoatl-daemon process directly.
#[async_trait::async_trait]
pub trait BuiltinTool: Send + Sync + 'static {
    /// Human-readable description for LLM tool calling.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Concurrency contract enforced for a provider's parallel tool-call
    /// group. Read-only tools may keep the safe default; stateful mutators and
    /// process/shell tools must opt into `Exclusive`.
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Safe
    }

    /// Execute the tool with the given arguments.
    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError>;
}

// --- Built-in tool implementations ---

/// Echo tool — returns the input unchanged. Useful for testing.
pub struct EchoTool;

#[async_trait::async_trait]
impl BuiltinTool for EchoTool {
    fn description(&self) -> &str {
        "Echo bounded input back unchanged (text maximum 256 KiB)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to echo (maximum 256 KiB)" }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        ensure_json_input(&arguments, "echo", SIMPLE_TOOL_INPUT_MAX_BYTES)?;
        let text = arguments
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "echo".to_string(),
                reason: "expected string field 'text'".to_string(),
            })?;
        validate_text_bytes(text, "text", "echo", ECHO_TEXT_MAX_BYTES)?;
        Ok(arguments)
    }
}

/// JSON keys extraction tool — returns the top-level keys of a JSON object.
pub struct JsonKeysTool;

#[async_trait::async_trait]
impl BuiltinTool for JsonKeysTool {
    fn description(&self) -> &str {
        "Extract a bounded list of top-level keys from a JSON object"
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
        ensure_json_input(&arguments, "json_keys", SIMPLE_TOOL_INPUT_MAX_BYTES)?;
        let obj = arguments
            .get("json")
            .and_then(|v| v.as_object())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "json_keys".to_string(),
                reason: "Expected 'json' field with an object value".to_string(),
            })?;

        if let Some(key) = obj.keys().find(|key| key.len() > JSON_KEYS_MAX_KEY_BYTES) {
            return Err(ToolError::InvalidArgs {
                tool: "json_keys".to_string(),
                reason: format!(
                    "an object key is {} bytes; the per-key limit is {JSON_KEYS_MAX_KEY_BYTES} bytes",
                    key.len()
                ),
            });
        }

        let total_count = obj.len();
        let mut returned_key_bytes = 0_usize;
        let mut keys = Vec::new();
        for key in obj.keys() {
            let Some(next_bytes) = returned_key_bytes.checked_add(key.len()) else {
                break;
            };
            if keys.len() >= JSON_KEYS_MAX_RETURNED || next_bytes > JSON_KEYS_OUTPUT_TEXT_MAX_BYTES
            {
                break;
            }
            returned_key_bytes = next_bytes;
            keys.push(key.as_str());
        }
        let count = keys.len();
        Ok(serde_json::json!({
            "keys": keys,
            "count": count,
            "total_count": total_count,
            "truncated": count < total_count,
            "entry_limit": JSON_KEYS_MAX_RETURNED,
            "key_text_limit_bytes": JSON_KEYS_OUTPUT_TEXT_MAX_BYTES,
        }))
    }
}

/// Text split tool — splits text by a delimiter.
pub struct TextSplitTool;

#[async_trait::async_trait]
impl BuiltinTool for TextSplitTool {
    fn description(&self) -> &str {
        "Split up to 256 KiB of text by a non-empty delimiter and return at most 4096 parts"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Text to split (maximum 256 KiB)" },
                "delimiter": { "type": "string", "description": "Non-empty delimiter (default: newline; maximum 4 KiB)" }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        ensure_json_input(&arguments, "text_split", SIMPLE_TOOL_INPUT_MAX_BYTES)?;
        let text = arguments
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "text_split".to_string(),
                reason: "Expected 'text' string field".to_string(),
            })?;

        validate_text_bytes(text, "text", "text_split", TEXT_SPLIT_TEXT_MAX_BYTES)?;
        let delimiter = match arguments.get("delimiter") {
            None => "\n",
            Some(value) => value.as_str().ok_or_else(|| ToolError::InvalidArgs {
                tool: "text_split".to_string(),
                reason: "field 'delimiter' must be a string".to_string(),
            })?,
        };
        validate_text_bytes(
            delimiter,
            "delimiter",
            "text_split",
            TEXT_SPLIT_DELIMITER_MAX_BYTES,
        )?;
        if delimiter.is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: "text_split".to_string(),
                reason: "field 'delimiter' must not be empty".to_string(),
            });
        }

        let mut total_count = 0_usize;
        let mut parts = Vec::new();
        for part in text.split(delimiter) {
            total_count = total_count.saturating_add(1);
            if parts.len() < TEXT_SPLIT_MAX_PARTS {
                parts.push(part);
            }
        }
        let count = parts.len();
        Ok(serde_json::json!({
            "parts": parts,
            "count": count,
            "total_count": total_count,
            "truncated": count < total_count,
            "part_limit": TEXT_SPLIT_MAX_PARTS,
        }))
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

    #[tokio::test]
    async fn echo_rejects_oversized_text() {
        let result = EchoTool
            .execute(serde_json::json!({"text": "x".repeat(ECHO_TEXT_MAX_BYTES + 1)}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgs { .. })));
    }

    #[tokio::test]
    async fn json_keys_truncates_deterministically() {
        let mut object = serde_json::Map::new();
        for index in 0..(JSON_KEYS_MAX_RETURNED + 10) {
            object.insert(format!("key-{index:05}"), serde_json::Value::Null);
        }
        let result = JsonKeysTool
            .execute(serde_json::json!({"json": object}))
            .await
            .unwrap();
        assert_eq!(result["count"], JSON_KEYS_MAX_RETURNED as u64);
        assert_eq!(result["total_count"], (JSON_KEYS_MAX_RETURNED + 10) as u64);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["keys"][0], "key-00000");
    }

    #[tokio::test]
    async fn json_keys_rejects_a_pathological_key() {
        let mut object = serde_json::Map::new();
        object.insert(
            "k".repeat(JSON_KEYS_MAX_KEY_BYTES + 1),
            serde_json::Value::Null,
        );
        let result = JsonKeysTool
            .execute(serde_json::json!({"json": object}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArgs { .. })));
    }

    #[tokio::test]
    async fn text_split_bounds_parts_and_rejects_empty_delimiter() {
        let text = std::iter::repeat_n("x", TEXT_SPLIT_MAX_PARTS + 10)
            .collect::<Vec<_>>()
            .join(",");
        let result = TextSplitTool
            .execute(serde_json::json!({"text": text, "delimiter": ","}))
            .await
            .unwrap();
        assert_eq!(result["count"], TEXT_SPLIT_MAX_PARTS as u64);
        assert_eq!(result["total_count"], (TEXT_SPLIT_MAX_PARTS + 10) as u64);
        assert_eq!(result["truncated"], true);

        let empty = TextSplitTool
            .execute(serde_json::json!({"text": "abc", "delimiter": ""}))
            .await;
        assert!(matches!(empty, Err(ToolError::InvalidArgs { .. })));
    }
}
