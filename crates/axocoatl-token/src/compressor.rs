//! Context compression strategies based on AgentDiet (arXiv 2509.23586).
//! Reduces token usage when context window pressure is detected.

use std::sync::Arc;

use axocoatl_core::ChatMessage;

use crate::counter::TokenCounter;

/// Strategy for handling context window pressure.
#[derive(Debug, Clone)]
pub enum CompressionStrategy {
    /// Sliding window — keep N most recent messages.
    SlidingWindow { keep_last: usize },
    /// Recursive summarization — summarize old messages with LLM.
    /// `trigger_at_pct`: fraction of context limit that triggers summarization (e.g., 0.8).
    RecursiveSummary { trigger_at_pct: f32 },
    /// AgentDiet — classify and remove useless/redundant/expired messages.
    AgentDiet,
    /// No compression.
    None,
}

/// Statistics about a compression operation.
#[derive(Debug, Default)]
pub struct CompressionStats {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_saved: usize,
}

/// Compresses conversation context when approaching token limits.
pub struct ContextCompressor {
    strategy: CompressionStrategy,
    counter: Arc<dyn TokenCounter>,
    model_context_limit: usize,
}

impl ContextCompressor {
    pub fn new(
        strategy: CompressionStrategy,
        counter: Arc<dyn TokenCounter>,
        model_context_limit: usize,
    ) -> Self {
        Self {
            strategy,
            counter,
            model_context_limit,
        }
    }

    /// Compress messages if context window pressure is detected.
    /// Returns the (possibly compressed) messages and statistics.
    pub fn compress_if_needed(
        &self,
        messages: Vec<ChatMessage>,
    ) -> (Vec<ChatMessage>, CompressionStats) {
        let current_tokens = self.counter.count_messages(&messages);
        let threshold = (self.model_context_limit as f32 * 0.85) as usize;

        if current_tokens < threshold {
            let len = messages.len();
            return (
                messages,
                CompressionStats {
                    messages_before: len,
                    messages_after: len,
                    tokens_saved: 0,
                },
            );
        }

        match &self.strategy {
            CompressionStrategy::SlidingWindow { keep_last } => {
                let before = messages.len();
                let keep = messages.len().min(*keep_last);
                let compressed: Vec<ChatMessage> =
                    messages.into_iter().rev().take(keep).rev().collect();
                let after_tokens = self.counter.count_messages(&compressed);

                (
                    compressed,
                    CompressionStats {
                        messages_before: before,
                        messages_after: keep,
                        tokens_saved: current_tokens.saturating_sub(after_tokens),
                    },
                )
            }
            CompressionStrategy::RecursiveSummary { .. } => {
                // Would need an LLM call — return unchanged for now
                tracing::warn!(
                    "Recursive summarization not yet implemented, using sliding window fallback"
                );
                let before = messages.len();
                let keep = messages.len().min(20);
                let compressed: Vec<ChatMessage> =
                    messages.into_iter().rev().take(keep).rev().collect();
                let after_tokens = self.counter.count_messages(&compressed);
                (
                    compressed,
                    CompressionStats {
                        messages_before: before,
                        messages_after: keep,
                        tokens_saved: current_tokens.saturating_sub(after_tokens),
                    },
                )
            }
            CompressionStrategy::AgentDiet => {
                // Full AgentDiet would classify each message as Essential/Useful/Useless/Redundant/Expired
                // For now, use a simple heuristic: keep system + last N user/assistant pairs
                tracing::warn!("AgentDiet not yet fully implemented, using heuristic fallback");
                let before = messages.len();
                let mut compressed = Vec::new();

                // Keep all system messages
                for msg in &messages {
                    if msg.role == axocoatl_core::MessageRole::System {
                        compressed.push(msg.clone());
                    }
                }

                // Keep last 10 non-system messages
                let non_system: Vec<_> = messages
                    .into_iter()
                    .filter(|m| m.role != axocoatl_core::MessageRole::System)
                    .collect();
                let keep = non_system.len().min(10);
                compressed.extend(non_system.into_iter().rev().take(keep).rev());

                let after_tokens = self.counter.count_messages(&compressed);
                let after_count = compressed.len();
                (
                    compressed,
                    CompressionStats {
                        messages_before: before,
                        messages_after: after_count,
                        tokens_saved: current_tokens.saturating_sub(after_tokens),
                    },
                )
            }
            CompressionStrategy::None => {
                let len = messages.len();
                (
                    messages,
                    CompressionStats {
                        messages_before: len,
                        messages_after: len,
                        tokens_saved: 0,
                    },
                )
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

    fn make_messages(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    ChatMessage::user(format!(
                        "Message number {i} with some content to use tokens"
                    ))
                } else {
                    ChatMessage::assistant(format!(
                        "Response number {i} with detailed explanation text"
                    ))
                }
            })
            .collect()
    }

    #[test]
    fn no_compression_under_threshold() {
        let compressor = ContextCompressor::new(
            CompressionStrategy::SlidingWindow { keep_last: 5 },
            counter(),
            100_000, // Very high limit
        );

        let messages = make_messages(10);
        let (result, stats) = compressor.compress_if_needed(messages);
        assert_eq!(result.len(), 10); // No compression needed
        assert_eq!(stats.tokens_saved, 0);
    }

    #[test]
    fn sliding_window_compresses() {
        let compressor = ContextCompressor::new(
            CompressionStrategy::SlidingWindow { keep_last: 4 },
            counter(),
            50, // Very low limit to trigger compression
        );

        let messages = make_messages(20);
        let (result, stats) = compressor.compress_if_needed(messages);
        assert_eq!(result.len(), 4);
        assert!(stats.tokens_saved > 0);
        assert_eq!(stats.messages_before, 20);
        assert_eq!(stats.messages_after, 4);
    }

    #[test]
    fn no_strategy_passes_through() {
        let compressor = ContextCompressor::new(
            CompressionStrategy::None,
            counter(),
            50, // Low limit but no strategy
        );

        let messages = make_messages(20);
        let (result, stats) = compressor.compress_if_needed(messages);
        assert_eq!(result.len(), 20);
        assert_eq!(stats.tokens_saved, 0);
    }

    #[test]
    fn agent_diet_keeps_system_messages() {
        let compressor = ContextCompressor::new(
            CompressionStrategy::AgentDiet,
            counter(),
            50, // Low limit
        );

        let mut messages = vec![ChatMessage::system("You are an agent.")];
        messages.extend(make_messages(30));

        let (result, stats) = compressor.compress_if_needed(messages);
        // Should keep the system message + last 10 non-system
        assert!(result.len() <= 12); // 1 system + up to 10 non-system + tolerance
        assert!(result[0].text_content().unwrap().contains("agent"));
        assert!(stats.messages_before == 31);
    }
}
