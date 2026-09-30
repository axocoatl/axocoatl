//! Request-only masking of stale tool output.
//!
//! A long tool loop re-sends every earlier tool result on each round, so input
//! grows with the square of the rounds. When enabled, results older than the
//! last few tool-call rounds are replaced in the outgoing request by a short
//! placeholder. The session history is never changed; call ids stay intact so
//! providers still pair every result with its call.

use std::collections::BTreeSet;

use axocoatl_core::{ChatMessage, MessageContent, MessageRole};

/// Results shorter than this stay as they are: masking them saves little and
/// loses the answer.
const MIN_MASKED_CHARS: usize = 400;
/// The tighter pass also elides shorter output; a placeholder is ~120.
const TIGHT_MASKED_CHARS: usize = 160;

#[derive(Debug, Clone)]
pub(crate) struct StaleToolResultMasking {
    keep_rounds: usize,
    exempt: BTreeSet<String>,
}

impl StaleToolResultMasking {
    pub(crate) fn new(keep_rounds: usize, exempt: impl IntoIterator<Item = String>) -> Self {
        Self {
            keep_rounds: keep_rounds.max(1),
            exempt: exempt.into_iter().collect(),
        }
    }

    /// Mask tool results that come before the last `keep_rounds` assistant
    /// tool-call rounds. Returns how many results were masked.
    pub(crate) fn apply(&self, messages: &mut [ChatMessage]) -> usize {
        // The cutoff moves in whole steps of `keep_rounds`, so the request
        // prefix stays byte-identical for that many rounds and providers can
        // reuse their prompt cache; between `keep_rounds` and twice that many
        // latest rounds stay whole.
        let rounds = tool_rounds(messages);
        let keep = self.keep_rounds;
        let stale = (rounds.len().saturating_sub(keep) / keep) * keep;
        self.mask_before(messages, &rounds, stale, MIN_MASKED_CHARS)
    }

    /// For a request that would not fit even after `apply`: keep only the
    /// latest round whole and elide shorter output too. Cache reuse matters
    /// less than finishing the work.
    pub(crate) fn apply_tight(&self, messages: &mut [ChatMessage]) -> usize {
        let rounds = tool_rounds(messages);
        let stale = rounds.len().saturating_sub(1);
        self.mask_before(messages, &rounds, stale, TIGHT_MASKED_CHARS)
    }

    /// Last resort before a request would fail for its context: remove the
    /// Agent's own earlier tool rounds (each call with its results, so the
    /// conversation stays well formed) except the latest `keep` rounds, and
    /// say so in one line. A person's messages within the turn are kept.
    /// Returns how many rounds were removed.
    pub(crate) fn drop_stale_rounds(&self, messages: &mut Vec<ChatMessage>, keep: usize) -> usize {
        let rounds = tool_rounds(messages);
        let dropped = rounds.len().saturating_sub(keep.max(1));
        if dropped == 0 {
            return 0;
        }
        let (start, end) = (rounds[0], rounds[dropped]);
        let mut kept = Vec::with_capacity(messages.len());
        for (index, message) in std::mem::take(messages).into_iter().enumerate() {
            let in_span = (start..end).contains(&index);
            if index == start {
                kept.push(ChatMessage::assistant(format!(
                    "[{dropped} of my earlier tool rounds in this task were removed to fit the \
                     context. Changes I made are still in the files; read again what I need.]"
                )));
            }
            if !in_span || message.role == MessageRole::User {
                kept.push(message);
            }
        }
        *messages = kept;
        dropped
    }

    /// Mask everything before `rounds[stale]` at least `min_chars` long.
    fn mask_before(
        &self,
        messages: &mut [ChatMessage],
        rounds: &[usize],
        stale: usize,
        min_chars: usize,
    ) -> usize {
        if stale == 0 {
            return 0;
        }
        let cutoff = rounds[stale];
        let mut masked = 0;
        for message in &mut messages[..cutoff] {
            // The model's own earlier calls re-send their arguments too; a
            // whole-file write repeats the file on every round. Long string
            // arguments are elided the same way; the call's name, id and
            // argument keys stay, so the replay remains a well-formed call.
            // A provider that replays its own record of the turn (Anthropic
            // content blocks, Gemini parts) checks the calls against it, so a
            // message carrying such metadata keeps its arguments.
            if message.role == MessageRole::Assistant {
                if message
                    .tool_calls
                    .iter()
                    .all(|call| call.provider_metadata.is_empty())
                {
                    for call in &mut message.tool_calls {
                        if !self.exempt.contains(&call.name) {
                            masked += elide_long_strings(&mut call.arguments, min_chars);
                        }
                    }
                }
                continue;
            }
            if message.role != MessageRole::Tool {
                continue;
            }
            let name = message.name.as_deref().unwrap_or("tool");
            if self.exempt.contains(name) {
                continue;
            }
            let MessageContent::Text(text) = &message.content else {
                continue;
            };
            let chars = text.chars().count();
            if chars < min_chars || text.starts_with("[earlier ") {
                continue;
            }
            message.content = MessageContent::Text(format!(
                "[earlier {name} output ({chars} characters) removed to save context; \
                 run it again, narrower, if you still need it]"
            ));
            masked += 1;
        }
        masked
    }
}

