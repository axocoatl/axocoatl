use axocoatl_core::{ChatMessage, MessageContent, MessageRole};
use serde::{Deserialize, Serialize};

/// In-memory session transcript — append-only.
/// This is what gets passed to the LLM as conversation history.
pub struct SessionMemory {
    messages: Vec<StoredMessage>,
    token_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct StoredMessage {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: u64,
    pub token_count: usize,
}

impl SessionMemory {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            token_count: 0,
        }
    }

    /// Append a message, tracking its token count.
    /// `token_count` should be pre-computed by the caller using a TokenCounter.
    pub fn append(&mut self, role: MessageRole, content: impl Into<String>, token_count: usize) {
        let content = content.into();
        self.token_count += token_count;
        self.messages.push(StoredMessage {
            role,
            content,
            timestamp: now_timestamp(),
            token_count,
        });
    }

    /// Get messages as LLM-ready ChatMessage vec.
    pub fn as_chat_messages(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: MessageContent::Text(m.content.clone()),
                name: None,
            })
            .collect()
    }

    /// Total tokens in current session.
    pub fn total_tokens(&self) -> usize {
        self.token_count
    }

    /// Truncate to last N messages (sliding window compression).
    pub fn truncate_to_last(&mut self, n: usize) {
        if self.messages.len() > n {
            let removed: usize = self
                .messages
                .drain(..self.messages.len() - n)
                .map(|m| m.token_count)
                .sum();
            self.token_count -= removed;
        }
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get the raw stored messages (for checkpointing).
    pub fn messages(&self) -> &[StoredMessage] {
        &self.messages
    }

    /// Restore from stored messages (after checkpoint load).
    pub fn restore(&mut self, messages: Vec<StoredMessage>) {
        self.token_count = messages.iter().map(|m| m.token_count).sum();
        self.messages = messages;
    }

    /// Find a segment boundary: the index of the first message after
    /// the given fraction of the conversation. Used by compression Stage 4.
    pub fn segment_boundary(&self, fraction: f32) -> usize {
        let target_index = (self.messages.len() as f32 * fraction) as usize;
        target_index.min(self.messages.len())
    }

    /// Archive (remove) all messages before the given index.
    /// Returns the archived messages.
    pub fn archive_before(&mut self, index: usize) -> Vec<StoredMessage> {
        if index >= self.messages.len() {
            return Vec::new();
        }
        let archived: Vec<StoredMessage> = self.messages.drain(..index).collect();
        let archived_tokens: usize = archived.iter().map(|m| m.token_count).sum();
        self.token_count = self.token_count.saturating_sub(archived_tokens);
        archived
    }
}

impl Default for SessionMemory {
    fn default() -> Self {
        Self::new()
    }
}

fn now_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_retrieve() {
        let mut session = SessionMemory::new();
        session.append(MessageRole::User, "hello", 2);
        session.append(MessageRole::Assistant, "hi there", 3);

        assert_eq!(session.len(), 2);
        assert_eq!(session.total_tokens(), 5);

        let msgs = session.as_chat_messages();
        assert_eq!(msgs[0].text_content(), Some("hello"));
        assert_eq!(msgs[1].text_content(), Some("hi there"));
    }

    #[test]
    fn truncate_to_last() {
        let mut session = SessionMemory::new();
        for i in 0..10 {
            session.append(MessageRole::User, format!("msg {i}"), 10);
        }
        assert_eq!(session.len(), 10);
        assert_eq!(session.total_tokens(), 100);

        session.truncate_to_last(3);
        assert_eq!(session.len(), 3);
        assert_eq!(session.total_tokens(), 30);

        let msgs = session.as_chat_messages();
        assert_eq!(msgs[0].text_content(), Some("msg 7"));
    }

    #[test]
    fn truncate_no_op_when_under() {
        let mut session = SessionMemory::new();
        session.append(MessageRole::User, "one", 5);
        session.truncate_to_last(10);
        assert_eq!(session.len(), 1);
    }

    #[test]
    fn empty_session() {
        let session = SessionMemory::new();
        assert!(session.is_empty());
        assert_eq!(session.total_tokens(), 0);
        assert!(session.as_chat_messages().is_empty());
    }

    #[test]
    fn restore_from_stored() {
        let mut session = SessionMemory::new();
        session.append(MessageRole::User, "hello", 5);
        session.append(MessageRole::Assistant, "hi", 3);

        let stored = session.messages().to_vec();

        let mut restored = SessionMemory::new();
        restored.restore(stored);
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.total_tokens(), 8);
    }
}
