//! Request-only masking of stale tool output.
//!
//! A long tool loop re-sends every earlier tool result on each round, so input
//! grows with the square of the rounds. When enabled, results older than the
//! last few tool-call rounds are replaced in the outgoing request by a short
//! placeholder. The session history is never changed; call ids stay intact so
//! providers still pair every result with its call.

use std::collections::BTreeSet;

use axocoatl_core::{ChatMessage, MessageContent, MessageRole};
use axocoatl_token::TokenCounter;

/// Results shorter than this stay as they are: masking them saves little and
/// loses the answer.
const MIN_MASKED_CHARS: usize = 400;
/// The tighter pass also elides shorter output; a placeholder is ~120.
const TIGHT_MASKED_CHARS: usize = 160;

#[derive(Debug, Clone)]
pub(crate) struct StaleToolResultMasking {
    keep_rounds: usize,
    exempt: BTreeSet<String>,
    /// Kept whole by the normal pass, like `exempt`, but masked by the tight
    /// pass: one such answer is worth keeping, many can overflow a small
    /// context.
    kept_until_tight: BTreeSet<String>,
}

impl StaleToolResultMasking {
    pub(crate) fn new(keep_rounds: usize, exempt: impl IntoIterator<Item = String>) -> Self {
        Self {
            keep_rounds: keep_rounds.max(1),
            exempt: exempt.into_iter().collect(),
            kept_until_tight: BTreeSet::new(),
        }
    }

    pub(crate) fn keep_until_tight(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.kept_until_tight.extend(names);
        self
    }

    /// Mask tool results that come before the last `keep_rounds` assistant
    /// tool-call rounds. Returns how many results were masked.
    pub(crate) fn apply(&self, messages: &mut [ChatMessage]) -> usize {
        let rounds = tool_rounds(messages);
        let stale = self.most_stale(rounds.len());
        self.mask_before(messages, &rounds, stale, MIN_MASKED_CHARS, false)
    }

    /// The normal pass's furthest cutoff. It moves in whole steps of
    /// `keep_rounds`, so the request prefix stays byte-identical for that many
    /// rounds and providers can reuse their prompt cache; between
    /// `keep_rounds` and twice that many latest rounds stay whole.
    fn most_stale(&self, rounds: usize) -> usize {
        let keep = self.keep_rounds;
        (rounds.saturating_sub(keep) / keep) * keep
    }

    /// The normal pass for a model with a known context: mask only as many of
    /// the oldest rounds as needed for `messages` to count at most `target`,
    /// so a small model keeps what it just read while there is room. The
    /// cutoff still moves in whole steps of `keep_rounds` (prefix-cache
    /// reuse) and never beyond `apply`'s. Returns how many values were masked.
    pub(crate) fn apply_by_pressure(
        &self,
        messages: &mut [ChatMessage],
        counter: &dyn TokenCounter,
        target: usize,
    ) -> usize {
        let rounds = tool_rounds(messages);
        let most = self.most_stale(rounds.len());
        let mut total = counter.count_messages(messages);
        let mut stale = 0;
        while stale < most && total > target {
            let next = stale + self.keep_rounds;
            let start = if stale == 0 { 0 } else { rounds[stale] };
            for message in &messages[start..rounds[next]] {
                let mut masked = message.clone();
                if self.mask_message(&mut masked, MIN_MASKED_CHARS, false) > 0 {
                    total = total
                        .saturating_sub(message_tokens(counter, message))
                        .saturating_add(message_tokens(counter, &masked));
                }
            }
            stale = next;
        }
        self.mask_before(messages, &rounds, stale, MIN_MASKED_CHARS, false)
    }

    /// For a request that would not fit even after `apply`: keep only the
    /// latest round whole and elide shorter output too. Cache reuse matters
    /// less than finishing the work.
    pub(crate) fn apply_tight(&self, messages: &mut [ChatMessage]) -> usize {
        let rounds = tool_rounds(messages);
        let stale = rounds.len().saturating_sub(1);
        self.mask_before(messages, &rounds, stale, TIGHT_MASKED_CHARS, true)
    }

