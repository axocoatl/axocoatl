//! 5-stage context compression pipeline.
//!
//! Progressive 5-stage compression strategy:
//! 1. Tool result budgeting — truncate oversized tool results
//! 2. History snipping — remove old conversation segments
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

/// Result of running the compression pipeline.
#[derive(Debug)]
pub struct CompressionResult {
    pub messages: Vec<ChatMessage>,
    pub stages_applied: Vec<String>,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Messages archived (moved to long-term memory) during Stage 4.
    pub archived_messages: Vec<ChatMessage>,
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
    pub fn compress_sync(&self, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
        let tokens_before = self.counter.count_messages(&messages);
        let threshold = (self.model_context_limit as f32 * COMPRESSION_TRIGGER_PCT) as usize;

        if tokens_before <= threshold {
            return messages;
        }

        let messages = self.stage1_tool_result_budget(messages);
        let current = self.counter.count_messages(&messages);
        if current <= threshold {
            return messages;
        }

        self.stage2_history_snip(messages)
    }

    /// Check if compression is needed based on current token count.
    pub fn needs_compression(&self, messages: &[ChatMessage]) -> bool {
        let current = self.counter.count_messages(messages);
        let threshold = (self.model_context_limit as f32 * COMPRESSION_TRIGGER_PCT) as usize;
        current > threshold
    }

    /// Run the full pipeline. Stages 1-2 always run; stages 3-5 only if summarizer is provided
    /// and token pressure remains after stages 1-2.
    pub async fn compress(
        &self,
        messages: Vec<ChatMessage>,
        summarizer: Option<&dyn Summarizer>,
        housekeeping_budget: usize,
    ) -> CompressionResult {
        let tokens_before = self.counter.count_messages(&messages);
        let threshold = (self.model_context_limit as f32 * COMPRESSION_TRIGGER_PCT) as usize;

        if tokens_before <= threshold {
            return CompressionResult {
                messages,
                stages_applied: vec![],
                tokens_before,
                tokens_after: tokens_before,
                archived_messages: Vec::new(),
            };
        }

        let mut stages_applied = Vec::new();
        let mut archived = Vec::new();

        // Stage 1: Tool result budgeting
        let messages = self.stage1_tool_result_budget(messages);
        stages_applied.push("tool_result_budget".to_string());

        let current = self.counter.count_messages(&messages);
        if current <= threshold {
            return CompressionResult {
                tokens_after: current,
                messages,
                stages_applied,
                tokens_before,
                archived_messages: archived,
            };
        }

        // Stage 2: History snipping
        let messages = self.stage2_history_snip(messages);
        stages_applied.push("history_snip".to_string());

        let current = self.counter.count_messages(&messages);
        if current <= threshold {
            return CompressionResult {
                tokens_after: current,
                messages,
                stages_applied,
                tokens_before,
                archived_messages: archived,
            };
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
                    .stage3_microcompact(messages, summarizer, remaining_budget)
                    .await;
                remaining_budget = remaining_budget.saturating_sub(used);
                stages_applied.push("microcompact".to_string());

                let current = self.counter.count_messages(&messages);
                if current <= threshold {
                    return CompressionResult {
                        tokens_after: current,
                        messages,
                        stages_applied,
                        tokens_before,
                        archived_messages: archived,
                    };
                }

                // Stage 4: Context collapse (archive old messages)
                let (messages, stage4_archived) = self.stage4_context_collapse(messages);
                archived = stage4_archived;
                stages_applied.push("context_collapse".to_string());

                let current = self.counter.count_messages(&messages);
                if current <= threshold {
                    return CompressionResult {
                        tokens_after: current,
                        messages,
                        stages_applied,
                        tokens_before,
                        archived_messages: archived,
                    };
                }

                // Stage 5: AutoCompact
                if current > MAX_INPUT_TOKENS && remaining_budget > 0 {
                    let messages = self
                        .stage5_autocompact(messages, summarizer, remaining_budget)
                        .await;
                    stages_applied.push("autocompact".to_string());
                    messages
                } else {
                    messages
                }
            }
        } else {
            messages
        };

