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
    /// Run this call statelessly — build the request from this input alone
    /// (system override or configured prompt + history + content), without
    /// reading or writing the agent's persistent session or checkpoint. A pure
    /// function of the input; the right mode for per-request prompt/model
    /// variants and for scoring an agent over independent inputs.
    #[serde(default)]
    pub stateless: bool,
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
            stateless: false,
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

    pub fn with_stateless(mut self, stateless: bool) -> Self {
        self.stateless = stateless;
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

/// A tool call requested by the LLM. Canonical, provider-independent shape —
/// every provider parses its native tool-call format into this and serializes
/// it back out when replaying the conversation. Lives in `axocoatl-core` (not
/// `axocoatl-llm`) because [`ChatMessage`] carries it; `axocoatl-llm`
/// re-exports it so existing `axocoatl_llm::ToolCall` paths keep working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id. Correlates a result back to its request
    /// (OpenAI/Anthropic/Mistral key on this). Gemini keys on `name` instead.
    pub id: String,
    pub name: String,
    /// Parsed arguments (already deserialized from the LLM's JSON string).
    pub arguments: serde_json::Value,
}

/// A chat message in the universal format across all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: MessageContent,
    /// For `Tool` results this carries the tool's function name (Gemini
    /// correlates `functionResponse` by name); otherwise the optional
    /// participant name. `#[serde(default)]` keeps older payloads loadable.
    #[serde(default)]
    pub name: Option<String>,
    /// Tool calls the assistant is requesting on this turn. Non-empty only for
    /// `Assistant` messages that invoke tools; providers serialize these into
    /// their native assistant-tool-call format so the model sees its own prior
    /// request when the conversation is replayed.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// For a `Tool` result message: the id of the originating tool call. Lets
    /// providers correlate each result with the call that produced it.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: MessageContent::Text(content.into()),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: MessageContent::Text(content.into()),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: MessageContent::Text(content.into()),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant turn that requests one or more tool calls. `content` may be
    /// empty (the model often returns only tool calls with no prose).
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: MessageContent::Text(content.into()),
            name: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: MessageContent::Text(content.into()),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A tool-result message answering a specific tool call. Carries both the
    /// tool's function `name` (for name-keyed providers like Gemini) and the
    /// originating `tool_call_id` (for id-keyed providers like OpenAI).
    pub fn tool_result(
        content: impl Into<String>,
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: MessageContent::Text(content.into()),
            name: Some(name.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
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
            tool_calls: Vec::new(),
            tool_call_id: None,
        };
        assert!(msg.text_content().is_none()); // Parts, not Text
        let json = serde_json::to_string(&msg).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, MessageRole::User);
    }

    #[test]
    fn assistant_tool_call_message_roundtrip() {
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            arguments: serde_json::json!({ "location": "NYC" }),
        };
        let msg = ChatMessage::assistant_with_tool_calls("", vec![call.clone()]);
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.tool_calls, vec![call]);

        let json = serde_json::to_string(&msg).unwrap();
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].name, "get_weather");
    }

    #[test]
    fn tool_result_message_carries_name_and_id() {
        let msg = ChatMessage::tool_result("72F", "get_weather", "call_1");
        assert_eq!(msg.role, MessageRole::Tool);
        assert_eq!(msg.name.as_deref(), Some("get_weather"));
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msg.text_content(), Some("72F"));
    }

    #[test]
    fn legacy_chat_message_without_tool_fields_deserializes() {
        // Payloads serialized before tool_calls/tool_call_id existed must still
        // load — the new fields are `#[serde(default)]`.
        let legacy = r#"{"role":"User","content":{"Text":"hi"},"name":null}"#;
        let msg: ChatMessage = serde_json::from_str(legacy).unwrap();
        assert_eq!(msg.text_content(), Some("hi"));
        assert!(msg.tool_calls.is_empty());
        assert!(msg.tool_call_id.is_none());
    }
}
