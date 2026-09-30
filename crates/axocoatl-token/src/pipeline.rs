//! 5-stage context compression pipeline.
//!
//! Progressive 5-stage compression strategy:
//! 1. Tool result budgeting — truncate oversized tool results
//! 2. History snipping — elide tool output, then leave out tool rounds, then
//!    the oldest turns (never the most recent completed turn)
//! 3. Microcompact — LLM-summarize individual tool results (async)
//! 4. Context collapse — archive older sequences to DailyLogMemory (async)
//! 5. AutoCompact — full-turn summarization when >180K tokens (async)
//!
//! Stages 1-2 are pure computation (no LLM calls).
//! Stages 3-5 consume tokens for summarization and require an LLM provider.

use std::sync::Arc;

use axocoatl_core::{ChatMessage, MessageContent, MessageRole};

use crate::constants::*;
use crate::counter::TokenCounter;

const CONTEXT_NOTE_PREFIX: &str = "[Context note:";
const AUTOCOMPACT_PREFIX: &str = "[AutoCompact summary of previous conversation]";
/// Internal provenance marker for replaceable compression summaries. Request
/// builders clear it before provider serialization; configured/user System
/// text is never classified by its public prefix.
pub const SYNTHETIC_CONTEXT_MESSAGE_NAME: &str = "__axocoatl_compression_context_v1";

fn is_synthetic_context(message: &ChatMessage) -> bool {
    message.role == MessageRole::System
        && message.name.as_deref() == Some(SYNTHETIC_CONTEXT_MESSAGE_NAME)
}

fn is_authoritative_system(message: &ChatMessage) -> bool {
    message.role == MessageRole::System && !is_synthetic_context(message)
}

fn synthetic_context_message(content: impl Into<String>) -> ChatMessage {
    let mut message = ChatMessage::system(content);
    message.name = Some(SYNTHETIC_CONTEXT_MESSAGE_NAME.to_string());
    message
}

/// Result of running the compression pipeline.
#[derive(Debug)]
pub struct CompressionResult {
    pub messages: Vec<ChatMessage>,
    pub stages_applied: Vec<String>,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Start of the exact, unmodified active-turn suffix in `messages`.
    /// Callers must use this returned boundary after replacing their transcript,
    /// because Stages 2, 4, and 5 can change the length of the older prefix.
    pub protected_suffix_start: usize,
    /// Messages evicted from the live model context during Stage 4. This is a
    /// cache projection for optional memory tiers, not authoritative history;
    /// the caller-owned canonical Session ledger retains the exact transcript.
    pub archived_messages: Vec<ChatMessage>,
}

/// Request context that compression is not allowed to rewrite or split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionGuard {
    /// Index of the current ordinary User message. Everything from this index
    /// through the tail is one atomic provider transaction.
    pub protected_suffix_start: usize,
    /// Provider-visible tokens outside `messages`, principally tool
    /// definitions (and, for session-only compaction, the system prompt).
    pub fixed_tokens: usize,
}

impl CompressionGuard {
    pub const fn new(protected_suffix_start: usize, fixed_tokens: usize) -> Self {
        Self {
            protected_suffix_start,
            fixed_tokens,
        }
    }
}

/// A context that cannot be made safe for a provider request without changing
/// the current turn fails locally instead of sending an orphaned/oversized turn.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompressionError {
    #[error(
        "protected suffix starts at message {start}, beyond transcript length {message_count}"
    )]
    InvalidProtectedSuffix { start: usize, message_count: usize },

    #[error("protected suffix at message {start} does not begin with a User message")]
    ProtectedSuffixMustStartWithUser { start: usize },

    #[error(
        "current turn and required request context need {required_tokens} tokens, exceeding target {target_tokens}"
    )]
    ProtectedContextExceedsTarget {
        required_tokens: usize,
        target_tokens: usize,
    },

    #[error("compression left {remaining_tokens} tokens, exceeding target {target_tokens}")]
    UnableToReachTarget {
        remaining_tokens: usize,
        target_tokens: usize,
    },
}

/// Async summarizer trait — implemented by LLM providers for Stages 3-5.
#[async_trait::async_trait]
pub trait Summarizer: Send + Sync {
    /// Summarize a single tool result into a compact form.
    async fn summarize_tool_result(&self, tool_name: &str, result: &str) -> Result<String, String>;

    /// Summarize a sequence of messages into a compact summary.
    async fn summarize_conversation(&self, messages: &[ChatMessage]) -> Result<String, String>;
}

/// The 5-stage compression pipeline.
pub struct CompressionPipeline {
    counter: Arc<dyn TokenCounter>,
    model_context_limit: usize,
}

impl CompressionPipeline {
    pub fn new(counter: Arc<dyn TokenCounter>, model_context_limit: usize) -> Self {
        Self {
            counter,
            model_context_limit,
        }
    }

    /// Synchronous compression — stages 1-2 only (pure computation, no LLM calls).
    /// Safe to call from any context including single-threaded runtimes.
    pub fn compress_sync(
        &self,
        messages: Vec<ChatMessage>,
        guard: CompressionGuard,
    ) -> Result<CompressionResult, CompressionError> {
        let threshold = (self.model_context_limit as f32 * COMPRESSION_TRIGGER_PCT) as usize;
        self.compress_sync_to(messages, guard, threshold)
    }

    /// `compress_sync` against an explicit target (messages plus
    /// `guard.fixed_tokens`) instead of the trigger share of the window.
    pub fn compress_sync_to(
        &self,
        messages: Vec<ChatMessage>,
        guard: CompressionGuard,
        threshold: usize,
    ) -> Result<CompressionResult, CompressionError> {
        self.compress_sync_toward(messages, guard, threshold, threshold)
    }

    /// `compress_sync_to` against `threshold`, except that once the context
    /// must shrink it shrinks toward `aim` (at most `threshold`), so the
    /// requests that follow keep fitting with the same prefix instead of each
    /// giving up a little more. Only a result over `threshold` is an error.
    pub fn compress_sync_toward(
        &self,
        messages: Vec<ChatMessage>,
        guard: CompressionGuard,
        threshold: usize,
        aim: usize,
    ) -> Result<CompressionResult, CompressionError> {
        let aim = aim.min(threshold);
        self.validate_guard(&messages, guard, threshold)?;
        let tokens_before = self.total_tokens(&messages, guard.fixed_tokens);

        if tokens_before <= threshold {
            return Ok(CompressionResult {
                messages,
                stages_applied: Vec::new(),
                tokens_before,
                tokens_after: tokens_before,
                protected_suffix_start: guard.protected_suffix_start,
                archived_messages: Vec::new(),
            });
        }

        let messages = self.stage1_tool_result_budget(messages, guard.protected_suffix_start);
        let current = self.total_tokens(&messages, guard.fixed_tokens);
        if current <= aim {
            return Ok(CompressionResult {
                messages,
                stages_applied: vec!["tool_result_budget".to_string()],
                tokens_before,
                tokens_after: current,
                protected_suffix_start: guard.protected_suffix_start,
                archived_messages: Vec::new(),
            });
        }

        let (messages, protected_suffix_start) = self.stage2_history_snip(
            messages,
            guard.protected_suffix_start,
            guard.fixed_tokens,
            aim,
        );
        let tokens_after = self.total_tokens(&messages, guard.fixed_tokens);
        if tokens_after > threshold {
            return Err(CompressionError::UnableToReachTarget {
                remaining_tokens: tokens_after,
                target_tokens: threshold,
            });
        }
        Ok(CompressionResult {
            messages,
            stages_applied: vec!["tool_result_budget".to_string(), "history_snip".to_string()],
            tokens_before,
            tokens_after,
            protected_suffix_start,
            archived_messages: Vec::new(),
        })
    }

