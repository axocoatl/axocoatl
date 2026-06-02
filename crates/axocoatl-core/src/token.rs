use serde::{Deserialize, Serialize};

/// Token usage statistics for a single LLM call or agent execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct TokenUsageStats {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_tokens: Option<usize>,
}

impl TokenUsageStats {
    pub fn new(input: usize, output: usize) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
        }
    }

    pub fn with_reasoning(mut self, tokens: usize) -> Self {
        self.reasoning_tokens = Some(tokens);
        self
    }

    pub fn total(&self) -> usize {
        self.input_tokens + self.output_tokens + self.reasoning_tokens.unwrap_or(0)
    }

    /// Merge another usage stat into this one (accumulate).
    pub fn merge(&mut self, other: &TokenUsageStats) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        match (&mut self.reasoning_tokens, other.reasoning_tokens) {
            (Some(a), Some(b)) => *a += b,
            (None, Some(b)) => self.reasoning_tokens = Some(b),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_default() {
        let stats = TokenUsageStats::default();
        assert_eq!(stats.total(), 0);
    }

    #[test]
    fn token_usage_total() {
        let stats = TokenUsageStats::new(100, 50);
        assert_eq!(stats.total(), 150);
    }

    #[test]
    fn token_usage_with_reasoning() {
        let stats = TokenUsageStats::new(100, 50).with_reasoning(200);
        assert_eq!(stats.total(), 350);
    }

    #[test]
    fn token_usage_merge() {
        let mut a = TokenUsageStats::new(100, 50);
        let b = TokenUsageStats::new(200, 75);
        a.merge(&b);
        assert_eq!(a.input_tokens, 300);
        assert_eq!(a.output_tokens, 125);
        assert_eq!(a.total(), 425);
    }

    #[test]
    fn token_usage_merge_reasoning() {
        let mut a = TokenUsageStats::new(10, 5).with_reasoning(20);
        let b = TokenUsageStats::new(10, 5).with_reasoning(30);
        a.merge(&b);
        assert_eq!(a.reasoning_tokens, Some(50));
    }

    #[test]
    fn token_usage_serde_roundtrip() {
        let stats = TokenUsageStats::new(100, 50).with_reasoning(200);
        let json = serde_json::to_string(&stats).unwrap();
        let back: TokenUsageStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total(), 350);
    }
}
