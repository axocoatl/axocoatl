//! Placeholders for stale tool output and long tool-call arguments.
//!
//! Shared by request-side masking of stale tool output (in the actor) and by
//! history compaction (Stage 2), so a value is elided the same way wherever
//! it happens, and never twice. Call ids, names and argument keys stay, so a
//! replayed conversation remains well formed.

use axocoatl_core::{ChatMessage, MessageContent, MessageRole};

/// Every placeholder starts with this, so an elided value is left alone.
pub const ELIDED_PREFIX: &str = "[earlier ";

/// Replace a Tool result's text of at least `min_chars` characters with a
/// placeholder naming the tool. Returns whether it was replaced.
pub fn elide_tool_output(message: &mut ChatMessage, min_chars: usize) -> bool {
    if message.role != MessageRole::Tool {
        return false;
    }
    let MessageContent::Text(text) = &message.content else {
        return false;
    };
    let chars = text.chars().count();
    if chars < min_chars || text.starts_with(ELIDED_PREFIX) {
        return false;
    }
    let name = message.name.as_deref().unwrap_or("tool");
    message.content = MessageContent::Text(format!(
        "[earlier {name} output ({chars} characters) removed to save context; \
         run it again, narrower, if you still need it]"
    ));
    true
}

/// Elide string arguments of at least `min_chars` characters in an Assistant
/// message's tool calls, except calls `keep` names. The model's own earlier
/// calls re-send their arguments too; a whole-file write repeats the file on
/// every request. A provider that replays its own record of the turn
/// (Anthropic content blocks, Gemini parts) checks the calls against it, so a
/// message carrying such metadata keeps its arguments. Returns how many
/// values were elided.
pub fn elide_call_arguments(
    message: &mut ChatMessage,
    min_chars: usize,
    keep: impl Fn(&str) -> bool,
) -> usize {
    if message.role != MessageRole::Assistant
        || message
            .tool_calls
            .iter()
            .any(|call| !call.provider_metadata.is_empty())
    {
        return 0;
    }
    message
        .tool_calls
        .iter_mut()
        .filter(|call| !keep(&call.name))
        .map(|call| elide_long_strings(&mut call.arguments, min_chars))
        .sum()
}

/// Replace string values of at least `min_chars` characters, at any depth,
/// with a placeholder. Returns how many were replaced.
pub fn elide_long_strings(value: &mut serde_json::Value, min_chars: usize) -> usize {
    match value {
        serde_json::Value::String(text) => {
            let chars = text.chars().count();
            if chars < min_chars || text.starts_with(ELIDED_PREFIX) {
                return 0;
            }
            *text = format!(
                "[earlier argument ({chars} characters) removed to save context; the call ran \
                 with the full value]"
            );
            1
        }
        serde_json::Value::Array(items) => items
            .iter_mut()
            .map(|item| elide_long_strings(item, min_chars))
            .sum(),
        serde_json::Value::Object(fields) => fields
            .values_mut()
            .map(|field| elide_long_strings(field, min_chars))
            .sum(),
        _ => 0,
    }
}