    /// Check if compression is needed based on current token count.
    pub fn needs_compression(&self, messages: &[ChatMessage], fixed_tokens: usize) -> bool {
        let current = self.total_tokens(messages, fixed_tokens);
        let threshold = (self.model_context_limit as f32 * COMPRESSION_TRIGGER_PCT) as usize;
        current > threshold
    }

    /// Validate the immutable request context before a caller performs any
    /// archive write or LLM-backed summarization side effect.
    pub fn validate_for_target(
        &self,
        messages: &[ChatMessage],
        guard: CompressionGuard,
        target_tokens: usize,
    ) -> Result<(), CompressionError> {
        self.validate_guard(messages, guard, target_tokens)
    }

    /// Run the full pipeline against the model context window. Stages 1-2 always
    /// run; stages 3-5 only if a summarizer is provided and token pressure remains.
    pub async fn compress(
        &self,
        messages: Vec<ChatMessage>,
        summarizer: Option<&dyn Summarizer>,
        housekeeping_budget: usize,
        guard: CompressionGuard,
    ) -> Result<CompressionResult, CompressionError> {
        let threshold = (self.model_context_limit as f32 * COMPRESSION_TRIGGER_PCT) as usize;
        self.compress_internal(
            messages,
            summarizer,
            housekeeping_budget,
            threshold,
            MAX_INPUT_TOKENS,
            guard,
        )
        .await
    }

    /// Run the full pipeline against an explicit `target_threshold` (e.g. a token
    /// budget's remaining headroom) rather than the model window. Stage 5
    /// (full-conversation summarization) fires relative to that target, so a
    /// sub-model-window target actually summarizes instead of only snipping.
    pub async fn compress_to(
        &self,
        messages: Vec<ChatMessage>,
        summarizer: Option<&dyn Summarizer>,
        housekeeping_budget: usize,
        target_threshold: usize,
        guard: CompressionGuard,
    ) -> Result<CompressionResult, CompressionError> {
        self.compress_internal(
            messages,
            summarizer,
            housekeeping_budget,
            target_threshold,
            target_threshold,
            guard,
        )
        .await
    }

    /// Shared pipeline core. `threshold` is the target to get under; `stage5_trigger`
    /// is the token count above which full-conversation summarization (Stage 5) runs.
    async fn compress_internal(
        &self,
        messages: Vec<ChatMessage>,
        summarizer: Option<&dyn Summarizer>,
        housekeeping_budget: usize,
        threshold: usize,
        stage5_trigger: usize,
        guard: CompressionGuard,
    ) -> Result<CompressionResult, CompressionError> {
        self.validate_guard(&messages, guard, threshold)?;
        let tokens_before = self.total_tokens(&messages, guard.fixed_tokens);
        let mut protected_suffix_start = guard.protected_suffix_start;

        if tokens_before <= threshold {
            return Ok(CompressionResult {
                messages,
                stages_applied: vec![],
                tokens_before,
                tokens_after: tokens_before,
                protected_suffix_start,
                archived_messages: Vec::new(),
            });
        }

        let mut stages_applied = Vec::new();
        let mut archived = Vec::new();

        // Stage 1: Tool result budgeting
        let messages = self.stage1_tool_result_budget(messages, protected_suffix_start);
        stages_applied.push("tool_result_budget".to_string());

        let current = self.total_tokens(&messages, guard.fixed_tokens);
        if current <= threshold {
            return Ok(CompressionResult {
                tokens_after: current,
                messages,
                stages_applied,
                tokens_before,
                protected_suffix_start,
                archived_messages: archived,
            });
        }

        // Stage 2: History snipping
        let (messages, new_protected_suffix_start) = self.stage2_history_snip(
            messages,
            protected_suffix_start,
            guard.fixed_tokens,
            threshold,
        );
        protected_suffix_start = new_protected_suffix_start;
        stages_applied.push("history_snip".to_string());

        let current = self.total_tokens(&messages, guard.fixed_tokens);
        if current <= threshold {
            return Ok(CompressionResult {
                tokens_after: current,
                messages,
                stages_applied,
                tokens_before,
                protected_suffix_start,
                archived_messages: archived,
            });
        }

        // Stages 3-5 require a summarizer and housekeeping budget
        let messages = if let Some(summarizer) = summarizer {
            if housekeeping_budget == 0 {
                tracing::warn!("No housekeeping budget for LLM-based compression stages");
                messages
            } else {
                let mut remaining_budget = housekeeping_budget;

                // Stage 3: Microcompact
                let (messages, used) = self
                    .stage3_microcompact(
                        messages,
                        protected_suffix_start,
                        summarizer,
                        remaining_budget,
                    )
                    .await;
                remaining_budget = remaining_budget.saturating_sub(used);
                stages_applied.push("microcompact".to_string());

                let current = self.total_tokens(&messages, guard.fixed_tokens);
                if current <= threshold {
                    return Ok(CompressionResult {
                        tokens_after: current,
                        messages,
                        stages_applied,
                        tokens_before,
                        protected_suffix_start,
                        archived_messages: archived,
                    });
                }

                // Stage 4: Context collapse (archive old messages)
                let (messages, stage4_archived, new_protected_suffix_start) =
                    self.stage4_context_collapse(messages, protected_suffix_start);
                protected_suffix_start = new_protected_suffix_start;
                archived = stage4_archived;
                stages_applied.push("context_collapse".to_string());

                let current = self.total_tokens(&messages, guard.fixed_tokens);
                if current <= threshold {
                    return Ok(CompressionResult {
                        tokens_after: current,
                        messages,
                        stages_applied,
                        tokens_before,
                        protected_suffix_start,
                        archived_messages: archived,
                    });
                }

                // Stage 5: AutoCompact (full-conversation summary), once token
                // pressure remains above the stage-5 trigger.
                if current > stage5_trigger && remaining_budget > 0 {
                    let (messages, new_protected_suffix_start) = self
                        .stage5_autocompact(
                            messages,
                            protected_suffix_start,
                            summarizer,
                            remaining_budget,
                        )
                        .await;
                    protected_suffix_start = new_protected_suffix_start;
                    stages_applied.push("autocompact".to_string());
                    messages
                } else {
                    messages
                }
            }
        } else {
            messages
        };

        let tokens_after = self.total_tokens(&messages, guard.fixed_tokens);
        if tokens_after > threshold {
            return Err(CompressionError::UnableToReachTarget {
                remaining_tokens: tokens_after,
                target_tokens: threshold,
            });
        }
        Ok(CompressionResult {
            messages,
            stages_applied,
            tokens_before,
            tokens_after,
            protected_suffix_start,
            archived_messages: archived,
        })
    }

    fn total_tokens(&self, messages: &[ChatMessage], fixed_tokens: usize) -> usize {
        self.counter
            .count_messages(messages)
            .saturating_add(fixed_tokens)
    }

