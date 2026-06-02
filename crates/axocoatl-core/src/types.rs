use serde::{Deserialize, Serialize};

use crate::token::TokenUsageStats;

/// One attached file the user dropped onto a chat. The runtime reads the
/// bytes and routes them by MIME: images get base64-inlined to the vision
/// model, text-bearing files (PDF/CSV/XLSX/plain) get their *extracted text*
/// inlined as a `<attachment>` block so the model sees the content directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAttachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    /// Absolute path the executor reads raw bytes from (used for images).
    pub path: String,
    pub size: u64,
    /// Pre-extracted text from the FileStore (PDF/CSV/XLSX → text; image
    /// OCR for images that have it). When `Some`, the executor inlines this
    /// in addition to (for images) or instead of (for non-images) the raw
    /// bytes — avoids burning the LLM's context on un-parsed binary blobs.
    #[serde(default)]
    pub extracted_text: Option<String>,
}

/// Input to an agent's execute function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInput {
    /// The user or upstream agent message.
    pub content: String,
    /// Optional structured context from the coordination layer.
    pub context: Option<serde_json::Value>,
    /// Conversation history to include (if any).
    pub history: Vec<ChatMessage>,
    /// Per-call system prompt override. When `Some`, replaces the agent's
    /// configured system prompt for this single turn (memory context still
    /// merges as usual). Used by the Chat tab's "per-chat instructions" field.
    #[serde(default)]
    pub system_override: Option<String>,
    /// Per-call model override (e.g. `"llama3.2:1b"`). When `Some`, the request
    /// to the provider is dispatched with this model instead of the agent's
    /// configured one. Same agent, same memory, different model for one turn.
    #[serde(default)]
    pub model_override: Option<String>,
    /// Files attached to this turn (images, text docs). The executor reads
    /// each one's bytes and routes them into the LLM call appropriately
    /// for the provider in use.
    #[serde(default)]
    pub attachments: Vec<AgentAttachment>,
}

impl AgentInput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            context: None,
            history: Vec::new(),
            system_override: None,
            model_override: None,
            attachments: Vec::new(),
        }
    }

    pub fn with_attachments(mut self, attachments: Vec<AgentAttachment>) -> Self {
        self.attachments = attachments;
        self
    }

    pub fn with_context(mut self, context: serde_json::Value) -> Self {
        self.context = Some(context);
        self
    }

    pub fn with_history(mut self, history: Vec<ChatMessage>) -> Self {
        self.history = history;
        self
    }

    pub fn with_system_override(mut self, system: Option<String>) -> Self {
        self.system_override = system;
        self
    }

    pub fn with_model_override(mut self, model: Option<String>) -> Self {
        self.model_override = model;
        self
    }
}

/// Output from an agent's execute function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// The agent's response content.
    pub content: String,
    /// Tool calls the agent wants to make (if any).
    pub tool_calls: Vec<ToolCallRecord>,
    /// Token usage for this execution.
    pub token_usage: TokenUsageStats,
}

impl AgentOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_calls: Vec::new(),
            token_usage: TokenUsageStats::default(),
        }
    }
}

/// A record of a tool call made during execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

/// A chat message in the universal format across all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: MessageContent,
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: MessageContent::Text(content.into()),
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: MessageContent::Text(content.into()),
            name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: MessageContent::Text(content.into()),
            name: None,
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: MessageContent::Text(content.into()),
            name: None,
        }
    }

    /// Get the text content, if this message is a simple text message.
    pub fn text_content(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(s) => Some(s),
            MessageContent::Parts(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    Image { url: String, detail: ImageDetail },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageDetail {
    Auto,
    Low,
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_input_text() {
        let input = AgentInput::text("hello");
        assert_eq!(input.content, "hello");
        assert!(input.context.is_none());
        assert!(input.history.is_empty());
    }

    #[test]
    fn agent_input_with_context() {
        let input = AgentInput::text("hello").with_context(serde_json::json!({"key": "value"}));
        assert!(input.context.is_some());
    }

    #[test]
    fn agent_output_text() {
        let output = AgentOutput::text("response");
        assert_eq!(output.content, "response");
        assert!(output.tool_calls.is_empty());
    }

    #[test]
    fn chat_message_constructors() {
        let sys = ChatMessage::system("You are helpful.");
        assert_eq!(sys.role, MessageRole::System);
        assert_eq!(sys.text_content(), Some("You are helpful."));

        let usr = ChatMessage::user("Hello");
        assert_eq!(usr.role, MessageRole::User);

        let ast = ChatMessage::assistant("Hi there");
        assert_eq!(ast.role, MessageRole::Assistant);

        let tool = ChatMessage::tool("result");
        assert_eq!(tool.role, MessageRole::Tool);
    }

    #[test]
    fn chat_message_serde_roundtrip() {
        let msg = ChatMessage::user("test message");
        let json = serde_json::to_string(&msg).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, MessageRole::User);
        assert_eq!(back.text_content(), Some("test message"));
    }

    #[test]
    fn multimodal_content() {
        let msg = ChatMessage {
            role: MessageRole::User,
            content: MessageContent::Parts(vec![
                ContentPart::Text("What's in this image?".to_string()),
                ContentPart::Image {
                    url: "https://example.com/image.png".to_string(),
                    detail: ImageDetail::Auto,
                },
            ]),
            name: None,
        };
        assert!(msg.text_content().is_none()); // Parts, not Text
        let json = serde_json::to_string(&msg).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, MessageRole::User);
    }
}
