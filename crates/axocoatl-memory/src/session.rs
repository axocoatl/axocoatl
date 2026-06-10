use axocoatl_core::{ChatMessage, MessageContent, MessageRole, ToolCall};
use serde::{Deserialize, Serialize};

/// In-memory session transcript — append-only.
/// This is what gets passed to the LLM as conversation history.
pub struct SessionMemory {
    messages: Vec<StoredMessage>,
    token_count: usize,
}

/// A tool call persisted alongside an assistant message. `bincode` has no
/// representation for `serde_json::Value`, so the arguments are stored as their
/// JSON text and parsed back when reconstructing a [`ToolCall`].
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct StoredToolCall {
    pub id: String,
    pub name: String,
    /// Tool-call arguments as a JSON string.
    pub arguments_json: String,
}

impl StoredToolCall {
    fn from_tool_call(tc: &ToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            name: tc.name.clone(),
            arguments_json: serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
        }
    }

    fn to_tool_call(&self) -> ToolCall {
        ToolCall {
            id: self.id.clone(),
            name: self.name.clone(),
            arguments: serde_json::from_str(&self.arguments_json)
                .unwrap_or(serde_json::Value::Null),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct StoredMessage {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: u64,
    pub token_count: usize,
    /// For `Tool` results: the tool's function name (Gemini correlates by
    /// name). Empty/`None` for plain text messages. `#[serde(default)]` and a
    /// trailing position keep older checkpoints/JSON loadable.
    #[serde(default)]
    pub name: Option<String>,
    /// Tool calls requested by an `Assistant` turn.
    #[serde(default)]
    pub tool_calls: Vec<StoredToolCall>,
    /// For a `Tool` result: the id of the originating tool call.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl SessionMemory {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            token_count: 0,
        }
    }

    /// Append a plain text message, tracking its token count.
    /// `token_count` should be pre-computed by the caller using a TokenCounter.
    pub fn append(&mut self, role: MessageRole, content: impl Into<String>, token_count: usize) {
        let content = content.into();
        self.token_count += token_count;
        self.messages.push(StoredMessage {
            role,
            content,
            timestamp: now_timestamp(),
            token_count,
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }

    /// Append an assistant turn that requested one or more tool calls. The
    /// tool-call payload must be recorded so the follow-up request replays the
    /// model's own `assistant(tool_calls)` turn — without it, providers reject
    /// the subsequent tool-result messages as orphaned.
    pub fn append_assistant_tool_calls(
        &mut self,
        content: impl Into<String>,
        tool_calls: &[ToolCall],
        token_count: usize,
    ) {
        let content = content.into();
        self.token_count += token_count;
        self.messages.push(StoredMessage {
            role: MessageRole::Assistant,
            content,
            timestamp: now_timestamp(),
            token_count,
            name: None,
            tool_calls: tool_calls
                .iter()
                .map(StoredToolCall::from_tool_call)
                .collect(),
            tool_call_id: None,
        });
    }

    /// Append a tool-result message answering a specific tool call. Records both
    /// the tool's function `name` (name-keyed providers like Gemini) and the
    /// originating `tool_call_id` (id-keyed providers like OpenAI).
    pub fn append_tool_result(
        &mut self,
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        token_count: usize,
    ) {
        let content = content.into();
        self.token_count += token_count;
        self.messages.push(StoredMessage {
            role: MessageRole::Tool,
            content,
            timestamp: now_timestamp(),
            token_count,
            name: Some(name.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        });
    }

    /// Get messages as LLM-ready ChatMessage vec, faithfully reconstructing
    /// assistant tool-call turns and tool-result correlation fields.
    pub fn as_chat_messages(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: MessageContent::Text(m.content.clone()),
                name: m.name.clone(),
                tool_calls: m
                    .tool_calls
                    .iter()
                    .map(StoredToolCall::to_tool_call)
                    .collect(),
                tool_call_id: m.tool_call_id.clone(),
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

    /// Replace the transcript with a compacted set of chat messages — used by
    /// context/budget compaction, which produces a `Vec<ChatMessage>`. Per-message
    /// token counts (which the compaction layer doesn't carry) are recomputed via
    /// `count`. Tool-call correlation (assistant `tool_calls` ↔ tool `tool_call_id`)
    /// is preserved.
    pub fn replace_with_chat_messages(
        &mut self,
        messages: &[ChatMessage],
        count: impl Fn(&str) -> usize,
    ) {
        let stored: Vec<StoredMessage> = messages
            .iter()
            .map(|m| {
                let content = match &m.content {
                    MessageContent::Text(s) => s.clone(),
                    MessageContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            axocoatl_core::ContentPart::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let token_count = count(&content);
                StoredMessage {
                    role: m.role.clone(),
                    content,
                    timestamp: now_timestamp(),
                    token_count,
                    name: m.name.clone(),
                    tool_calls: m
                        .tool_calls
                        .iter()
                        .map(StoredToolCall::from_tool_call)
                        .collect(),
                    tool_call_id: m.tool_call_id.clone(),
                }
            })
            .collect();
        self.token_count = stored.iter().map(|m| m.token_count).sum();
        self.messages = stored;
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
    fn tool_call_round_trip_reconstructs_assistant_and_tool_messages() {
        let mut session = SessionMemory::new();
        session.append(MessageRole::User, "weather in NYC?", 3);
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            arguments: serde_json::json!({ "location": "NYC" }),
        };
        session.append_assistant_tool_calls("", std::slice::from_ref(&call), 0);
        session.append_tool_result("get_weather", "call_1", "{\"temp\":72}", 4);

        let msgs = session.as_chat_messages();
        assert_eq!(msgs.len(), 3);

        // Assistant turn carries the tool call verbatim.
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].tool_calls.len(), 1);
        assert_eq!(msgs[1].tool_calls[0].id, "call_1");
        assert_eq!(msgs[1].tool_calls[0].arguments["location"], "NYC");

        // Tool result correlates by both name (Gemini) and id (OpenAI).
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].name.as_deref(), Some("get_weather"));
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn tool_calls_survive_checkpoint_bincode_round_trip() {
        let mut session = SessionMemory::new();
        let call = ToolCall {
            id: "call_9".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({ "q": "rust" }),
        };
        session.append_assistant_tool_calls("looking it up", std::slice::from_ref(&call), 2);
        session.append_tool_result("search", "call_9", "ok", 1);

        let stored = session.messages().to_vec();
        let bytes = bincode::encode_to_vec(&stored, bincode::config::standard()).unwrap();
        let (back, _): (Vec<StoredMessage>, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

        let mut restored = SessionMemory::new();
        restored.restore(back);
        let msgs = restored.as_chat_messages();
        assert_eq!(msgs[0].tool_calls[0].name, "search");
        assert_eq!(msgs[0].tool_calls[0].arguments["q"], "rust");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("call_9"));
    }

    #[test]
    fn replace_with_chat_messages_swaps_and_recounts() {
        let mut session = SessionMemory::new();
        for i in 0..3 {
            session.append(MessageRole::User, format!("old {i}"), 99);
        }
        let replacement = vec![
            ChatMessage::system("summary of earlier"),
            ChatMessage::user("recent question"),
        ];
        session.replace_with_chat_messages(&replacement, |s| s.len() / 4 + 1);

        assert_eq!(session.len(), 2);
        let msgs = session.as_chat_messages();
        assert_eq!(msgs[0].text_content(), Some("summary of earlier"));
        assert_eq!(msgs[1].text_content(), Some("recent question"));
        // Token count recomputed from the new content, not the old 3×99.
        let expected: usize = ["summary of earlier", "recent question"]
            .iter()
            .map(|s| s.len() / 4 + 1)
            .sum();
        assert_eq!(session.total_tokens(), expected);
    }

    #[test]
    fn replace_with_chat_messages_preserves_tool_correlation() {
        let mut session = SessionMemory::new();
        let assistant = ChatMessage {
            role: MessageRole::Assistant,
            content: MessageContent::Text("calling".to_string()),
            name: None,
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "search".to_string(),
                arguments: serde_json::json!({ "q": "x" }),
            }],
            tool_call_id: None,
        };
        let tool = ChatMessage {
            role: MessageRole::Tool,
            content: MessageContent::Text("result".to_string()),
            name: Some("search".to_string()),
            tool_calls: vec![],
            tool_call_id: Some("c1".to_string()),
        };
        session.replace_with_chat_messages(&[assistant, tool], |s| s.len() / 4 + 1);

        let msgs = session.as_chat_messages();
        assert_eq!(msgs[0].tool_calls[0].id, "c1");
        assert_eq!(msgs[0].tool_calls[0].arguments["q"], "x");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[1].name.as_deref(), Some("search"));
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