    fn validate_guard(
        &self,
        messages: &[ChatMessage],
        guard: CompressionGuard,
        target_tokens: usize,
    ) -> Result<(), CompressionError> {
        if guard.protected_suffix_start > messages.len() {
            return Err(CompressionError::InvalidProtectedSuffix {
                start: guard.protected_suffix_start,
                message_count: messages.len(),
            });
        }
        if guard.protected_suffix_start < messages.len()
            && messages[guard.protected_suffix_start].role != MessageRole::User
        {
            return Err(CompressionError::ProtectedSuffixMustStartWithUser {
                start: guard.protected_suffix_start,
            });
        }

        // System messages and the active User-to-tail transaction are both
        // immutable. If those plus provider-visible fixed context cannot fit,
        // no compression stage can safely repair the request.
        let protected: Vec<ChatMessage> = messages
            .iter()
            .enumerate()
            .filter(|(index, message)| {
                is_authoritative_system(message) || *index >= guard.protected_suffix_start
            })
            .map(|(_, message)| message.clone())
            .collect();
        let required_tokens = self.total_tokens(&protected, guard.fixed_tokens);
        if required_tokens > target_tokens {
            return Err(CompressionError::ProtectedContextExceedsTarget {
                required_tokens,
                target_tokens,
            });
        }
        Ok(())
    }