/// The assistant tool-call rounds, by message index.
fn tool_rounds(messages: &[ChatMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == MessageRole::Assistant && !message.tool_calls.is_empty()
        })
        .map(|(index, _)| index)
        .collect()
}

/// Replace string values of at least `min_chars` characters, at any depth,
/// with a placeholder. Returns how many were replaced.
fn elide_long_strings(value: &mut serde_json::Value, min_chars: usize) -> usize {
    match value {
        serde_json::Value::String(text) => {
            let chars = text.chars().count();
            if chars < min_chars || text.starts_with("[earlier ") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::ToolCall;

    fn round(id: &str, name: &str, output: &str) -> [ChatMessage; 2] {
        let call = ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
            provider_metadata: Default::default(),
        };
        let mut result = ChatMessage::tool(output);
        result.name = Some(name.into());
        result.tool_call_id = Some(id.into());
        [
            ChatMessage::assistant_with_tool_calls("", vec![call]),
            result,
        ]
    }

    fn text(message: &ChatMessage) -> &str {
        match &message.content {
            MessageContent::Text(text) => text,
            MessageContent::Parts(_) => "",
        }
    }

    #[test]
    fn only_results_before_the_kept_rounds_are_masked() {
        let long = "x".repeat(MIN_MASKED_CHARS);
        let mut messages = vec![ChatMessage::system("s"), ChatMessage::user("u")];
        messages.extend(round("a", "read_file", &long));
        messages.extend(round("b", "workspace_knowledge", &long));
        messages.extend(round("c", "bash", "short"));
        messages.extend(round("d", "read_file", &long));
        messages.extend(round("e", "read_file", &long));

        let masked = StaleToolResultMasking::new(2, ["workspace_knowledge".to_string()])
            .apply(&mut messages);

        assert_eq!(masked, 1);
        assert!(text(&messages[3]).starts_with("[earlier read_file output (400 characters)"));
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("a"));
        assert_eq!(text(&messages[5]), long, "exempt tools keep their output");
        assert_eq!(text(&messages[7]), "short", "short results are kept");
        assert_eq!(text(&messages[9]), long);
        assert_eq!(text(&messages[11]), long);
    }

    #[test]
    fn the_masked_prefix_changes_only_every_kept_rounds() {
        let long = "x".repeat(MIN_MASKED_CHARS);
        let masked_after = |rounds: usize| {
            let mut messages = vec![ChatMessage::user("u")];
            for index in 0..rounds {
                messages.extend(round(&format!("c{index}"), "read_file", &long));
            }
            StaleToolResultMasking::new(3, []).apply(&mut messages)
        };
        // Up to 5 rounds nothing is masked; rounds 6 to 8 mask the same first
        // three, so those requests share their prefix; round 9 moves on.
        assert_eq!(
            (1..=9).map(masked_after).collect::<Vec<_>>(),
            vec![0, 0, 0, 0, 0, 3, 3, 3, 6]
        );
    }

    #[test]
    fn stale_long_arguments_are_elided_but_the_call_stays_well_formed() {
        let file = "y".repeat(MIN_MASKED_CHARS * 3);
        let write = |id: &str, name: &str| {
            let call = ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: serde_json::json!({"path": "lib/a.js", "content": file, "edits": [{"new": file}]}),
                provider_metadata: Default::default(),
            };
            let mut result = ChatMessage::tool("ok");
            result.name = Some(name.into());
            result.tool_call_id = Some(id.into());
            [
                ChatMessage::assistant_with_tool_calls("", vec![call]),
                result,
            ]
        };
        let mut messages = vec![ChatMessage::user("u")];
        messages.extend(write("a", "write_file"));
        messages.extend(write("b", "workspace_knowledge"));
        messages.extend(write("c", "write_file"));
        messages.extend(write("d", "write_file"));

        let masked = StaleToolResultMasking::new(2, ["workspace_knowledge".to_string()])
            .apply(&mut messages);

        assert_eq!(masked, 2, "both long strings of the stale write");
        let stale = &messages[1].tool_calls[0];
        assert_eq!(stale.id, "a");
        assert_eq!(stale.arguments["path"], "lib/a.js", "short arguments stay");
        assert!(stale.arguments["content"]
            .as_str()
            .unwrap()
            .starts_with("[earlier argument (1200 characters)"));
        assert!(stale.arguments["edits"][0]["new"]
            .as_str()
            .unwrap()
            .starts_with("[earlier"));
        assert_eq!(
            messages[3].tool_calls[0].arguments["content"], file,
            "exempt tools keep theirs"
        );
        assert_eq!(
            messages[5].tool_calls[0].arguments["content"], file,
            "kept rounds are whole"
        );

        // A provider replay record pins the call's arguments.
        let mut replayed = vec![ChatMessage::user("u")];
        replayed.extend(write("a", "write_file"));
        replayed[1].tool_calls[0]
            .provider_metadata
            .insert("anthropic.assistant_content_blocks".into(), "[]".into());
        replayed.extend(write("c", "write_file"));
        replayed.extend(write("d", "write_file"));
        StaleToolResultMasking::new(2, []).apply(&mut replayed);
        assert_eq!(replayed[1].tool_calls[0].arguments["content"], file);
    }

    #[test]
    fn the_tight_pass_keeps_only_the_latest_round_and_shorter_output() {
        let medium = "m".repeat(TIGHT_MASKED_CHARS);
        let mut messages = vec![ChatMessage::user("u")];
        for id in ["a", "b", "c"] {
            messages.extend(round(id, "read_file", &medium));
        }
        let masking = StaleToolResultMasking::new(3, []);
        assert_eq!(
            masking.apply(&mut messages),
            0,
            "the normal pass keeps these"
        );
        assert_eq!(masking.apply_tight(&mut messages), 2);
        assert!(text(&messages[2]).starts_with("[earlier read_file output"));
        assert!(text(&messages[4]).starts_with("[earlier read_file output"));
        assert_eq!(text(&messages[6]), medium, "the latest round stays whole");
        assert_eq!(
            masking.apply_tight(&mut messages),
            0,
            "placeholders are not masked again"
        );
    }

    #[test]
    fn dropping_stale_rounds_keeps_pairs_whole_and_a_person_s_messages() {
        let mut messages = vec![ChatMessage::system("s"), ChatMessage::user("task")];
        messages.extend(round("a", "read_file", "one"));
        messages.push(ChatMessage::user("guidance from a person"));
        messages.extend(round("b", "read_file", "two"));
        messages.extend(round("c", "read_file", "three"));
        messages.extend(round("d", "read_file", "four"));
        let masking = StaleToolResultMasking::new(3, []);
        assert_eq!(masking.drop_stale_rounds(&mut messages, 2), 2);
        let roles: Vec<_> = messages
            .iter()
            .map(|message| message.role.clone())
            .collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::System,
                MessageRole::User,
                MessageRole::Assistant, // the note
                MessageRole::User,      // the person's guidance stays
                MessageRole::Assistant,
                MessageRole::Tool,
                MessageRole::Assistant,
                MessageRole::Tool,
            ]
        );
        assert!(text(&messages[2]).starts_with("[2 of my earlier tool rounds"));
        assert_eq!(messages[4].tool_calls[0].id, "c");
        assert_eq!(messages[5].tool_call_id.as_deref(), Some("c"));
        assert_eq!(masking.drop_stale_rounds(&mut messages, 2), 0);
    }

    #[test]
    fn nothing_is_masked_until_there_are_more_rounds_than_kept() {
        let long = "x".repeat(MIN_MASKED_CHARS * 2);
        let mut messages = vec![ChatMessage::user("u")];
        messages.extend(round("a", "read_file", &long));
        messages.extend(round("b", "read_file", &long));

        assert_eq!(StaleToolResultMasking::new(2, []).apply(&mut messages), 0);
        assert_eq!(text(&messages[2]), long);
    }
}
