//! Shared model-facing tool payload limits.
//!
//! Tool-specific code should reject oversized inputs before doing work and
//! return useful bounded structures. This module is the final executor safety
//! net: an unexpectedly large builtin or MCP value becomes an explicit JSON
//! preview before it can be appended to model history.

use std::io::{self, Write};

use crate::error::ToolError;

pub(crate) const TOOL_ARGUMENT_MAX_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const TOOL_NAME_MAX_BYTES: usize = 4 * 1024;
pub(crate) const TOOL_OUTPUT_MAX_BYTES: usize = 256 * 1024;
pub(crate) const TOOL_OUTPUT_PREVIEW_MAX_BYTES: usize = 64 * 1024;
pub(crate) const TOOL_ERROR_MAX_BYTES: usize = 8 * 1024;
pub(crate) const SIMPLE_TOOL_INPUT_MAX_BYTES: usize = 1024 * 1024;

const WRITER_LIMIT_ERROR: &str = "Axocoatl bounded JSON writer reached its limit";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LimitedText {
    pub(crate) text: String,
    pub(crate) truncated: bool,
    pub(crate) original_bytes: usize,
}

pub(crate) fn limit_text(mut text: String, max_bytes: usize) -> LimitedText {
    let original_bytes = text.len();
    if original_bytes <= max_bytes {
        return LimitedText {
            text,
            truncated: false,
            original_bytes,
        };
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    LimitedText {
        text,
        truncated: true,
        original_bytes,
    }
}

pub(crate) fn limit_error_text(text: impl Into<String>) -> String {
    let limited = limit_text(text.into(), TOOL_ERROR_MAX_BYTES);
    if limited.truncated {
        format!(
            "{}\n[error detail truncated: captured {} bytes; limit {} bytes]",
            limited.text, limited.original_bytes, TOOL_ERROR_MAX_BYTES
        )
    } else {
        limited.text
    }
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedJsonWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if buf.len() <= remaining {
            self.bytes.extend_from_slice(buf);
            return Ok(buf.len());
        }

        self.bytes.extend_from_slice(&buf[..remaining]);
        self.exceeded = true;
        Err(io::Error::other(WRITER_LIMIT_ERROR))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum JsonSize {
    Within,
    Exceeded { prefix: Vec<u8> },
}

fn inspect_json_size(
    value: &serde_json::Value,
    max_bytes: usize,
) -> Result<JsonSize, serde_json::Error> {
    let capture_bytes = max_bytes.saturating_add(1);
    let mut writer = BoundedJsonWriter::new(capture_bytes);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) if writer.bytes.len() <= max_bytes => Ok(JsonSize::Within),
        Ok(()) => Ok(JsonSize::Exceeded {
            prefix: writer.bytes,
        }),
        Err(_) if writer.exceeded => Ok(JsonSize::Exceeded {
            prefix: writer.bytes,
        }),
        Err(error) => Err(error),
    }
}

pub(crate) fn ensure_json_input(
    value: &serde_json::Value,
    tool: &str,
    max_bytes: usize,
) -> Result<(), ToolError> {
    match inspect_json_size(value, max_bytes)? {
        JsonSize::Within => Ok(()),
        JsonSize::Exceeded { .. } => Err(ToolError::InvalidArgs {
            tool: tool.to_string(),
            reason: format!(
                "serialized arguments exceed the {max_bytes}-byte limit. Narrow or split the operation."
            ),
        }),
    }
}

pub(crate) fn bound_tool_output(value: serde_json::Value) -> Result<serde_json::Value, ToolError> {
    match inspect_json_size(&value, TOOL_OUTPUT_MAX_BYTES)? {
        JsonSize::Within => Ok(value),
        JsonSize::Exceeded { prefix } => {
            let valid = match std::str::from_utf8(&prefix) {
                Ok(_) => prefix.len(),
                Err(error) => error.valid_up_to(),
            };
            let valid_text = std::str::from_utf8(&prefix[..valid]).map_err(|error| {
                ToolError::ExecutionFailed {
                    tool: "tool_output_boundary".to_string(),
                    reason: format!("bounded JSON preview was not UTF-8: {error}"),
                }
            })?;
            let mut preview_end = valid_text.len().min(TOOL_OUTPUT_PREVIEW_MAX_BYTES);
            while !valid_text.is_char_boundary(preview_end) {
                preview_end -= 1;
            }
            let preview = valid_text[..preview_end].to_string();
            Ok(serde_json::json!({
                "tool_result_truncated": true,
                "message": format!(
                    "Tool result exceeded the {TOOL_OUTPUT_MAX_BYTES}-byte model-transport limit. Narrow the request or use a tool-specific filter."
                ),
                "preview": preview,
                "preview_format": "serialized_json_prefix",
                "preview_bytes": preview_end,
                "original_bytes_at_least": TOOL_OUTPUT_MAX_BYTES + 1,
                "output_limit_bytes": TOOL_OUTPUT_MAX_BYTES,
            }))
        }
    }
}

pub(crate) fn bound_tool_error(error: ToolError) -> ToolError {
    match error {
        ToolError::NotFound(name) => ToolError::NotFound(limit_error_text(name)),
        ToolError::ExecutionFailed { tool, reason } => ToolError::ExecutionFailed {
            tool: limit_error_text(tool),
            reason: limit_error_text(reason),
        },
        ToolError::InvalidArgs { tool, reason } => ToolError::InvalidArgs {
            tool: limit_error_text(tool),
            reason: limit_error_text(reason),
        },
        ToolError::Serialization(error) => ToolError::Serialization(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_writer_stops_without_serializing_an_entire_value() {
        let value = serde_json::json!({"payload": "x".repeat(TOOL_OUTPUT_MAX_BYTES * 4)});
        let JsonSize::Exceeded { prefix } =
            inspect_json_size(&value, TOOL_OUTPUT_MAX_BYTES).unwrap()
        else {
            panic!("oversized value was accepted");
        };
        assert_eq!(prefix.len(), TOOL_OUTPUT_MAX_BYTES + 1);
    }

    #[test]
    fn output_wrapper_is_bounded_utf8_and_explicit() {
        let value = serde_json::json!({"payload": "🦀".repeat(TOOL_OUTPUT_MAX_BYTES)});
        let bounded = bound_tool_output(value).unwrap();
        assert_eq!(bounded["tool_result_truncated"], true);
        let preview = bounded["preview"].as_str().unwrap();
        assert!(preview.len() <= TOOL_OUTPUT_PREVIEW_MAX_BYTES);
        assert!(preview.is_char_boundary(preview.len()));
        assert!(serde_json::to_vec(&bounded).unwrap().len() < TOOL_OUTPUT_MAX_BYTES);
    }

    #[test]
    fn small_output_is_unchanged() {
        let value = serde_json::json!({"ok": true, "value": [1, 2, 3]});
        assert_eq!(bound_tool_output(value.clone()).unwrap(), value);
    }

    #[test]
    fn input_limit_rejects_at_the_boundary() {
        let value = serde_json::json!({"payload": "x".repeat(1024)});
        assert!(ensure_json_input(&value, "example", 100).is_err());
        assert!(ensure_json_input(&value, "example", 2048).is_ok());
    }
}