    /// Last resort before a request would fail for its context: remove the
    /// Agent's oldest `count` tool rounds (each call with its results, so the
    /// conversation stays well formed; never the latest round) and say so in
    /// one line. A person's messages within the span are kept. Returns how
    /// many rounds were removed.
    pub(crate) fn drop_oldest_rounds(
        &self,
        messages: &mut Vec<ChatMessage>,
        count: usize,
    ) -> usize {
        let rounds = tool_rounds(messages);
        let dropped = count.min(rounds.len().saturating_sub(1));
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

    /// How many more of the oldest tool rounds `drop_oldest_rounds` must
    /// remove for `messages` to count at most `target`: every message a
    /// round carries counts, including the Agent's own text, which masking
    /// never shortens. Keeps at least the latest round.
    pub(crate) fn rounds_to_drop(
        &self,
        messages: &[ChatMessage],
        counter: &dyn TokenCounter,
        target: usize,
    ) -> usize {
        let rounds = tool_rounds(messages);
        if rounds.len() < 2 {
            return 0;
        }
        // The dropped-rounds note replaces what is removed.
        let note = message_tokens(
            counter,
            &ChatMessage::assistant(format!(
                "[{} of my earlier tool rounds in this task were removed to fit the context. \
                 Changes I made are still in the files; read again what I need.]",
                rounds.len()
            )),
        );
        let mut total = counter.count_messages(messages).saturating_add(note);
        for count in 1..rounds.len() {
            total = messages[rounds[count - 1]..rounds[count]]
                .iter()
                .filter(|message| message.role != MessageRole::User)
                .fold(total, |total, message| {
                    total.saturating_sub(message_tokens(counter, message))
                });
            if total <= target {
                return count;
            }
        }
        rounds.len() - 1
    }

    /// Mask everything before `rounds[stale]` at least `min_chars` long.
    fn mask_before(
        &self,
        messages: &mut [ChatMessage],
        rounds: &[usize],
        stale: usize,
        min_chars: usize,
        tight: bool,
    ) -> usize {
        if stale == 0 {
            return 0;
        }
        let cutoff = rounds[stale];
        messages[..cutoff]
            .iter_mut()
            .map(|message| self.mask_message(message, min_chars, tight))
            .sum()
    }

    /// Mask one message's tool output (or long call arguments) at least
    /// `min_chars` long. Returns how many values were masked.
    fn mask_message(&self, message: &mut ChatMessage, min_chars: usize, tight: bool) -> usize {
        let exempt = |name: &str| {
            self.exempt.contains(name) || (!tight && self.kept_until_tight.contains(name))
        };
        // The model's own earlier calls re-send their arguments too; a
        // whole-file write repeats the file on every round. Long string
        // arguments are elided the same way; the call's name, id and
        // argument keys stay, so the replay remains a well-formed call.
        // A provider that replays its own record of the turn (Anthropic
        // content blocks, Gemini parts) checks the calls against it, so a
        // message carrying such metadata keeps its arguments.
        if message.role == MessageRole::Assistant {
            let mut masked = 0;
            if message
                .tool_calls
                .iter()
                .all(|call| call.provider_metadata.is_empty())
            {
                for call in &mut message.tool_calls {
                    if !exempt(&call.name) {
                        masked += elide_long_strings(&mut call.arguments, min_chars);
                    }
                }
            }
            return masked;
        }
        if message.role != MessageRole::Tool {
            return 0;
        }
        let name = message.name.as_deref().unwrap_or("tool");
        if exempt(name) {
            return 0;
        }
        let MessageContent::Text(text) = &message.content else {
            return 0;
        };
        let chars = text.chars().count();
        if chars < min_chars || text.starts_with("[earlier ") {
            return 0;
        }
        message.content = MessageContent::Text(format!(
            "[earlier {name} output ({chars} characters) removed to save context; \
             run it again, narrower, if you still need it]"
        ));
        1
    }
}

/// One message's share of a request count.
fn message_tokens(counter: &dyn TokenCounter, message: &ChatMessage) -> usize {
    counter
        .count_messages(std::slice::from_ref(message))
        .saturating_sub(counter.count_messages(&[]))
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
    fn results_kept_until_tight_survive_the_normal_pass_only() {
        let long = "d".repeat(MIN_MASKED_CHARS * 2);
        let mut messages = vec![ChatMessage::user("u")];
        for id in ["a", "b", "c", "d"] {
            messages.extend(round(id, "delegate", &long));
        }
        messages.extend(round("e", "workspace_knowledge", &long));
        messages.extend(round("f", "read_file", "short"));
        let masking = StaleToolResultMasking::new(2, ["workspace_knowledge".to_string()])
            .keep_until_tight(["delegate".to_string()]);
        assert_eq!(
            masking.apply(&mut messages),
            0,
            "the normal pass keeps every helper answer"
        );
        assert!((0..4).all(|index| text(&messages[2 + 2 * index]) == long));
        assert_eq!(masking.apply_tight(&mut messages), 4);
        assert!((0..4).all(|index| text(&messages[2 + 2 * index])
            .starts_with("[earlier delegate output (800 characters)")));
        assert_eq!(text(&messages[10]), long, "exempt tools stay whole");
        assert_eq!(masking.drop_oldest_rounds(&mut messages, 4), 4);
        assert!(messages
            .iter()
            .all(|message| message.tool_call_id.as_deref() != Some("a")));
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
        assert_eq!(masking.drop_oldest_rounds(&mut messages, 2), 2);
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
        // The latest round always stays.
        assert_eq!(masking.drop_oldest_rounds(&mut messages, 9), 1);
        assert_eq!(masking.drop_oldest_rounds(&mut messages, 9), 0);
        assert_eq!(messages.last().unwrap().tool_call_id.as_deref(), Some("d"));
    }

    /// A quarter token per character plus a small per-message overhead, with
    /// tool calls counted like the real counter.
    struct QuarterCounter;
    impl TokenCounter for QuarterCounter {
        fn count_text(&self, text: &str) -> usize {
            text.chars().count() / 4
        }
        fn count_messages(&self, messages: &[ChatMessage]) -> usize {
            3 + messages
                .iter()
                .map(|message| {
                    4 + self.count_text(message.text_content().unwrap_or(""))
                        + if message.tool_calls.is_empty() {
                            0
                        } else {
                            self.count_text(&serde_json::to_string(&message.tool_calls).unwrap())
                        }
                })
                .sum::<usize>()
        }
        fn count_tool_definition(&self, value: &serde_json::Value) -> usize {
            self.count_text(&value.to_string())
        }
    }

    #[test]
    fn the_pressure_pass_masks_only_what_the_target_needs_in_whole_steps() {
        let long = "x".repeat(4_000);
        let messages_after = |rounds: usize| {
            let mut messages = vec![ChatMessage::user("u")];
            for index in 0..rounds {
                messages.extend(round(&format!("c{index}"), "read_file", &long));
            }
            messages
        };
        let masking = StaleToolResultMasking::new(3, []);
        let count = |messages: &[ChatMessage]| QuarterCounter.count_messages(messages);

        // Room for everything: nothing is masked, unlike the normal pass.
        let mut roomy = messages_after(9);
        assert_eq!(
            masking.apply_by_pressure(&mut roomy, &QuarterCounter, 100_000),
            0
        );
        assert_eq!(masking.apply(&mut messages_after(9)), 6);

        // Under pressure it masks the fewest whole steps that fit.
        let mut pressed = messages_after(9);
        let target = count(&pressed) - 2_000;
        assert_eq!(
            masking.apply_by_pressure(&mut pressed, &QuarterCounter, target),
            3
        );
        assert!(count(&pressed) <= target);

        // Never further than the normal pass, even when nothing fits.
        let mut tight = messages_after(9);
        assert_eq!(
            masking.apply_by_pressure(&mut tight, &QuarterCounter, 10),
            6
        );

        // With a fixed target the cutoff only moves forward, in steps of 3.
        let target = 9_000;
        let mut last = 0;
        for rounds in 1..=20 {
            let mut messages = messages_after(rounds);
            let masked = masking.apply_by_pressure(&mut messages, &QuarterCounter, target);
            assert_eq!(masked % 3, 0, "{rounds} rounds");
            assert!(masked >= last, "{rounds} rounds");
            last = masked;
        }
    }

    #[test]
    fn rounds_to_drop_counts_the_agent_s_own_text_and_keeps_the_latest_round() {
        let text = "t".repeat(4_000);
        let mut messages = vec![ChatMessage::system("s"), ChatMessage::user("task")];
        for index in 0..6 {
            let [mut call, result] = round(&format!("c{index}"), "read_file", "short");
            call.content = MessageContent::Text(text.clone());
            messages.extend([call, result]);
        }
        let masking = StaleToolResultMasking::new(3, []);
        // Masking cannot shorten the Agent's text; each round is ~1,000.
        let total = QuarterCounter.count_messages(&messages);
        assert_eq!(masking.apply_tight(&mut messages.clone()), 0);
        assert_eq!(masking.rounds_to_drop(&messages, &QuarterCounter, total), 1);
        assert_eq!(
            masking.rounds_to_drop(&messages, &QuarterCounter, total - 2_500),
            3
        );
        assert_eq!(masking.rounds_to_drop(&messages, &QuarterCounter, 0), 5);
        let mut dropped = messages.clone();
        masking.drop_oldest_rounds(&mut dropped, 3);
        assert!(QuarterCounter.count_messages(&dropped) <= total - 2_500);
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