        let tokens_after = self.counter.count_messages(&messages);
        CompressionResult {
            messages,
            stages_applied,
            tokens_before,
            tokens_after,
            archived_messages: archived,
        }
    }

    /// Stage 1: Truncate tool results exceeding per-message token limit.
    fn stage1_tool_result_budget(&self, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
        messages
            .into_iter()
            .map(|msg| {
                if msg.role != MessageRole::Tool {
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
                    role: msg.role,
                    content: MessageContent::Text(truncated),
                    name: msg.name,
                    // Preserve tool-call correlation across truncation so the
                    // assistant(tool_calls) ↔ tool(result) pairing stays valid.
                    tool_calls: msg.tool_calls,
                    tool_call_id: msg.tool_call_id,
                }
            })
            .collect()
    }

    /// Stage 2: Remove old conversation segments, keeping system + recent messages.
    /// Preserves message boundaries: never splits a tool result from its preceding assistant message.
    fn stage2_history_snip(&self, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
        let mut result = Vec::new();

        // Always keep system messages
        let mut system_msgs: Vec<ChatMessage> = messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .cloned()
            .collect();

        let non_system: Vec<ChatMessage> = messages
            .into_iter()
            .filter(|m| m.role != MessageRole::System)
            .collect();

        // Keep the last N messages, but adjust to not split tool results from their request.
        // Walk backward to find a safe cut point (start of a user message).
        let keep_count = SNIP_KEEP_RECENT_PAIRS * 3; // allow for user+assistant+tool triples
        let mut cut_point = non_system.len().saturating_sub(keep_count);

        // Adjust cut point forward to the start of a User message (safe boundary)
        while cut_point < non_system.len() && non_system[cut_point].role != MessageRole::User {
            cut_point += 1;
        }

        let kept = non_system[cut_point..].to_vec();

        result.append(&mut system_msgs);
        result.extend(kept);
        result
    }

    /// Stage 3: Microcompact — LLM-summarize individual oversized tool results.
    async fn stage3_microcompact(
        &self,
        messages: Vec<ChatMessage>,
        summarizer: &dyn Summarizer,
        budget: usize,
    ) -> (Vec<ChatMessage>, usize) {
        let mut result = Vec::with_capacity(messages.len());
        let mut tokens_used = 0;
        let mut compacted = 0;

        for msg in messages {
            if msg.role != MessageRole::Tool || tokens_used >= budget {
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
                        role: msg.role,
                        content: MessageContent::Text(format!("[summarized] {summary}")),
                        name: msg.name,
                        tool_calls: msg.tool_calls,
                        tool_call_id: msg.tool_call_id,
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
    ) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
        let total = messages.len();
        if total <= SNIP_KEEP_RECENT_PAIRS * 2 + 1 {
            // Too few messages to archive
            return (messages, Vec::new());
        }

        let mut system_msgs: Vec<ChatMessage> = Vec::new();
        let mut non_system: Vec<ChatMessage> = Vec::new();

        for msg in messages {
            if msg.role == MessageRole::System {
                system_msgs.push(msg);
            } else {
                non_system.push(msg);
            }
        }

        // Archive the first half of non-system messages
        let split_point = non_system.len() / 2;
        let archived: Vec<ChatMessage> = non_system.drain(..split_point).collect();

        // Add a summary marker where archived content was (as User, not System,
        // to avoid multi-system-message issues with some APIs)
        let mut result = system_msgs;
        result.push(ChatMessage::user(format!(
            "[Context note: {} earlier messages have been archived to long-term memory]",
            archived.len()
        )));
        result.extend(non_system);

        (result, archived)
    }

    /// Stage 5: AutoCompact — full conversation summarization.
    async fn stage5_autocompact(
        &self,
        messages: Vec<ChatMessage>,
        summarizer: &dyn Summarizer,
        _budget: usize,
    ) -> Vec<ChatMessage> {
        // Keep system messages and summarize everything else
        let mut system_msgs: Vec<ChatMessage> = messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .cloned()
            .collect();

        let to_summarize: Vec<ChatMessage> = messages
            .into_iter()
            .filter(|m| m.role != MessageRole::System)
            .collect();

        match summarizer.summarize_conversation(&to_summarize).await {
            Ok(summary) => {
                system_msgs.push(ChatMessage::system(format!(
                    "[AutoCompact summary of previous conversation]\n{summary}"
                )));
                system_msgs
            }
            Err(e) => {
                tracing::error!(error = %e, "AutoCompact failed, keeping original messages");
                system_msgs.extend(to_summarize);
                system_msgs
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::ApproximateCounter;

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

    #[test]
    fn stage1_truncates_large_tool_results() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let messages = vec![
            ChatMessage::user("hello"),
            make_long_tool_result(10_000), // Way over TOOL_RESULT_MAX_TOKENS
            ChatMessage::assistant("ok"),
        ];

        let compressed = pipeline.stage1_tool_result_budget(messages);
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

        let compressed = pipeline.stage1_tool_result_budget(messages);
        assert_eq!(compressed[1].text_content(), Some("small result"));
    }

    #[test]
    fn stage2_keeps_recent_messages() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("You are helpful.")];
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("msg {i}")));
            messages.push(ChatMessage::assistant(format!("resp {i}")));
        }

        let snipped = pipeline.stage2_history_snip(messages);
        // Should keep system + recent messages (cut at user boundary)
        assert!(
            snipped.len() > 1,
            "Should keep at least system + some messages"
        );
        assert!(snipped.len() <= 1 + SNIP_KEEP_RECENT_PAIRS * 3 + 1);
        assert_eq!(snipped[0].role, MessageRole::System);
        // First non-system message should be a User message (safe boundary)
        assert_eq!(snipped[1].role, MessageRole::User);
    }

    #[test]
    fn stage4_archives_old_messages() {
        let pipeline = CompressionPipeline::new(counter(), 100_000);
        let mut messages = vec![ChatMessage::system("sys")];
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("u{i}")));
            messages.push(ChatMessage::assistant(format!("a{i}")));
        }

        let (remaining, archived) = pipeline.stage4_context_collapse(messages);
        assert!(!archived.is_empty());
        // Remaining should have system + archive marker + recent messages
        assert!(remaining.iter().any(|m| {
            m.text_content()
                .map(|t| t.contains("archived"))
                .unwrap_or(false)
        }));
    }

    #[test]
    fn needs_compression_below_threshold() {
        let pipeline = CompressionPipeline::new(counter(), 200_000);
        let messages = vec![ChatMessage::user("hello"), ChatMessage::assistant("hi")];
        assert!(!pipeline.needs_compression(&messages));
    }

    #[tokio::test]
    async fn full_pipeline_stages_1_2_only() {
        let pipeline = CompressionPipeline::new(counter(), 50); // Very low limit
        let mut messages = vec![ChatMessage::system("sys")];
        for i in 0..30 {
            messages.push(ChatMessage::user(format!(
                "message number {i} with some filler"
            )));
            messages.push(ChatMessage::assistant(format!("response {i} with details")));
        }

        let result = pipeline.compress(messages, None, 0).await;
        assert!(!result.stages_applied.is_empty());
        assert!(result.tokens_after <= result.tokens_before);
    }
}
