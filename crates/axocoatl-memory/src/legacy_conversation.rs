//! Versioned v1 canonical-history to model-conversation projection.
//!
//! This preserves the existing restart cache policy. It does not authorize
//! native tool execution or infer accepted v2 generations. Callers select the
//! visible canonical turns; full source history remains independently retained.

use axocoatl_session::turn_ledger::{
    SessionTurn, SessionTurnContextReference, SessionTurnLifecycle,
};
use std::collections::{HashSet, VecDeque};

pub const LEGACY_CONVERSATION_PROJECTION_VERSION: &str = "canonical-v1-restart-context-v1";
/// Recovery cache bound; the canonical ledger remains complete.
pub const SESSION_CHECKPOINT_MESSAGE_CAP: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReplayPolicy {
    CompleteNativeGroups,
    OmitNativeGroups,
}

fn inline_context_block(context: &[SessionTurnContextReference]) -> Option<String> {
    let mut lines = vec!["## Context the user attached:".to_string()];
    let mut included = false;
    for reference in context {
        match reference.kind.as_str() {
            "code_selection" => {
                included = true;
                let path = reference
                    .metadata
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .or(reference.origin.as_deref())
                    .unwrap_or(reference.display_name.as_str());
                let start = reference
                    .metadata
                    .get("start_line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                let end = reference
                    .metadata
                    .get("end_line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(start);
                let range = if start == end {
                    start.to_string()
                } else {
                    format!("{start}-{end}")
                };
                let language = reference
                    .metadata
                    .get("language")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let content = reference
                    .metadata
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                lines.push(format!("\n### File: `{path}` (lines {range})"));
                lines.push(format!("```{language}"));
                lines.push(content.to_string());
                lines.push("```".to_string());
            }
            "coordination_reference" => {
                if let Some(content) = reference
                    .metadata
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                {
                    included = true;
                    lines.push(format!(
                        "\n### Recorded coordination evidence: {}",
                        reference.display_name
                    ));
                    lines.push(
                        "The following is quoted execution evidence attached by the user."
                            .to_string(),
                    );
                    lines.push(content.to_string());
                }
            }
            "browser_selection" => {
                included = true;
                let url = reference
                    .metadata
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .or(reference.origin.as_deref())
                    .unwrap_or("(unknown)");
                let selector = reference
                    .metadata
                    .get("selector")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let html = reference
                    .metadata
                    .get("html")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                lines.push(format!("\n### DOM element on {url}"));
                if !selector.is_empty() {
                    lines.push(format!("Selector: `{selector}`"));
                }
                if !html.is_empty() {
                    lines.push("```html".to_string());
                    lines.push(html.to_string());
                    lines.push("```".to_string());
                }
            }
            _ => {}
        }
    }
    included.then(|| {
        lines.push(String::new());
        lines.join("\n")
    })
}

pub fn checkpoint_user_content(turn: &SessionTurn) -> String {
    inline_context_block(&turn.context).map_or_else(
        || turn.user_input.clone(),
        |context| format!("{context}\n\n{}", turn.user_input),
    )
}

fn checkpoint_token_count(counter: &dyn Fn(&str) -> usize, text: &str) -> usize {
    const CHUNK_BYTES: usize = 32 * 1024;
    if text.len() <= CHUNK_BYTES {
        return counter(text);
    }
    let mut total = 0_usize;
    let mut start = 0_usize;
    while start < text.len() {
        let mut end = (start + CHUNK_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        total = total.saturating_add(counter(&text[start..end]));
        start = end;
    }
    total
}

pub fn checkpoint_turn_minimum_bytes(turn: &SessionTurn) -> usize {
    let assistant_bytes = if turn.agent_outputs.is_empty() {
        turn.final_output
            .as_ref()
            .map_or(turn.partial_output.len(), String::len)
    } else {
        turn.agent_outputs
            .iter()
            .filter(|output| !output.superseded)
            .map(|output| output.output.len())
            .sum()
    };
    turn.user_input.len().saturating_add(assistant_bytes)
}

pub fn project_history(
    counter: &dyn Fn(&str) -> usize,
    turns: &[SessionTurn],
    policy: ToolReplayPolicy,
) -> Vec<crate::StoredMessage> {
    let mut projected = Vec::new();
    for turn in turns {
        // Cancelled work remains fully durable in the canonical turn ledger
        // and visible Route, but it is not conversation context. Replaying its
        // user request and completed tool pairs invites a newly spawned model
        // to resume an explicitly stopped edit plan instead of following the
        // next prompt.
        if turn.status == SessionTurnLifecycle::Cancelled {
            continue;
        }
        let user_content = checkpoint_user_content(turn);
        projected.push(crate::StoredMessage {
            content_parts: None,
            role: axocoatl_core::MessageRole::User,
            token_count: checkpoint_token_count(counter, &user_content),
            content: user_content,
            timestamp: turn.created_at / 1_000,
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });

        // Rebuild one provider assistant message per original response, followed
        // by every correlated result. Parallel calls must stay grouped: Anthropic
        // replay metadata on the first call describes the complete native block
        // array, and splitting that response makes the next request invalid.
        // Incomplete, legacy, malformed, or bounded/truncated groups remain
        // visible in Route evidence but are omitted atomically from provider
        // history instead of replaying an orphaned or protocol-invalid call.
        let mut projected_groups = HashSet::new();
        for started in turn
            .execution_events
            .iter()
            .filter(|_| policy == ToolReplayPolicy::CompleteNativeGroups)
        {
            if started.event.kind != "tool_started" {
                continue;
            }
            let Some(agent_id) = started
                .event
                .metadata
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(response_group) = started
                .event
                .metadata
                .get("provider_response_group")
                .and_then(serde_json::Value::as_u64)
            else {
                continue;
            };
            if !projected_groups.insert((agent_id.to_string(), response_group)) {
                continue;
            }

            let mut group_starts = turn
                .execution_events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.event.kind == "tool_started"
                        && event
                            .event
                            .metadata
                            .get("agent_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(agent_id)
                        && event
                            .event
                            .metadata
                            .get("provider_response_group")
                            .and_then(serde_json::Value::as_u64)
                            == Some(response_group)
                })
                .collect::<Vec<_>>();
            if group_starts.is_empty() {
                continue;
            }
            let Some(provider_call_count) = group_starts
                .first()
                .and_then(|(_, event)| event.event.metadata.get("provider_call_count"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|count| usize::try_from(count).ok())
                .filter(|count| *count > 0 && *count == group_starts.len())
            else {
                continue;
            };
            group_starts.sort_by_key(|(_, event)| {
                event
                    .event
                    .metadata
                    .get("provider_call_index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(u64::MAX)
            });
            if group_starts
                .iter()
                .enumerate()
                .any(|(expected, (_, event))| {
                    event
                        .event
                        .metadata
                        .get("provider_call_index")
                        .and_then(serde_json::Value::as_u64)
                        != u64::try_from(expected).ok()
                        || event
                            .event
                            .metadata
                            .get("provider_call_count")
                            .and_then(serde_json::Value::as_u64)
                            != u64::try_from(provider_call_count).ok()
                })
            {
                continue;
            }

            let mut assistant_content: Option<String> = None;
            let mut tool_calls = Vec::with_capacity(group_starts.len());
            let mut tool_results = Vec::with_capacity(group_starts.len());
            let mut seen_call_occurrences = HashSet::with_capacity(group_starts.len());
            let mut consumed_result_indices = HashSet::with_capacity(group_starts.len());
            let mut replayable = true;
            for (start_index, group_start) in group_starts {
                let metadata = &group_start.event.metadata;
                let Some(provider_call_index) = metadata
                    .get("provider_call_index")
                    .and_then(serde_json::Value::as_u64)
                else {
                    replayable = false;
                    break;
                };
                if metadata
                    .get("call_id_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                {
                    replayable = false;
                    break;
                }
                if metadata
                    .get("tool_name_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                {
                    replayable = false;
                    break;
                }
                let Some(call_id) = metadata.get("call_id").and_then(serde_json::Value::as_str)
                else {
                    replayable = false;
                    break;
                };
                let Some(call_hash) = metadata
                    .get("call_id_sha256")
                    .and_then(serde_json::Value::as_str)
                else {
                    replayable = false;
                    break;
                };
                let Some(occurrence) = metadata
                    .get("occurrence")
                    .and_then(serde_json::Value::as_u64)
                else {
                    replayable = false;
                    break;
                };
                if !seen_call_occurrences.insert((call_hash.to_string(), occurrence)) {
                    replayable = false;
                    break;
                }
                let Some(tool_name) = metadata
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                else {
                    replayable = false;
                    break;
                };
                if metadata
                    .get("arguments_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                {
                    replayable = false;
                    break;
                }
                let Some(_executed_arguments) =
                    metadata.get("arguments").filter(|value| value.is_object())
                else {
                    replayable = false;
                    break;
                };
                if metadata
                    .get("provider_arguments_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                {
                    replayable = false;
                    break;
                }
                let Some(provider_arguments) = metadata
                    .get("provider_arguments")
                    .filter(|value| value.is_object())
                else {
                    replayable = false;
                    break;
                };
                if provider_call_index == 0 {
                    if metadata
                        .get("assistant_content_truncated")
                        .and_then(serde_json::Value::as_bool)
                        != Some(false)
                    {
                        replayable = false;
                        break;
                    }
                    let Some(group_content) = metadata
                        .get("assistant_content")
                        .and_then(serde_json::Value::as_str)
                    else {
                        replayable = false;
                        break;
                    };
                    assistant_content = Some(group_content.to_string());
                } else if metadata.contains_key("assistant_content")
                    || metadata.contains_key("assistant_content_truncated")
                {
                    replayable = false;
                    break;
                }

                if metadata
                    .get("provider_metadata_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                {
                    replayable = false;
                    break;
                }
                let Some(provider_metadata) = metadata
                    .get("provider_metadata")
                    .and_then(|value| {
                        serde_json::from_value::<axocoatl_core::ProviderMetadata>(value.clone())
                            .ok()
                    })
                    .filter(|metadata| !metadata.is_empty())
                else {
                    replayable = false;
                    break;
                };
                let Some((result_index, result)) = turn
                    .execution_events
                    .iter()
                    .enumerate()
                    .skip(start_index + 1)
                    .find(|(result_index, event)| {
                        !consumed_result_indices.contains(result_index)
                            && event.event.kind == "tool_result"
                            && event
                                .event
                                .metadata
                                .get("agent_id")
                                .and_then(serde_json::Value::as_str)
                                == Some(agent_id)
                            && event
                                .event
                                .metadata
                                .get("call_id_sha256")
                                .and_then(serde_json::Value::as_str)
                                == Some(call_hash)
                            && event
                                .event
                                .metadata
                                .get("occurrence")
                                .and_then(serde_json::Value::as_u64)
                                == Some(occurrence)
                    })
                else {
                    replayable = false;
                    break;
                };
                consumed_result_indices.insert(result_index);
                if result
                    .event
                    .metadata
                    .get("call_id_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                    || result
                        .event
                        .metadata
                        .get("tool_name_truncated")
                        .and_then(serde_json::Value::as_bool)
                        != Some(false)
                {
                    replayable = false;
                    break;
                }
                if result
                    .event
                    .metadata
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    != Some(tool_name)
                {
                    replayable = false;
                    break;
                }
                if result
                    .event
                    .metadata
                    .get("result_truncated")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
                {
                    replayable = false;
                    break;
                }
                let Some(result_value) = result.event.metadata.get("result") else {
                    replayable = false;
                    break;
                };
                let Ok(arguments_json) = serde_json::to_string(provider_arguments) else {
                    replayable = false;
                    break;
                };
                let Ok(result_content) = serde_json::to_string(result_value) else {
                    replayable = false;
                    break;
                };
                tool_calls.push(crate::StoredToolCall {
                    id: call_id.to_string(),
                    name: tool_name.to_string(),
                    arguments_json,
                    provider_metadata,
                });
                tool_results.push((
                    call_id.to_string(),
                    tool_name.to_string(),
                    result_content,
                    result.recorded_at,
                ));
            }
            if !replayable || tool_calls.len() != tool_results.len() {
                continue;
            }
            let Some(assistant_content) = assistant_content else {
                continue;
            };
            projected.push(crate::StoredMessage {
                content_parts: None,
                role: axocoatl_core::MessageRole::Assistant,
                token_count: checkpoint_token_count(counter, &assistant_content),
                content: assistant_content,
                timestamp: started.recorded_at / 1_000,
                name: None,
                tool_calls,
                tool_call_id: None,
            });
            for (call_id, tool_name, result_content, recorded_at) in tool_results {
                projected.push(crate::StoredMessage {
                    content_parts: None,
                    role: axocoatl_core::MessageRole::Tool,
                    token_count: checkpoint_token_count(counter, &result_content),
                    content: result_content,
                    timestamp: recorded_at / 1_000,
                    name: Some(tool_name),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call_id),
                });
            }
        }

        if turn.agent_outputs.is_empty() {
            if let Some(content) = turn
                .final_output
                .as_ref()
                .or_else(|| (!turn.partial_output.is_empty()).then_some(&turn.partial_output))
            {
                projected.push(crate::StoredMessage {
                    content_parts: None,
                    role: axocoatl_core::MessageRole::Assistant,
                    token_count: checkpoint_token_count(counter, content),
                    content: content.clone(),
                    timestamp: turn.updated_at / 1_000,
                    name: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
        } else {
            for output in turn
                .agent_outputs
                .iter()
                .filter(|output| !output.superseded)
            {
                projected.push(crate::StoredMessage {
                    content_parts: None,
                    role: axocoatl_core::MessageRole::Assistant,
                    token_count: checkpoint_token_count(counter, &output.output),
                    content: output.output.clone(),
                    timestamp: output.recorded_at / 1_000,
                    name: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
            // Once exact per-Agent output records exist they are the only
            // transcript authority. `partial_output` is an append-only stream
            // observation and can include assistant preambles from earlier
            // tool rounds, so byte-length subtraction cannot recover a valid
            // message boundary.
        }
    }
    projected
}

pub struct BoundedHistoryCheckpoint {
    pub checkpoint: crate::AgentCheckpoint,
    pub history_truncated: bool,
    pub behavior_state_dropped: bool,
    /// Exact whole-turn segments selected for this model-facing cache.
    pub retained_turn_ids: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn bounded_history_checkpoint(
    counter: &dyn Fn(&str) -> usize,
    turns: &[SessionTurn],
    version: u64,
    agent_id: String,
    checkpoint_time: u64,
    cumulative_token_usage: axocoatl_core::TokenUsageStats,
    cumulative_token_usage_known: bool,
    behavior_state: Option<String>,
    policy: ToolReplayPolicy,
) -> Result<BoundedHistoryCheckpoint, crate::MemoryError> {
    let mut checkpoint = crate::AgentCheckpoint {
        version,
        agent_id,
        checkpoint_time,
        session_messages: Vec::new(),
        cumulative_token_usage,
        cumulative_token_usage_known,
        behavior_state,
    };
    let mut behavior_state_dropped = false;
    let mut base_size = crate::encoded_checkpoint_size(&checkpoint)?;
    if base_size > crate::MAX_CHECKPOINT_BYTES && checkpoint.behavior_state.is_some() {
        checkpoint.behavior_state = None;
        behavior_state_dropped = true;
        base_size = crate::encoded_checkpoint_size(&checkpoint)?;
    }
    if base_size > crate::MAX_CHECKPOINT_BYTES {
        return Err(crate::MemoryError::Serialization(format!(
            "checkpoint metadata is {base_size} bytes; limit is {}",
            crate::MAX_CHECKPOINT_BYTES
        )));
    }

    let message_budget = crate::MAX_CHECKPOINT_BYTES
        .saturating_sub(base_size)
        .min(SESSION_CHECKPOINT_MESSAGE_CAP);
    let mut segments: VecDeque<Vec<crate::StoredMessage>> = VecDeque::new();
    let mut retained_turn_ids = VecDeque::new();
    let mut segment_bytes = 0_usize;
    let mut history_truncated = false;
    for turn in turns.iter().rev() {
        // Cancelled turns are canonical Route evidence, not provider-visible
        // restart context. Skip them before estimating the message budget so
        // an arbitrarily large stopped turn cannot evict older completed work
        // that would otherwise fit in the bounded recovery tail.
        if turn.status == SessionTurnLifecycle::Cancelled {
            continue;
        }
        if checkpoint_turn_minimum_bytes(turn) > message_budget.saturating_sub(segment_bytes) {
            history_truncated = true;
            break;
        }
        let segment = project_history(counter, std::slice::from_ref(turn), policy);
        if segment.is_empty() {
            continue;
        }
        let encoded = crate::encoded_checkpoint_messages_size(&segment)?;
        if encoded > message_budget.saturating_sub(segment_bytes) {
            history_truncated = true;
            break;
        }
        segment_bytes = segment_bytes.saturating_add(encoded);
        segments.push_front(segment);
        retained_turn_ids.push_front(turn.id.clone());
    }

    checkpoint.session_messages = segments.iter().flatten().cloned().collect();
    let mut encoded_size = crate::encoded_checkpoint_size(&checkpoint)?;
    // Separate segment vector prefixes make `segment_bytes` conservative, but
    // retain an exact fail-safe if the persistence encoding ever changes.
    while encoded_size > crate::MAX_CHECKPOINT_BYTES && segments.pop_front().is_some() {
        retained_turn_ids.pop_front();
        history_truncated = true;
        checkpoint.session_messages = segments.iter().flatten().cloned().collect();
        encoded_size = crate::encoded_checkpoint_size(&checkpoint)?;
    }
    if encoded_size > crate::MAX_CHECKPOINT_BYTES {
        return Err(crate::MemoryError::Serialization(format!(
            "bounded checkpoint is {encoded_size} bytes; limit is {}",
            crate::MAX_CHECKPOINT_BYTES
        )));
    }

    Ok(BoundedHistoryCheckpoint {
        checkpoint,
        history_truncated,
        behavior_state_dropped,
        retained_turn_ids: retained_turn_ids.into_iter().collect(),
    })
}

pub fn checkpoint_visible_matches(
    existing: &[crate::StoredMessage],
    projected: &[crate::StoredMessage],
) -> bool {
    existing.len() == projected.len()
        && existing.iter().zip(projected).all(|(left, right)| {
            left.role == right.role
                && left.content == right.content
                && left.name == right.name
                && left.tool_call_id == right.tool_call_id
                && left.tool_calls.len() == right.tool_calls.len()
                && left
                    .tool_calls
                    .iter()
                    .zip(&right.tool_calls)
                    .all(|(left_call, right_call)| {
                        left_call.id == right_call.id
                            && left_call.name == right_call.name
                            && left_call.arguments_json == right_call.arguments_json
                            && left_call.provider_metadata == right_call.provider_metadata
                    })
        })
}
