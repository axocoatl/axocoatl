//! Exact Postcard wire shapes preceding multimodal checkpoint messages.
use super::*;
use crate::session::StoredToolCall;
use axocoatl_core::MessageRole;

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PreviousStoredMessage {
    role: MessageRole,
    content: String,
    timestamp: u64,
    token_count: usize,
    name: Option<String>,
    tool_calls: Vec<StoredToolCall>,
    tool_call_id: Option<String>,
}
impl From<PreviousStoredMessage> for StoredMessage {
    fn from(value: PreviousStoredMessage) -> Self {
        Self {
            role: value.role,
            content: value.content,
            timestamp: value.timestamp,
            token_count: value.token_count,
            name: value.name,
            tool_calls: value.tool_calls,
            tool_call_id: value.tool_call_id,
            content_parts: None,
        }
    }
}
#[cfg(test)]
impl From<StoredMessage> for PreviousStoredMessage {
    fn from(value: StoredMessage) -> Self {
        assert!(value.content_parts.is_none());
        Self {
            role: value.role,
            content: value.content,
            timestamp: value.timestamp,
            token_count: value.token_count,
            name: value.name,
            tool_calls: value.tool_calls,
            tool_call_id: value.tool_call_id,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AgentCheckpointPostcardV1 {
    pub version: u64,
    pub agent_id: String,
    pub checkpoint_time: u64,
    pub session_messages: Vec<PreviousStoredMessage>,
    pub cumulative_token_usage: TokenUsageStats,
    pub behavior_state: Option<String>,
}
impl From<AgentCheckpointPostcardV1> for AgentCheckpoint {
    fn from(value: AgentCheckpointPostcardV1) -> Self {
        Self {
            version: value.version,
            agent_id: value.agent_id,
            checkpoint_time: value.checkpoint_time,
            session_messages: value.session_messages.into_iter().map(Into::into).collect(),
            cumulative_token_usage: value.cumulative_token_usage,
            cumulative_token_usage_known: false,
            behavior_state: value.behavior_state,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AgentCheckpointPostcardV2 {
    pub version: u64,
    pub agent_id: String,
    pub checkpoint_time: u64,
    pub session_messages: Vec<PreviousStoredMessage>,
    pub cumulative_token_usage: TokenUsageStats,
    pub cumulative_token_usage_known: bool,
    pub behavior_state: Option<String>,
}
impl From<AgentCheckpointPostcardV2> for AgentCheckpoint {
    fn from(value: AgentCheckpointPostcardV2) -> Self {
        Self {
            version: value.version,
            agent_id: value.agent_id,
            checkpoint_time: value.checkpoint_time,
            session_messages: value.session_messages.into_iter().map(Into::into).collect(),
            cumulative_token_usage: value.cumulative_token_usage,
            cumulative_token_usage_known: value.cumulative_token_usage_known,
            behavior_state: value.behavior_state,
        }
    }
}
