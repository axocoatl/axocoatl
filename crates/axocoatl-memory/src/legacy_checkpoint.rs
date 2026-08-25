//! Decode-only compatibility for checkpoint files written by Axocoatl 0.1.x.
//!
//! This module deliberately uses the exact Bincode release that wrote those
//! files. It must never be used by the current save path. Keeping the legacy
//! wire structs private prevents a new persistence caller from accidentally
//! extending the compatibility dependency's lifetime.

use axocoatl_core::{MessageRole, TokenUsageStats};

use crate::checkpoint::AgentCheckpoint;
use crate::session::{StoredMessage, StoredToolCall};

/// Checkpoints are conversation caches, not arbitrary object containers. This
/// cap bounds both untrusted file input and Bincode's allocation accounting.
pub(super) const MAX_CHECKPOINT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DECODE_ALLOCATION: usize = MAX_CHECKPOINT_BYTES * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegacyCheckpointSchema {
    V0_1_0,
    V0_1_1ThroughV0_1_4,
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
enum LegacyMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl From<LegacyMessageRole> for MessageRole {
    fn from(role: LegacyMessageRole) -> Self {
        match role {
            LegacyMessageRole::System => Self::System,
            LegacyMessageRole::User => Self::User,
            LegacyMessageRole::Assistant => Self::Assistant,
            LegacyMessageRole::Tool => Self::Tool,
        }
    }
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
struct LegacyTokenUsageStats {
    input_tokens: usize,
    output_tokens: usize,
    reasoning_tokens: Option<usize>,
}

impl From<LegacyTokenUsageStats> for TokenUsageStats {
    fn from(usage: LegacyTokenUsageStats) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
struct LegacyStoredMessageV0_1_0 {
    role: LegacyMessageRole,
    content: String,
    timestamp: u64,
    token_count: usize,
}

impl From<LegacyStoredMessageV0_1_0> for StoredMessage {
    fn from(message: LegacyStoredMessageV0_1_0) -> Self {
        Self {
            role: message.role.into(),
            content: message.content,
            timestamp: message.timestamp,
            token_count: message.token_count,
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
struct LegacyStoredToolCall {
    id: String,
    name: String,
    arguments_json: String,
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
struct LegacyStoredMessageV0_1_1 {
    role: LegacyMessageRole,
    content: String,
    timestamp: u64,
    token_count: usize,
    name: Option<String>,
    tool_calls: Vec<LegacyStoredToolCall>,
    tool_call_id: Option<String>,
}

#[derive(Debug, bincode::Encode, bincode::Decode)]
struct LegacyAgentCheckpoint<M> {
    version: u64,
    agent_id: String,
    checkpoint_time: u64,
    session_messages: Vec<M>,
    cumulative_token_usage: LegacyTokenUsageStats,
    behavior_state: Option<String>,
}

fn decode_exact<T: bincode::Decode<()> + bincode::Encode>(bytes: &[u8]) -> Result<T, String> {
    let config = bincode::config::standard().with_limit::<MAX_DECODE_ALLOCATION>();
    let (value, consumed) = bincode::decode_from_slice(bytes, config)
        .map_err(|error| format!("Bincode decode failed: {error}"))?;
    if consumed != bytes.len() {
        return Err(format!(
            "Bincode checkpoint has {} trailing bytes",
            bytes.len().saturating_sub(consumed)
        ));
    }
    let canonical = bincode::encode_to_vec(&value, bincode::config::standard())
        .map_err(|error| format!("Bincode canonical re-encode failed: {error}"))?;
    if canonical != bytes {
        return Err("Bincode checkpoint is not canonically encoded".to_string());
    }
    Ok(value)
}

fn convert_v0_1_0(checkpoint: LegacyAgentCheckpoint<LegacyStoredMessageV0_1_0>) -> AgentCheckpoint {
    AgentCheckpoint {
        version: checkpoint.version,
        agent_id: checkpoint.agent_id,
        checkpoint_time: checkpoint.checkpoint_time,
        session_messages: checkpoint
            .session_messages
            .into_iter()
            .map(StoredMessage::from)
            .collect(),
        cumulative_token_usage: checkpoint.cumulative_token_usage.into(),
        cumulative_token_usage_known: false,
        behavior_state: checkpoint.behavior_state,
    }
}

fn convert_v0_1_1(checkpoint: LegacyAgentCheckpoint<LegacyStoredMessageV0_1_1>) -> AgentCheckpoint {
    let mut session_messages = Vec::with_capacity(checkpoint.session_messages.len());
    for message in checkpoint.session_messages {
        let role: MessageRole = message.role.into();
        let mut tool_calls = Vec::with_capacity(message.tool_calls.len());
        for call in message.tool_calls {
            tool_calls.push(StoredToolCall {
                id: call.id,
                name: call.name,
                arguments_json: call.arguments_json,
                provider_metadata: Default::default(),
            });
        }
        session_messages.push(StoredMessage {
            role,
            content: message.content,
            timestamp: message.timestamp,
            token_count: message.token_count,
            name: message.name,
            tool_calls,
            tool_call_id: message.tool_call_id,
        });
    }

    AgentCheckpoint {
        version: checkpoint.version,
        agent_id: checkpoint.agent_id,
        checkpoint_time: checkpoint.checkpoint_time,
        session_messages,
        cumulative_token_usage: checkpoint.cumulative_token_usage.into(),
        cumulative_token_usage_known: false,
        behavior_state: checkpoint.behavior_state,
    }
}

/// Decode either exact schema shipped in the 0.1 series. Both parses require
/// full consumption. In the unlikely event both layouts parse to different
/// values, reject the bytes as ambiguous rather than guessing around history.
pub(super) fn decode(bytes: &[u8]) -> Result<(AgentCheckpoint, LegacyCheckpointSchema), String> {
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err(format!(
            "legacy checkpoint is {} bytes; limit is {MAX_CHECKPOINT_BYTES}",
            bytes.len()
        ));
    }

    let modern =
        decode_exact::<LegacyAgentCheckpoint<LegacyStoredMessageV0_1_1>>(bytes).map(convert_v0_1_1);
    let original =
        decode_exact::<LegacyAgentCheckpoint<LegacyStoredMessageV0_1_0>>(bytes).map(convert_v0_1_0);

    match (modern, original) {
        (Ok(modern), Ok(original)) => {
            let modern_value = serde_json::to_value(&modern)
                .map_err(|error| format!("could not compare decoded legacy schemas: {error}"))?;
            let original_value = serde_json::to_value(&original)
                .map_err(|error| format!("could not compare decoded legacy schemas: {error}"))?;
            if modern_value != original_value {
                return Err("legacy checkpoint matches both 0.1 layouts ambiguously".to_string());
            }
            Ok((modern, LegacyCheckpointSchema::V0_1_1ThroughV0_1_4))
        }
        (Ok(checkpoint), Err(_)) => Ok((checkpoint, LegacyCheckpointSchema::V0_1_1ThroughV0_1_4)),
        (Err(_), Ok(checkpoint)) => Ok((checkpoint, LegacyCheckpointSchema::V0_1_0)),
        (Err(modern), Err(original)) => Err(format!(
            "not a supported 0.1.x checkpoint ({modern}; {original})"
        )),
    }
}
