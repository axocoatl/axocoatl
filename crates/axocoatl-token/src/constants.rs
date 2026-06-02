//! Token budget constants for compression pipeline stages.

/// Maximum input tokens before triggering AutoCompact (Stage 5).
pub const MAX_INPUT_TOKENS: usize = 180_000;

/// Target token count after full compaction.
pub const TARGET_AFTER_COMPACTION: usize = 40_000;

/// Maximum tokens for a single tool result before truncation (Stage 1).
pub const TOOL_RESULT_MAX_TOKENS: usize = 4_000;

/// Trigger compression when session tokens exceed this fraction of model context limit.
pub const COMPRESSION_TRIGGER_PCT: f32 = 0.85;

/// Fraction of per_execution budget reserved for housekeeping (Stages 3-5 summarization).
pub const HOUSEKEEPING_BUDGET_PCT: f32 = 0.10;

/// Number of recent message pairs to always keep during history snipping (Stage 2).
pub const SNIP_KEEP_RECENT_PAIRS: usize = 5;

/// Maximum messages to microcompact in a single pass (Stage 3).
pub const MICROCOMPACT_BATCH_SIZE: usize = 10;