    /// Stage 1: Truncate tool results exceeding per-message token limit.
    fn stage1_tool_result_budget(
        &self,
        messages: Vec<ChatMessage>,
        protected_suffix_start: usize,
    ) -> Vec<ChatMessage> {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, msg)| {
                if index >= protected_suffix_start || msg.role != MessageRole::Tool {
                    return msg;
                }
                let text = match &msg.content {
                    MessageContent::Text(s) => s,
                    _ => return msg,
                };
                let tokens = self.counter.count_text(text);
                if tokens <= TOOL_RESULT_MAX_TOKENS {
                    return msg;
                }
                // Truncate: keep ~TOOL_RESULT_MAX_TOKENS worth of chars
                // Rough: 4 chars per token
                let max_chars = TOOL_RESULT_MAX_TOKENS * 4;
                let truncated = if text.len() > max_chars {
                    // Safe UTF-8 truncation: find the last char boundary at or before max_chars
                    let safe_end = text
                        .char_indices()
                        .take_while(|(i, _)| *i < max_chars)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    format!(
                        "{}...\n[truncated: {} tokens → ~{} tokens]",
                        &text[..safe_end],
                        tokens,
                        TOOL_RESULT_MAX_TOKENS
                    )
                } else {
                    text.clone()
                };
                ChatMessage {
                    content: MessageContent::Text(truncated),
                    // Preserve every correlation and provider replay field as
                    // the message shape evolves.
                    ..msg
                }
            })
            .collect()
    }

    /// Stage 2: shrink the completed turns before the protected suffix until
    /// the request counts at most `threshold`, giving up the least useful
    /// context first:
    /// 1. long tool output and tool-call arguments, oldest turn first;
    /// 2. tool rounds, oldest turn first — every person's message and each
    ///    final answer stay, with a note of how many rounds were left out;
    /// 3. the oldest turns entirely, never the most recent completed one.
    ///
    /// A tool call always leaves with its results, and the active suffix is
    /// appended verbatim. The same transcript and target give the same result.
    fn stage2_history_snip(
        &self,
        messages: Vec<ChatMessage>,
        protected_suffix_start: usize,
        fixed_tokens: usize,
        threshold: usize,
    ) -> (Vec<ChatMessage>, usize) {
        let (older_prefix, active_suffix) = messages.split_at(protected_suffix_start);
        let mut result: Vec<ChatMessage> = older_prefix
            .iter()
            .filter(|message| is_authoritative_system(message))
            .cloned()
            .collect();
        let mut turns = completed_turns(
            older_prefix
                .iter()
                .filter(|message| !is_authoritative_system(message))
                .cloned(),
        );

        // Counts add up message by message, so each turn is measured once
        // and again only when it changes.
        let empty = self.counter.count_messages(&[]);
        let tokens =
            |messages: &[ChatMessage]| self.counter.count_messages(messages).saturating_sub(empty);
        let mut sizes: Vec<usize> = turns.iter().map(|turn| tokens(turn)).collect();
        let mut fixed = self
            .total_tokens(&result, fixed_tokens)
            .saturating_add(tokens(active_suffix));
        let over = |fixed: usize, sizes: &[usize]| {
            fixed.saturating_add(sizes.iter().sum::<usize>()) > threshold
        };

        let shrink_steps: [fn(&mut Vec<ChatMessage>); 2] =
            [|turn| elide_tool_values(turn), leave_out_rounds];
        for shrink in shrink_steps {
            for (index, turn) in turns.iter_mut().enumerate() {
                if !over(fixed, &sizes) {
                    break;
                }
                shrink(turn);
                sizes[index] = tokens(turn);
            }
        }

        // Last resort: the oldest turns go whole, with one note saying so.
        let note = synthetic_context_message(TURNS_LEFT_OUT_NOTE);
        let mut left_out = 0;
        let most_recent = turns.len().saturating_sub(1);
        for (index, turn) in turns.iter_mut().enumerate().take(most_recent) {
            if !over(fixed, &sizes) {
                break;
            }
            if left_out == 0 {
                fixed = fixed.saturating_add(tokens(std::slice::from_ref(&note)));
            }
            turn.clear();
            sizes[index] = 0;
            left_out += 1;
        }
        if left_out > 0 {
            result.push(note);
        }
        result.extend(turns.into_iter().flatten());
        let new_protected_suffix_start = result.len();
        result.extend_from_slice(active_suffix);
        (result, new_protected_suffix_start)
    }

    /// Stage 3: Microcompact — LLM-summarize individual oversized tool results.
    async fn stage3_microcompact(
        &self,
        messages: Vec<ChatMessage>,
        protected_suffix_start: usize,
        summarizer: &dyn Summarizer,
        budget: usize,
    ) -> (Vec<ChatMessage>, usize) {
        let mut result = Vec::with_capacity(messages.len());
        let mut tokens_used = 0;
        let mut compacted = 0;

        for (index, msg) in messages.into_iter().enumerate() {
            if index >= protected_suffix_start
                || msg.role != MessageRole::Tool
                || tokens_used >= budget
            {
                result.push(msg);
                continue;
            }

            let text = match &msg.content {
                MessageContent::Text(s) => s,
                _ => {
                    result.push(msg);
                    continue;
                }
            };

            let tokens = self.counter.count_text(text);
            if tokens <= TOOL_RESULT_MAX_TOKENS / 2 {
                result.push(msg);
                continue;
            }

            // Attempt LLM summarization
            let tool_name = msg.name.as_deref().unwrap_or("unknown");
            match summarizer.summarize_tool_result(tool_name, text).await {
                Ok(summary) => {
                    let summary_tokens = self.counter.count_text(&summary);
                    tokens_used += summary_tokens;
                    compacted += 1;
                    result.push(ChatMessage {
                        content: MessageContent::Text(format!("[summarized] {summary}")),
                        // Keep ids, calls, and opaque provider replay metadata
                        // intact when the textual result is summarized.
                        ..msg
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Microcompact summarization failed, keeping original");
                    result.push(msg);
                }
            }
        }

        if compacted > 0 {
            tracing::debug!(
                compacted,
                tokens_used,
                "Stage 3: microcompacted tool results"
            );
        }

        (result, tokens_used)
    }

    /// Stage 4: Context collapse — archive older conversation segments.
    /// Returns (remaining messages, archived messages).
    fn stage4_context_collapse(
        &self,
        messages: Vec<ChatMessage>,
        protected_suffix_start: usize,
    ) -> (Vec<ChatMessage>, Vec<ChatMessage>, usize) {
        let (older_prefix, active_suffix) = messages.split_at(protected_suffix_start);
        let system_msgs: Vec<ChatMessage> = older_prefix
            .iter()
            .filter(|message| is_authoritative_system(message))
            .cloned()
            .collect();
        let mut older_non_system: Vec<ChatMessage> = older_prefix
            .iter()
            .filter(|message| !is_authoritative_system(message))
            .cloned()
            .collect();

        if older_non_system.len() <= SNIP_KEEP_RECENT_PAIRS * 2 {
            return (messages, Vec::new(), protected_suffix_start);
        }

        // Split near the midpoint, but only at the start of a complete older
        // User segment. If no later User exists, archiving the entire completed
        // prefix is safe because the protected suffix itself begins with User.
        let mut split_point = older_non_system.len() / 2;
        while split_point < older_non_system.len()
            && older_non_system[split_point].role != MessageRole::User
        {
            split_point += 1;
        }
        let archived: Vec<ChatMessage> = older_non_system.drain(..split_point).collect();
        if archived.is_empty() {
            return (messages, Vec::new(), protected_suffix_start);
        }

        // Record the collapse as replaceable system context. Never synthesize a User
        // message immediately before the real current User: that would invent
        // a turn boundary and some providers reject consecutive user turns.
        let mut result = system_msgs;
        let note = format!(
            "{CONTEXT_NOTE_PREFIX} {} earlier messages were trimmed from the live context to save space]",
            archived.len()
        );
        result.push(synthetic_context_message(note));
        result.extend(older_non_system);
        let new_protected_suffix_start = result.len();
        result.extend_from_slice(active_suffix);

        (result, archived, new_protected_suffix_start)
    }

    /// Stage 5: AutoCompact — full conversation summarization.
    async fn stage5_autocompact(
        &self,
        messages: Vec<ChatMessage>,
        protected_suffix_start: usize,
        summarizer: &dyn Summarizer,
        _budget: usize,
    ) -> (Vec<ChatMessage>, usize) {
        let (older_prefix, active_suffix) = messages.split_at(protected_suffix_start);
        let mut system_msgs: Vec<ChatMessage> = older_prefix
            .iter()
            .filter(|message| is_authoritative_system(message))
            .cloned()
            .collect();

        let to_summarize: Vec<ChatMessage> = older_prefix
            .iter()
            .filter(|message| !is_authoritative_system(message))
            .cloned()
            .collect();
        if to_summarize.is_empty() {
            return (messages, protected_suffix_start);
        }

        match summarizer.summarize_conversation(&to_summarize).await {
            Ok(summary) => {
                system_msgs.push(synthetic_context_message(format!(
                    "{AUTOCOMPACT_PREFIX}\n{summary}"
                )));
                let new_protected_suffix_start = system_msgs.len();
                system_msgs.extend_from_slice(active_suffix);
                (system_msgs, new_protected_suffix_start)
            }
            Err(e) => {
                tracing::error!(error = %e, "AutoCompact failed, keeping original messages");
                (messages, protected_suffix_start)
            }
        }
    }
}

/// Stage 2 elides tool values of at least this many characters, as the
/// request-side masking's normal pass does: shorter output saves little and
/// is often the answer.
const STAGE2_ELIDE_MIN_CHARS: usize = 400;

/// Stands in for turns Stage 2 left out entirely. Without a count, so a
/// later compaction replaces it instead of adding another.
const TURNS_LEFT_OUT_NOTE: &str = "[Context note: earlier turns of this conversation were left \
     out to save space; the most recent requests and answers follow]";

/// Split completed history into turns. A turn starts at a person's message,
/// except guidance sent while the Agent was using tools (right after a tool
/// result), which stays with the turn it steered. Anything before the first
/// person's message (such as an earlier compaction's note) is its own unit.
fn completed_turns(messages: impl IntoIterator<Item = ChatMessage>) -> Vec<Vec<ChatMessage>> {
    let mut turns: Vec<Vec<ChatMessage>> = Vec::new();
    for message in messages {
        let starts_turn = match turns.last().and_then(|turn| turn.last()) {
            None => true,
            Some(previous) => {
                message.role == MessageRole::User && previous.role != MessageRole::Tool
            }
        };
        match turns.last_mut() {
            Some(turn) if !starts_turn => turn.push(message),
            _ => turns.push(vec![message]),
        }
    }
    turns
}

/// Stage 2, step 1: elide a turn's long tool output and call arguments.
fn elide_tool_values(turn: &mut [ChatMessage]) {
    for message in turn.iter_mut() {
        crate::elide_tool_output(message, STAGE2_ELIDE_MIN_CHARS);
        crate::elide_call_arguments(message, STAGE2_ELIDE_MIN_CHARS, |_| false);
    }
}

/// Stage 2, step 2: leave a turn's tool rounds out (each call with its
/// results), keeping every person's message and the Agent's answers. Each
/// answer says how many rounds before it were left out; rounds with no
/// answer after them (a person's guidance came next) get the note alone, so
/// person and Agent still take turns.
fn leave_out_rounds(turn: &mut Vec<ChatMessage>) {
    let mut kept = Vec::with_capacity(turn.len());
    let mut left_out = 0;
    for message in std::mem::take(turn) {
        match message.role {
            MessageRole::Assistant if !message.tool_calls.is_empty() => left_out += 1,
            MessageRole::Tool => {}
            MessageRole::Assistant => {
                kept.push(with_rounds_note(message, left_out));
                left_out = 0;
            }
            MessageRole::User | MessageRole::System => {
                if left_out > 0 {
                    kept.push(ChatMessage::assistant(rounds_note(left_out)));
                    left_out = 0;
                }
                kept.push(message);
            }
        }
    }
    if left_out > 0 {
        kept.push(ChatMessage::assistant(rounds_note(left_out)));
    }
    *turn = kept;
}

fn rounds_note(left_out: usize) -> String {
    let (rounds, were) = if left_out == 1 {
        ("round", "was")
    } else {
        ("rounds", "were")
    };
    format!(
        "[{left_out} earlier tool {rounds} for this request {were} left out to save context; \
         files changed are in the repository]"
    )
}

fn with_rounds_note(mut answer: ChatMessage, left_out: usize) -> ChatMessage {
    if left_out == 0 {
        return answer;
    }
    let note = rounds_note(left_out);
    answer.content = match answer.content {
        MessageContent::Text(text) if text.is_empty() => MessageContent::Text(note),
        MessageContent::Text(text) => MessageContent::Text(format!("{note}\n\n{text}")),
        MessageContent::Parts(mut parts) => {
            parts.insert(0, axocoatl_core::ContentPart::Text(note));
            MessageContent::Parts(parts)
        }
    };
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::ApproximateCounter;
    use axocoatl_core::{ProviderMetadata, ToolCall};
    use std::collections::BTreeMap;

    fn counter() -> Arc<dyn TokenCounter> {
        Arc::new(ApproximateCounter::new().unwrap())
    }

    fn make_long_tool_result(tokens: usize) -> ChatMessage {
        // ~4 chars per token
        let text = "x".repeat(tokens * 4);
        ChatMessage {
            role: MessageRole::Tool,
            content: MessageContent::Text(text),
            name: Some("big_tool".to_string()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn signature_call(id: &str, name: &str) -> ToolCall {
        let mut provider_metadata = ProviderMetadata::new();
        provider_metadata.insert(
            "gemini.thought_signature".to_string(),
            format!("signature-{id}"),
        );
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"id": id, "path": format!("/{id}")}),
            provider_metadata,
        }
    }

    fn tool_turn(user: &str, calls: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user(user)];
        let tool_calls: Vec<ToolCall> = (0..calls)
            .map(|index| signature_call(&format!("{user}-{index}"), "repo::read"))
            .collect();
        messages.push(ChatMessage::assistant_with_tool_calls(
            "",
            tool_calls.clone(),
        ));
        for call in tool_calls {
            messages.push(ChatMessage::tool_result(
                format!("result for {}", call.id),
                &call.name,
                &call.id,
            ));
        }
        messages.push(ChatMessage::assistant(format!("completed {user}")));
        messages
    }

    fn sequential_tool_turn(user: &str, calls: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user(user)];
        for index in 0..calls {
            let call = signature_call(&format!("{user}-{index}"), "repo::read");
            messages.push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![call.clone()],
            ));
            messages.push(ChatMessage::tool_result(
                format!("result for {}", call.id),
                &call.name,
                &call.id,
            ));
        }
        messages.push(ChatMessage::assistant(format!("completed {user}")));
        messages
    }

    fn serialized(messages: &[ChatMessage]) -> Vec<u8> {
        serde_json::to_vec(messages).unwrap()
    }

    fn assert_valid_tool_transactions(messages: &[ChatMessage]) {
        let mut pending: BTreeMap<String, String> = BTreeMap::new();
        for message in messages {
            match message.role {
                MessageRole::User => assert!(
                    pending.is_empty(),
                    "a User message split an assistant tool transaction"
                ),
                MessageRole::Assistant if !message.tool_calls.is_empty() => {
                    assert!(
                        pending.is_empty(),
                        "a new assistant tool turn started before prior results"
                    );
                    for call in &message.tool_calls {
                        assert!(
                            pending.insert(call.id.clone(), call.name.clone()).is_none(),
                            "duplicate tool-call id"
                        );
                    }
                }
                MessageRole::Tool => {
                    let id = message
                        .tool_call_id
                        .as_ref()
                        .expect("tool result must carry an id");
                    let expected_name = pending.remove(id).expect("orphaned tool result");
                    assert_eq!(message.name.as_deref(), Some(expected_name.as_str()));
                }
                _ => {}
            }
        }
        assert!(pending.is_empty(), "tool calls were left without results");
    }

    #[test]
    fn stage1_truncates_large_tool_results() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let messages = vec![
            ChatMessage::user("hello"),
            make_long_tool_result(10_000), // Way over TOOL_RESULT_MAX_TOKENS
            ChatMessage::assistant("ok"),
        ];

        let compressed = pipeline.stage1_tool_result_budget(messages, 3);
        assert_eq!(compressed.len(), 3);
        // The tool result should be truncated
        let tool_text = compressed[1].text_content().unwrap();
        assert!(tool_text.contains("truncated"));
    }

    #[test]
    fn stage1_leaves_small_results_alone() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let messages = vec![
            ChatMessage::user("hello"),
            ChatMessage::tool("small result"),
            ChatMessage::assistant("ok"),
        ];

        let compressed = pipeline.stage1_tool_result_budget(messages, 3);
        assert_eq!(compressed[1].text_content(), Some("small result"));
    }

    /// Was `stage2_keeps_recent_messages`, which kept a fixed fifteen
    /// messages whatever the target. Stage 2 now leaves out only as many of
    /// the oldest turns as the target needs, says so once, and never leaves
    /// out the most recent completed turn.
    #[test]
    fn stage2_leaves_out_the_oldest_turns_until_the_rest_fits() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("You are helpful.")];
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("msg {i}")));
            messages.push(ChatMessage::assistant(format!("resp {i}")));
        }
        let message_count = messages.len();
        let threshold = pipeline.total_tokens(&messages[..11], 0) + 20;

        let (snipped, start) =
            pipeline.stage2_history_snip(messages.clone(), message_count, 0, threshold);
        assert_eq!(start, snipped.len());
        assert!(pipeline.total_tokens(&snipped, 0) <= threshold);
        assert_eq!(snipped[0].role, MessageRole::System);
        assert!(is_synthetic_context(&snipped[1]));
        assert_eq!(snipped[1].text_content(), Some(TURNS_LEFT_OUT_NOTE));
        assert_eq!(snipped[2].role, MessageRole::User, "a turn starts whole");
        assert!(snipped.len() > 6, "only what the target needs goes");
        assert_eq!(snipped.last().unwrap().text_content(), Some("resp 19"));

        // Nothing fits: every turn but the most recent goes.
        let (snipped, _) = pipeline.stage2_history_snip(messages, message_count, 0, 0);
        let texts: Vec<_> = snipped
            .iter()
            .map(|message| message.text_content().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec!["You are helpful.", TURNS_LEFT_OUT_NOTE, "msg 19", "resp 19"]
        );
    }

    #[test]
    fn stage4_archives_old_messages() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("u{i}")));
            messages.push(ChatMessage::assistant(format!("a{i}")));
        }

        let message_count = messages.len();
        let (remaining, archived, _) = pipeline.stage4_context_collapse(messages, message_count);
        assert!(!archived.is_empty());
        // Remaining should have system + the trim marker + recent messages
        assert!(remaining.iter().any(|m| {
            m.text_content()
                .map(|t| t.contains("Context note") && t.contains("trimmed"))
                .unwrap_or(false)
        }));
    }

    #[test]
    fn needs_compression_below_threshold() {
        let pipeline = CompressionPipeline::new(counter(), 200_000);
        let messages = vec![ChatMessage::user("hello"), ChatMessage::assistant("hi")];
        assert!(!pipeline.needs_compression(&messages, 0));
    }

    #[test]
    fn stage1_never_truncates_an_active_tool_result() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::user("old")];
        let old_call = signature_call("old-call", "repo::read");
        messages.push(ChatMessage::assistant_with_tool_calls(
            "",
            vec![old_call.clone()],
        ));
        messages.push(ChatMessage::tool_result(
            "x".repeat(TOOL_RESULT_MAX_TOKENS * 32),
            &old_call.name,
            &old_call.id,
        ));
        let protected_suffix_start = messages.len();
        let active_call = signature_call("active-call", "repo::read");
        messages.push(ChatMessage::user("current"));
        messages.push(ChatMessage::assistant_with_tool_calls(
            "",
            vec![active_call.clone()],
        ));
        messages.push(ChatMessage::tool_result(
            "y".repeat(TOOL_RESULT_MAX_TOKENS * 32),
            &active_call.name,
            &active_call.id,
        ));
        let active_before = serialized(&messages[protected_suffix_start..]);

        let compressed = pipeline.stage1_tool_result_budget(messages, protected_suffix_start);

        assert!(compressed[2]
            .text_content()
            .is_some_and(|content| content.contains("truncated")));
        assert_eq!(
            serialized(&compressed[protected_suffix_start..]),
            active_before
        );
        assert_eq!(
            compressed[protected_suffix_start + 1].tool_calls[0].provider_metadata,
            active_call.provider_metadata
        );
    }

    #[test]
    fn stage2_preserves_more_than_twelve_sequential_active_messages() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        for index in 0..20 {
            messages.push(ChatMessage::user(format!("old user {index}")));
            messages.push(ChatMessage::assistant(format!("old answer {index}")));
        }
        let protected_suffix_start = messages.len();
        let active = sequential_tool_turn("current sequential", 7);
        assert!(active.len() > 12);
        let active_bytes = serialized(&active);
        messages.extend(active);

        let (snipped, new_start) =
            pipeline.stage2_history_snip(messages, protected_suffix_start, 0, 0);

        assert_eq!(serialized(&snipped[new_start..]), active_bytes);
        assert_eq!(snipped[new_start].role, MessageRole::User);
        assert_valid_tool_transactions(&snipped);
    }

    #[test]
    fn stage2_preserves_parallel_active_tool_results_and_metadata() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = Vec::new();
        for index in 0..20 {
            messages.extend(tool_turn(&format!("old-{index}"), 1));
        }
        let protected_suffix_start = messages.len();
        let active = tool_turn("current parallel", 12);
        assert!(active.len() > 12);
        let active_bytes = serialized(&active);
        messages.extend(active);

        let (snipped, new_start) =
            pipeline.stage2_history_snip(messages, protected_suffix_start, 0, 0);

        assert_eq!(serialized(&snipped[new_start..]), active_bytes);
        assert_valid_tool_transactions(&snipped);
    }

    #[tokio::test]
    async fn stage3_never_summarizes_active_tool_results() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let old_call = signature_call("old", "repo::read");
        let mut messages = vec![
            ChatMessage::user("old user"),
            ChatMessage::assistant_with_tool_calls("", vec![old_call.clone()]),
            ChatMessage::tool_result(
                "x".repeat(TOOL_RESULT_MAX_TOKENS * 32),
                &old_call.name,
                &old_call.id,
            ),
        ];
        let protected_suffix_start = messages.len();
        let active = sequential_tool_turn("current", 7);
        let active_bytes = serialized(&active);
        messages.extend(active);

        let (compacted, _) = pipeline
            .stage3_microcompact(messages, protected_suffix_start, &MockSummarizer, 10_000)
            .await;

        assert!(compacted[2]
            .text_content()
            .is_some_and(|content| content.contains("TOOL_SUMMARY")));
        assert_eq!(
            serialized(&compacted[protected_suffix_start..]),
            active_bytes
        );
        assert_eq!(
            compacted[1].tool_calls[0].provider_metadata,
            old_call.provider_metadata
        );
    }

    #[test]
    fn stage4_moves_an_arbitrary_half_split_to_a_user_boundary() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        for index in 0..7 {
            messages.extend(tool_turn(&format!("old-{index}"), 1));
        }
        let protected_suffix_start = messages.len();
        let active = tool_turn("current", 12);
        let active_bytes = serialized(&active);
        messages.extend(active);

        let (remaining, archived, new_start) =
            pipeline.stage4_context_collapse(messages, protected_suffix_start);

        assert!(!archived.is_empty());
        assert_eq!(archived[0].role, MessageRole::User);
        assert_valid_tool_transactions(&archived);
        assert_eq!(serialized(&remaining[new_start..]), active_bytes);
        assert_eq!(remaining[new_start].role, MessageRole::User);
        assert_ne!(remaining[new_start - 1].role, MessageRole::User);
        assert_valid_tool_transactions(&remaining);
    }

    struct RecordingSummarizer {
        conversations: std::sync::Mutex<Vec<Vec<ChatMessage>>>,
    }

    #[async_trait::async_trait]
    impl Summarizer for RecordingSummarizer {
        async fn summarize_tool_result(&self, _: &str, _: &str) -> Result<String, String> {
            Ok("tool summary".to_string())
        }

        async fn summarize_conversation(&self, messages: &[ChatMessage]) -> Result<String, String> {
            self.conversations.lock().unwrap().push(messages.to_vec());
            Ok("completed prefix summary".to_string())
        }
    }

    #[tokio::test]
    async fn stage5_summarizes_only_completed_prefix_and_keeps_exact_current_turn() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        messages.extend(tool_turn("old", 2));
        let protected_suffix_start = messages.len();
        let active = sequential_tool_turn("CURRENT_SENTINEL", 7);
        let active_bytes = serialized(&active);
        messages.extend(active);
        let summarizer = RecordingSummarizer {
            conversations: std::sync::Mutex::new(Vec::new()),
        };

        let (compacted, new_start) = pipeline
            .stage5_autocompact(messages, protected_suffix_start, &summarizer, 10_000)
            .await;

        assert_eq!(serialized(&compacted[new_start..]), active_bytes);
        assert_valid_tool_transactions(&compacted);
        let summarized = summarizer.conversations.lock().unwrap();
        assert!(summarized[0]
            .iter()
            .all(|message| message.text_content() != Some("CURRENT_SENTINEL")));
        assert!(serialized(&summarized[0])
            .windows("signature-old-0".len())
            .any(|window| window == b"signature-old-0"));
    }

    #[tokio::test]
    async fn successive_compactions_replace_synthetic_summary_instead_of_accumulating_it() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let summarizer = RecordingSummarizer {
            conversations: std::sync::Mutex::new(Vec::new()),
        };
        let mut messages = vec![ChatMessage::system("AUTHORITATIVE_SYSTEM")];
        messages.extend(tool_turn("completed-0", 2));

        for turn in 1..=3 {
            let protected_suffix_start = messages.len();
            let active = sequential_tool_turn(&format!("current-{turn}"), 7);
            let active_bytes = serialized(&active);
            messages.extend(active);

            let (compacted, new_start) = pipeline
                .stage5_autocompact(messages, protected_suffix_start, &summarizer, 10_000)
                .await;
            assert_eq!(serialized(&compacted[new_start..]), active_bytes);
            assert_eq!(
                compacted
                    .iter()
                    .filter(|message| is_synthetic_context(message))
                    .count(),
                1
            );
            assert_eq!(compacted[0].text_content(), Some("AUTHORITATIVE_SYSTEM"));
            assert_valid_tool_transactions(&compacted);
            messages = compacted;
        }
    }

    #[tokio::test]
    async fn public_summary_prefixes_do_not_make_authoritative_system_prompts_replaceable() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let summarizer = RecordingSummarizer {
            conversations: std::sync::Mutex::new(Vec::new()),
        };

        for configured_prompt in [
            format!("{CONTEXT_NOTE_PREFIX} configured authority]"),
            format!("{AUTOCOMPACT_PREFIX}\nconfigured authority"),
        ] {
            let mut messages = vec![ChatMessage::system(&configured_prompt)];
            messages.extend(tool_turn("completed", 1));
            let protected_suffix_start = messages.len();
            let active = tool_turn("current", 1);
            messages.extend(active);

            let (compacted, _) = pipeline
                .stage5_autocompact(messages, protected_suffix_start, &summarizer, 10_000)
                .await;

            assert_eq!(
                compacted[0].text_content(),
                Some(configured_prompt.as_str())
            );
            assert!(!is_synthetic_context(&compacted[0]));
            assert_eq!(
                compacted
                    .iter()
                    .filter(|message| is_synthetic_context(message))
                    .count(),
                1
            );
        }
    }

    struct FailingSummarizer;

    #[async_trait::async_trait]
    impl Summarizer for FailingSummarizer {
        async fn summarize_tool_result(&self, _: &str, _: &str) -> Result<String, String> {
            Err("no summary".to_string())
        }

        async fn summarize_conversation(&self, _: &[ChatMessage]) -> Result<String, String> {
            Err("no summary".to_string())
        }
    }

    #[tokio::test]
    async fn no_summarizer_summarizer_failure_and_insufficient_summary_never_return_over_target() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        for index in 0..20 {
            messages.extend(tool_turn(&format!("completed-{index}"), 1));
        }
        let protected_suffix_start = messages.len();
        messages.extend(tool_turn("current", 1));
        let guard = CompressionGuard::new(protected_suffix_start, 0);

        for result in [
            pipeline
                .compress_to(messages.clone(), None, 0, 30, guard)
                .await,
            pipeline
                .compress_to(
                    messages.clone(),
                    Some(&FailingSummarizer),
                    10_000,
                    30,
                    guard,
                )
                .await,
            pipeline
                .compress_to(messages.clone(), Some(&MockSummarizer), 10_000, 30, guard)
                .await,
        ] {
            assert!(matches!(
                result,
                Err(CompressionError::UnableToReachTarget { .. })
                    | Err(CompressionError::ProtectedContextExceedsTarget { .. })
            ));
        }

        let sync = CompressionPipeline::new(counter(), 36).compress_sync(messages, guard);
        assert!(sync.is_err());
    }

    #[tokio::test]
    async fn oversized_active_suffix_fails_without_rewriting_it() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let messages = vec![
            ChatMessage::user("x".repeat(8_000)),
            ChatMessage::assistant("tail"),
        ];
        let before = serialized(&messages);

        let error = pipeline
            .compress_to(
                messages.clone(),
                Some(&MockSummarizer),
                10_000,
                10,
                CompressionGuard::new(0, 0),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            CompressionError::ProtectedContextExceedsTarget { .. }
        ));
        assert_eq!(serialized(&messages), before);
    }

    #[tokio::test]
    async fn fixed_provider_context_participates_in_the_fail_closed_limit() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let messages = vec![ChatMessage::user("current")];

        let error = pipeline
            .compress_to(messages, None, 0, 25, CompressionGuard::new(0, 100))
            .await
            .unwrap_err();

        match error {
            CompressionError::ProtectedContextExceedsTarget {
                required_tokens,
                target_tokens,
            } => {
                assert!(required_tokens >= 100);
                assert_eq!(target_tokens, 25);
            }
            _ => panic!("unexpected compression error"),
        }
    }

    #[tokio::test]
    async fn full_pipeline_stages_1_2_only() {
        let pipeline = CompressionPipeline::new(counter(), 250); // Forces stages 1-2
        let mut messages = vec![ChatMessage::system("sys")];
        for i in 0..30 {
            messages.push(ChatMessage::user(format!(
                "message number {i} with some filler"
            )));
            messages.push(ChatMessage::assistant(format!("response {i} with details")));
        }

        let protected_suffix_start = messages.len() - 2;
        let result = pipeline
            .compress(
                messages,
                None,
                0,
                CompressionGuard::new(protected_suffix_start, 0),
            )
            .await
            .unwrap();
        assert!(!result.stages_applied.is_empty());
        assert!(result.tokens_after <= result.tokens_before);
    }

    /// `chars` characters of ordinary words (a tokenizer's cheap case).
    fn prose(chars: usize) -> String {
        "the file reads ".repeat(chars / 15 + 1)[..chars].to_string()
    }

    /// One completed turn: the person's request, `rounds` tool rounds that
    /// each read `output_chars` of a file, and the Agent's final answer.
    fn long_completed_turn(
        request: &str,
        answer: &str,
        rounds: usize,
        output_chars: usize,
    ) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user(request)];
        for index in 0..rounds {
            let id = format!("{request}-{index}");
            let call = ToolCall {
                id: id.clone(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": format!("src/file_{index}.rs")}),
                provider_metadata: ProviderMetadata::new(),
            };
            messages.push(ChatMessage::assistant_with_tool_calls("", vec![call]));
            messages.push(ChatMessage::tool_result(
                prose(output_chars),
                "read_file",
                &id,
            ));
        }
        messages.push(ChatMessage::assistant(answer));
        messages
    }

    fn has_text(messages: &[ChatMessage], role: MessageRole, needle: &str) -> bool {
        messages.iter().any(|message| {
            message.role == role
                && message
                    .text_content()
                    .is_some_and(|text| text.contains(needle))
        })
    }

    /// The 1.1.0 live eval: turn 1 of a native Session left 33,510 tokens of
    /// history (one request, 39 tool rounds, one answer). At the start of
    /// turn 2 on a 32k model, history snipping kept "recent messages" and then
    /// advanced to the next User boundary — there was none — so it dropped
    /// the whole turn and the lead started turn 2 with no memory of turn 1.
    #[test]
    fn a_long_turn_keeps_its_request_and_answer_when_history_must_shrink() {
        let pipeline = CompressionPipeline::new(counter(), 32_768);
        let mut messages = vec![ChatMessage::system("You are the lead.")];
        messages.extend(long_completed_turn(
            "TURN_ONE_REQUEST fix the parser",
            "TURN_ONE_ANSWER the parser is fixed in src/parser.rs",
            39,
            4_000,
        ));
        let protected_suffix_start = messages.len();
        messages.push(ChatMessage::user("TURN_TWO_REQUEST now add tests"));
        let threshold = (32_768.0 * COMPRESSION_TRIGGER_PCT) as usize;
        let before = pipeline.total_tokens(&messages, 0);
        assert!(before > threshold && before < 40_000, "{before}");

        let result = pipeline
            .compress_sync_to(
                messages,
                CompressionGuard::new(protected_suffix_start, 0),
                threshold,
            )
            .unwrap();

        assert!(result.tokens_after <= threshold);
        assert!(has_text(
            &result.messages,
            MessageRole::User,
            "TURN_ONE_REQUEST"
        ));
        assert!(has_text(
            &result.messages,
            MessageRole::Assistant,
            "TURN_ONE_ANSWER"
        ));
        assert_eq!(
            result.messages[result.protected_suffix_start].text_content(),
            Some("TURN_TWO_REQUEST now add tests")
        );
        assert_valid_tool_transactions(&result.messages);
    }

    /// However small the target, Stage 2 never leaves out the most recent
    /// completed turn's request or answer: it keeps them, with a note for
    /// the rounds it left out, and reports the target unmet (Stages 3-5 or
    /// the caller decide what happens next) rather than forget them.
    #[test]
    fn the_most_recent_completed_turn_is_always_kept() {
        let pipeline = CompressionPipeline::new(counter(), 32_768);
        let mut messages = vec![ChatMessage::system("sys")];
        for turn in 0..4 {
            messages.extend(long_completed_turn(
                &format!("REQUEST_{turn}"),
                &format!("ANSWER_{turn}"),
                6,
                2_000,
            ));
        }
        let protected_suffix_start = messages.len();
        messages.push(ChatMessage::user("CURRENT"));

        for threshold in [0, 200, 2_000, 8_000] {
            let (snipped, start) = pipeline.stage2_history_snip(
                messages.clone(),
                protected_suffix_start,
                0,
                threshold,
            );
            assert!(
                has_text(&snipped, MessageRole::User, "REQUEST_3"),
                "{threshold}"
            );
            assert!(
                has_text(&snipped, MessageRole::Assistant, "ANSWER_3"),
                "{threshold}"
            );
            assert_eq!(snipped[start].text_content(), Some("CURRENT"));
            assert_valid_tool_transactions(&snipped);
        }

        // Room for little more than the most recent turn: its rounds are
        // left out, its request and answer stay, and the older turns go.
        let (snipped, _) =
            pipeline.stage2_history_snip(messages.clone(), protected_suffix_start, 0, 0);
        let texts: Vec<_> = snipped
            .iter()
            .map(|message| message.text_content().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec![
                "sys",
                TURNS_LEFT_OUT_NOTE,
                "REQUEST_3",
                "[6 earlier tool rounds for this request were left out to save context; files \
                 changed are in the repository]\n\nANSWER_3",
                "CURRENT",
            ]
        );
        let smallest = pipeline.total_tokens(&snipped, 0);
        let guard = CompressionGuard::new(protected_suffix_start, 0);
        let error = pipeline
            .compress_sync_to(messages.clone(), guard, smallest - 1)
            .unwrap_err();
        assert!(matches!(
            error,
            CompressionError::UnableToReachTarget { .. }
        ));

        // Aiming lower than the limit shrinks as far as the aim needs, but
        // only a result over the limit fails.
        let aimed = pipeline
            .compress_sync_toward(messages, guard, smallest + 1_000, 0)
            .unwrap();
        assert_eq!(serialized(&aimed.messages), serialized(&snipped));
    }

    /// Stage 2 gives up the least useful context first and only as much as
    /// the target needs: eliding the oldest turns' tool output comes before
    /// eliding the most recent turn's, and eliding comes before leaving
    /// rounds out. The result is the same every time, so a compacted
    /// conversation stored in a checkpoint is reproducible.
    #[test]
    fn stage2_elides_before_leaving_rounds_out_and_is_deterministic() {
        let pipeline = CompressionPipeline::new(counter(), 32_768);
        let mut messages = vec![ChatMessage::system("sys")];
        messages.extend(long_completed_turn("REQUEST_0", "ANSWER_0", 6, 2_000));
        messages.extend(long_completed_turn("REQUEST_1", "ANSWER_1", 6, 2_000));
        let protected_suffix_start = messages.len();
        messages.push(ChatMessage::user("CURRENT"));
        let turn_one_start = 1 + 6 * 2 + 2;
        let whole = pipeline.total_tokens(&messages, 0);
        let turn_zero = pipeline.total_tokens(&messages[1..turn_one_start], 0);

        // Eliding turn 0's output is enough.
        let threshold = whole - turn_zero / 2;
        let (snipped, start) =
            pipeline.stage2_history_snip(messages.clone(), protected_suffix_start, 0, threshold);
        assert!(pipeline.total_tokens(&snipped, 0) <= threshold);
        assert_eq!(snipped.len(), messages.len(), "no round was left out");
        assert!(snipped[3]
            .text_content()
            .unwrap()
            .starts_with("[earlier read_file output (2000 characters)"));
        assert_eq!(snipped[3].tool_call_id.as_deref(), Some("REQUEST_0-0"));
        assert_eq!(
            snipped[turn_one_start + 2].text_content().unwrap().len(),
            2_000,
            "the most recent turn's output stays whole"
        );
        assert_eq!(snipped[start].text_content(), Some("CURRENT"));

        // Once every turn's output is elided, rounds go, oldest turn first.
        let elided = {
            let mut elided = messages.clone();
            elide_tool_values(&mut elided);
            pipeline.total_tokens(&elided, 0)
        };
        let (snipped, _) =
            pipeline.stage2_history_snip(messages.clone(), protected_suffix_start, 0, elided - 10);
        let has_call = |id: &str| {
            snipped
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id))
        };
        assert!(!has_call("REQUEST_0-0"));
        assert!(has_call("REQUEST_1-0"));
        assert!(has_text(&snipped, MessageRole::User, "REQUEST_0"));
        assert!(has_text(&snipped, MessageRole::Assistant, "ANSWER_0"));
        assert_valid_tool_transactions(&snipped);

        let again = pipeline.stage2_history_snip(messages, protected_suffix_start, 0, elided - 10);
        assert_eq!(serialized(&again.0), serialized(&snipped));
    }

    /// Guidance a person sent while the Agent was using tools belongs to the
    /// turn it steered: the most recent turn keeps its original request too,
    /// and leaving its rounds out keeps person and Agent taking turns.
    #[test]
    fn guidance_during_tool_use_stays_with_its_turn() {
        let pipeline = CompressionPipeline::new(counter(), 32_768);
        let mut messages = vec![ChatMessage::system("sys")];
        messages.extend(long_completed_turn("OLD_REQUEST", "OLD_ANSWER", 3, 2_000));
        let mut steered = long_completed_turn("LAST_REQUEST", "LAST_ANSWER", 4, 2_000);
        steered.insert(5, ChatMessage::user("GUIDANCE"));
        messages.extend(steered);
        let protected_suffix_start = messages.len();
        messages.push(ChatMessage::user("CURRENT"));

        let (snipped, _) = pipeline.stage2_history_snip(messages, protected_suffix_start, 0, 0);
        let texts: Vec<_> = snipped
            .iter()
            .map(|message| message.text_content().unwrap())
            .collect();
        assert_eq!(
            texts,
            vec![
                "sys",
                TURNS_LEFT_OUT_NOTE,
                "LAST_REQUEST",
                "[2 earlier tool rounds for this request were left out to save context; files \
                 changed are in the repository]",
                "GUIDANCE",
                "[2 earlier tool rounds for this request were left out to save context; files \
                 changed are in the repository]\n\nLAST_ANSWER",
                "CURRENT",
            ]
        );
    }

    struct MockSummarizer;
    #[async_trait::async_trait]
    impl Summarizer for MockSummarizer {
        async fn summarize_tool_result(&self, _: &str, _: &str) -> Result<String, String> {
            Ok("TOOL_SUMMARY".to_string())
        }
        async fn summarize_conversation(&self, _: &[ChatMessage]) -> Result<String, String> {
            Ok("CONVO_SUMMARY".to_string())
        }
    }

    #[tokio::test]
    async fn compress_to_runs_llm_summarization() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("question {i} with filler words")));
            messages.push(ChatMessage::assistant(format!("answer {i} with details")));
        }
        // Tiny target + housekeeping budget → the pipeline escalates to the LLM
        // autocompact stage (stage 5), which calls the summarizer. (With the
        // model-window `compress`, stage 5 wouldn't fire below 180k tokens.)
        let protected_suffix_start = messages.len() - 2;
        let result = pipeline
            .compress_to(
                messages,
                Some(&MockSummarizer),
                10_000,
                70,
                CompressionGuard::new(protected_suffix_start, 0),
            )
            .await
            .unwrap();
        assert!(result.stages_applied.contains(&"autocompact".to_string()));
        assert!(result.messages.iter().any(|m| m
            .text_content()
            .is_some_and(|t| t.contains("CONVO_SUMMARY"))));
        assert!(result.tokens_after < result.tokens_before);
    }
}
