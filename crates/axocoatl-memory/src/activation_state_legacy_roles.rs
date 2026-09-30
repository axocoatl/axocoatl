//! Source-bound per-identity legacy conversation conversion. The source archive
//! is retained before the baseline is published, including private state that
//! must not resume after the execution-format change.

use super::*;
use crate::checkpoint::{CheckpointStore, LegacySessionCheckpointSnapshot};
use std::collections::BTreeSet;
use std::sync::Arc;

const CHECKPOINT_PROJECTION_POLICY: &str = "captured-session-checkpoint-canonical-role-v1";
const MAX_SOURCE_ARCHIVE_BYTES: usize = 132 * 1024 * 1024;
// Each owned namespace object is bounded independently. Preserve the entire
// captured Session archive without requesting or writing an oversized object.
const SOURCE_ARCHIVE_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// These are conversion policies selected from recorded execution ownership,
/// not claims about a historical Agent template inferred from current Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyActorProjectionPolicy {
    /// Preserve the existing autonomous Failed/Interrupted restart policy.
    OrdinaryAutonomous,
    /// Coordinators, team actors and Workers retain completed own outputs only.
    CompletedPerAgent,
    /// The historical role is not recorded. Keep its source and accounting,
    /// but do not turn an unproved execution cache into future model context.
    UnknownRoleHistoryOnly,
}

#[derive(Debug, Clone)]
pub struct LegacyRoleAssignment {
    pub checkpoint_agent_id: String,
    pub recorded_agent_id: String,
    pub slot_id: SessionTeamSlotId,
    pub conversation_id: NodeConversationId,
    pub policy: LegacyActorProjectionPolicy,
    pub tool_replay_policy: ToolReplayPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCheckpointProjectionDetails {
    pub policy: LegacyActorProjectionPolicy,
    pub recorded_agent_id: String,
    pub archive_sha256: String,
    pub archive_bytes: usize,
    pub source_checkpoint_version: u64,
    pub source_checkpoint_sha256: String,
    pub source_message_count: usize,
    pub retained_message_count: usize,
    /// Exact private bytes remain in the archive; none is adopted as runnable state.
    pub private_state_sha256: Option<String>,
    pub private_state_reset: bool,
    pub omitted_turn_ids: Vec<String>,
    pub history_truncated: bool,
    pub tool_replay_policy: ToolReplayPolicy,
    pub source_tool_starts: usize,
    pub retained_tool_calls: usize,
}

impl LegacyBaselineProjection {
    /// Consume a fresh checkpoint capture from the exact standard data-root
    /// child used by the daemon, together with that Session's sealed canonical
    /// History. Every captured actor must be mapped once, even if its only
    /// surviving state is accounting. No usage is added once per generation.
    pub fn from_captured_session(
        content: &ExecutionContentStore,
        seal: &DurableLegacySeal,
        source_store: &CheckpointStore,
        captured: LegacySessionCheckpointSnapshot,
        assignments: &[LegacyRoleAssignment],
        count_text: &dyn Fn(&str) -> usize,
    ) -> Result<Vec<Self>> {
        captured
            .verify_current(source_store)
            .map_err(|error| invalid_error(error.to_string()))?;
        let frontier = content.read_legacy_history(seal)?;
        if captured.session_id() != seal.identity().owner().session_id.as_str()
            || captured.data_root_inode() != Some(frontier.source.root_inode.as_str())
        {
            return invalid(
                "checkpoint capture and sealed history have different Session/data-root ownership",
            );
        }
        if frontier.turns.len() > MAX_BASELINE_TURNS
            || frontier.turns.iter().any(|turn| !turn.status.is_terminal())
        {
            return invalid("legacy role conversion requires bounded terminal canonical history");
        }
        let captured_ids: BTreeSet<_> = captured
            .checkpoints()
            .map(|item| item.agent_id.as_str())
            .collect();
        let mut mapped_ids = BTreeSet::new();
        let mut recorded_ids = BTreeSet::new();
        let mut slots = HashSet::new();
        let mut conversations = HashSet::new();
        let session_prefix = format!("{}:", captured.session_id());
        for assignment in assignments {
            if !assignment.checkpoint_agent_id.starts_with(&session_prefix)
                || assignment.checkpoint_agent_id.len() > 256
                || assignment.recorded_agent_id.is_empty()
                || assignment.recorded_agent_id.len() > 256
                || !mapped_ids.insert(assignment.checkpoint_agent_id.as_str())
                || !recorded_ids.insert(assignment.recorded_agent_id.as_str())
                || !slots.insert(&assignment.slot_id)
                || !conversations.insert(&assignment.conversation_id)
                || !valid_recorded_identity(captured.session_id(), assignment)
            {
                return invalid(
                    "legacy role mapping is ambiguous, duplicated or lacks exact scoped identity",
                );
            }
        }
        if mapped_ids != captured_ids {
            return invalid(
                "legacy role mapping must preserve every captured actor's accounting exactly once",
            );
        }
        let history_ids: BTreeSet<_> = frontier
            .turns
            .iter()
            .flat_map(|turn| {
                turn.agent_id.iter().map(String::as_str).chain(
                    turn.agent_outputs
                        .iter()
                        .filter(|output| output.attempt_id.is_none())
                        .map(|output| output.agent_id.as_str()),
                )
            })
            .collect();
        if !history_ids.is_subset(&recorded_ids) {
            return invalid(
                "canonical legacy actor has no exact checkpoint/accounting source mapping",
            );
        }
        let archive_sha256 = captured.source_sha256().to_owned();
        let archive_bytes = captured.archive_bytes().len();
        if archive_bytes > MAX_SOURCE_ARCHIVE_BYTES {
            return Err(ActivationStateError::Capacity);
        }
        let mut projections = Vec::with_capacity(assignments.len());
        for assignment in assignments {
            let source = captured
                .checkpoint(&assignment.checkpoint_agent_id)
                .ok_or_else(|| invalid_error("legacy actor checkpoint is missing"))?;
            let (
                checkpoint,
                visible_turns,
                omitted_turn_ids,
                history_truncated,
                source_tool_starts,
            ) = project_actor(&frontier.turns, source, assignment, count_text)?;
            let details = LegacyCheckpointProjectionDetails {
                policy: assignment.policy,
                recorded_agent_id: assignment.recorded_agent_id.clone(),
                archive_sha256: archive_sha256.clone(),
                archive_bytes,
                source_checkpoint_version: source.version,
                source_checkpoint_sha256: digest_bytes(
                    &encode_current(source).map_err(|error| invalid_error(error.to_string()))?,
                ),
                source_message_count: source.session_messages.len(),
                retained_message_count: checkpoint.session_messages.len(),
                private_state_sha256: source
                    .behavior_state
                    .as_ref()
                    .map(|state| digest_bytes(state.as_bytes())),
                private_state_reset: source.behavior_state.is_some(),
                omitted_turn_ids,
                history_truncated,
                tool_replay_policy: assignment.tool_replay_policy,
                source_tool_starts,
                retained_tool_calls: checkpoint
                    .session_messages
                    .iter()
                    .map(|message| message.tool_calls.len())
                    .sum(),
            };
            let mut projection = Self::encode_projection(
                seal,
                assignment.slot_id.clone(),
                checkpoint,
                assignment.checkpoint_agent_id.clone(),
                visible_turns,
                None,
            )?;
            projection.record.policy = CHECKPOINT_PROJECTION_POLICY.to_owned();
            projection.record.checkpoint_projection = Some(details);
            projection.record.reference =
                baseline_reference(&canonical_journal(seal.identity()), &projection.record)?;
            projections.push(projection);
        }
        let archive = Arc::new(captured);
        for projection in &mut projections {
            projection.source_archive = Some(archive.clone());
        }
        Ok(projections)
    }
}

fn valid_recorded_identity(session: &str, assignment: &LegacyRoleAssignment) -> bool {
    assignment.checkpoint_agent_id == format!("{session}:{}", assignment.recorded_agent_id)
        || assignment
            .checkpoint_agent_id
            .strip_prefix(&format!("{session}:"))
            .and_then(|rest| rest.rsplit_once(":worker:"))
            .is_some_and(|(coordinator, worker)| {
                !coordinator.is_empty() && worker == assignment.recorded_agent_id
            })
}

type ProjectedActor = (AgentCheckpoint, Vec<String>, Vec<String>, bool, usize);

fn project_actor(
    turns: &[SessionTurn],
    source: &AgentCheckpoint,
    assignment: &LegacyRoleAssignment,
    count_text: &dyn Fn(&str) -> usize,
) -> Result<ProjectedActor> {
    if assignment.policy == LegacyActorProjectionPolicy::UnknownRoleHistoryOnly {
        return accounting_only(source, assignment, turns);
    }
    if assignment.policy == LegacyActorProjectionPolicy::OrdinaryAutonomous {
        let selected: Vec<_> = turns
            .iter()
            .filter(|turn| turn.agent_id.as_deref() == Some(assignment.recorded_agent_id.as_str()))
            .cloned()
            .collect();
        if selected.is_empty() {
            if !source.session_messages.is_empty() {
                return invalid("ordinary checkpoint conversation has no canonical history binding; import its legacy history before conversion");
            }
            return accounting_only(source, assignment, turns);
        }
        let (mut checkpoint, _, visible, details) = project_ordinary_legacy(
            &selected,
            &assignment.conversation_id,
            assignment.tool_replay_policy,
            count_text,
        )?;
        if checkpoint.cumulative_token_usage.input_tokens
            > source.cumulative_token_usage.input_tokens
            || checkpoint.cumulative_token_usage.output_tokens
                > source.cumulative_token_usage.output_tokens
            || checkpoint
                .cumulative_token_usage
                .reasoning_tokens
                .unwrap_or(0)
                > source.cumulative_token_usage.reasoning_tokens.unwrap_or(0)
        {
            return invalid("checkpoint accounting is behind its canonical autonomous history");
        }
        // Cumulative checkpoint accounting includes earlier failed generations
        // and checkpoint-only usage. The canonical subtotal is not added again.
        checkpoint.cumulative_token_usage = source.cumulative_token_usage.clone();
        checkpoint.cumulative_token_usage_known = source.cumulative_token_usage_known;
        checkpoint.behavior_state = None;
        let visible_ids: HashSet<_> = visible.iter().map(String::as_str).collect();
        let omitted = turns
            .iter()
            .filter(|turn| !visible_ids.contains(turn.id.as_str()))
            .map(|turn| turn.id.clone())
            .collect();
        return Ok((
            checkpoint,
            visible,
            omitted,
            details.history_truncated,
            details.source_tool_starts,
        ));
    }
    let mut selected = Vec::new();
    let mut source_tool_starts = 0;
    for turn in turns
        .iter()
        .filter(|turn| !turn.superseded && turn.status == SessionTurnLifecycle::Completed)
    {
        let outputs: Vec<_> = turn
            .agent_outputs
            .iter()
            .filter(|output| {
                output.agent_id == assignment.recorded_agent_id
                    && !output.superseded
                    && output.attempt_id.is_none()
            })
            .cloned()
            .collect();
        if outputs.len() > 1 {
            return invalid(
                "multiple current outputs cannot establish one legacy actor conversation",
            );
        }
        let selected_generation = outputs
            .first()
            .and_then(|output| output.activation_generation);
        let has_own_final = outputs.is_empty()
            && turn.agent_outputs.is_empty()
            && turn.agent_id.as_deref() == Some(assignment.recorded_agent_id.as_str())
            && turn.final_output.is_some();
        if outputs.is_empty() && !has_own_final {
            continue;
        }
        for context in &turn.context {
            let required =
                match context.kind.as_str() {
                    "code_selection" => "content",
                    "browser_selection" => "html",
                    _ => return invalid(
                        "legacy actor context requires a separately retained attachment projection",
                    ),
                };
            if !context
                .metadata
                .get(required)
                .is_some_and(serde_json::Value::is_string)
            {
                return invalid("legacy actor inline context is incomplete");
            }
        }
        let mut own = turn.clone();
        own.user_input = legacy_graph_input(turn, selected_generation)?;
        own.agent_outputs = outputs;
        own.partial_output.clear();
        if !has_own_final {
            own.final_output = None;
        }
        own.execution_events.retain(|record| {
            let event = &record.event;
            let own_identity = event
                .metadata
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                == Some(assignment.recorded_agent_id.as_str());
            let own_generation = match selected_generation {
                Some(generation) => {
                    event
                        .metadata
                        .get("generation")
                        .and_then(serde_json::Value::as_u64)
                        == Some(u64::from(generation))
                }
                None => !event.metadata.contains_key("generation"),
            };
            own_identity
                && own_generation
                && event.attempt_id.is_none()
                && matches!(event.kind.as_str(), "tool_started" | "tool_result")
        });
        source_tool_starts += own
            .execution_events
            .iter()
            .filter(|record| record.event.kind == "tool_started")
            .count();
        selected.push(own);
    }
    if selected.is_empty() && !source.session_messages.is_empty() {
        return invalid("legacy role checkpoint conversation has no completed own canonical evidence; migration cannot discard unbound conversation");
    }
    let bounded = bounded_history_checkpoint(
        count_text,
        &selected,
        1,
        assignment.conversation_id.as_str().to_owned(),
        source.checkpoint_time,
        source.cumulative_token_usage.clone(),
        source.cumulative_token_usage_known,
        None,
        assignment.tool_replay_policy,
    )
    .map_err(|error| invalid_error(error.to_string()))?;
    let visible: HashSet<_> = bounded
        .retained_turn_ids
        .iter()
        .map(String::as_str)
        .collect();
    let omitted = turns
        .iter()
        .filter(|turn| !visible.contains(turn.id.as_str()))
        .map(|turn| turn.id.clone())
        .collect();
    Ok((
        bounded.checkpoint,
        bounded.retained_turn_ids,
        omitted,
        bounded.history_truncated,
        source_tool_starts,
    ))
}

/// Only an ordinary turn has provable semantic input. Coordinated multi-Agent
/// turns recorded by 1.1.0 development builds carry graph handoffs whose
/// causal authority is no longer reconstructed, so they cannot become
/// runnable context.
fn legacy_graph_input(turn: &SessionTurn, generation: Option<u32>) -> Result<String> {
    let planned = turn
        .execution_events
        .iter()
        .any(|record| record.event.kind == "coordination_planned");
    if !planned && generation.is_none() {
        return Ok(turn.user_input.clone());
    }
    invalid("coordinated legacy history cannot become runnable Agent context")
}

fn accounting_only(
    source: &AgentCheckpoint,
    assignment: &LegacyRoleAssignment,
    turns: &[SessionTurn],
) -> Result<ProjectedActor> {
    Ok((
        AgentCheckpoint {
            version: 1,
            agent_id: assignment.conversation_id.as_str().to_owned(),
            checkpoint_time: source.checkpoint_time,
            session_messages: Vec::new(),
            cumulative_token_usage: source.cumulative_token_usage.clone(),
            cumulative_token_usage_known: source.cumulative_token_usage_known,
            behavior_state: None,
        },
        Vec::new(),
        turns.iter().map(|turn| turn.id.clone()).collect(),
        false,
        0,
    ))
}

impl ActivationStateStore {
    pub(super) fn retain_legacy_checkpoint_archive(
        &self,
        projection: &LegacyBaselineProjection,
    ) -> Result<()> {
        let Some(details) = &projection.record.checkpoint_projection else {
            return Ok(());
        };
        let source = projection
            .source_archive
            .as_ref()
            .ok_or_else(|| invalid_error("captured legacy archive is missing"))?;
        source
            .verify_captured_source()
            .map_err(|error| invalid_error(error.to_string()))?;
        let archive = source.archive_bytes();
        if archive.len() != details.archive_bytes || digest_bytes(archive) != details.archive_sha256
        {
            return invalid("captured legacy archive does not match its immutable baseline");
        }
        for (index, chunk) in archive.chunks(SOURCE_ARCHIVE_CHUNK_BYTES).enumerate() {
            let name = archive_chunk_name(&details.archive_sha256, index);
            if self.objects.is_file(&name)? {
                if self.objects.read_limited(&name, chunk.len())? != chunk {
                    return invalid("legacy archive already has different bytes");
                }
            } else {
                self.objects.atomic_write(&name, chunk)?;
            }
        }
        Ok(())
    }

    pub(super) fn verify_legacy_checkpoint_archive(&self, baseline: &BaselineRecord) -> Result<()> {
        let Some(details) = &baseline.checkpoint_projection else {
            return Ok(());
        };
        self.read_legacy_checkpoint_archive(details).map(|_| ())
    }

    fn read_legacy_checkpoint_archive(
        &self,
        details: &LegacyCheckpointProjectionDetails,
    ) -> Result<Vec<u8>> {
        if details.archive_bytes == 0 || details.archive_bytes > MAX_SOURCE_ARCHIVE_BYTES {
            return invalid("retained legacy checkpoint archive has an invalid size");
        }
        let mut archive = Vec::with_capacity(details.archive_bytes);
        let mut index = 0;
        while archive.len() < details.archive_bytes {
            let expected = (details.archive_bytes - archive.len()).min(SOURCE_ARCHIVE_CHUNK_BYTES);
            let chunk = self
                .objects
                .read_limited(archive_chunk_name(&details.archive_sha256, index), expected)?;
            if chunk.len() != expected {
                return invalid("retained legacy checkpoint archive chunk has changed size");
            }
            archive.extend_from_slice(&chunk);
            index += 1;
        }
        if archive.len() != details.archive_bytes
            || digest_bytes(&archive) != details.archive_sha256
        {
            return invalid("retained legacy checkpoint archive is missing or changed");
        }
        Ok(archive)
    }

    /// Retained source access remains Session-owned and independent of later
    /// generation promotion. It never reads a caller-supplied filesystem path.
    pub fn legacy_checkpoint_source_archive(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<Option<Vec<u8>>> {
        self.ready()?;
        let Some(baseline) = self
            .state
            .baselines
            .iter()
            .find(|item| item.reference.conversation_id == *conversation)
        else {
            return Ok(None);
        };
        baseline
            .checkpoint_projection
            .as_ref()
            .map(|details| self.read_legacy_checkpoint_archive(details))
            .transpose()
    }
}

pub(super) fn valid_checkpoint_baseline_policy(record: &BaselineRecord) -> bool {
    let Some(details) = &record.checkpoint_projection else {
        return false;
    };
    let mut ids: HashSet<_> = record.visible_turns.iter().map(String::as_str).collect();
    record.policy == CHECKPOINT_PROJECTION_POLICY
        && record.projection.is_none()
        && is_digest(&details.archive_sha256)
        && is_digest(&details.source_checkpoint_sha256)
        && details.archive_bytes <= MAX_SOURCE_ARCHIVE_BYTES
        && details.archive_bytes > 0
        && !details.recorded_agent_id.is_empty()
        && details.recorded_agent_id.len() <= 256
        && details.private_state_reset == details.private_state_sha256.is_some()
        && details
            .private_state_sha256
            .as_ref()
            .is_none_or(|hash| is_digest(hash))
        && details.retained_tool_calls <= details.source_tool_starts
        && (details.policy != LegacyActorProjectionPolicy::UnknownRoleHistoryOnly
            || (record.visible_turns.is_empty()
                && details.retained_message_count == 0
                && details.retained_tool_calls == 0
                && details.tool_replay_policy == ToolReplayPolicy::OmitNativeGroups))
        && (details.tool_replay_policy != ToolReplayPolicy::OmitNativeGroups
            || details.retained_tool_calls == 0)
        && ids.len().saturating_add(details.omitted_turn_ids.len()) <= MAX_BASELINE_TURNS
        && details
            .omitted_turn_ids
            .iter()
            .all(|id| !id.is_empty() && ids.insert(id))
}

fn archive_name(sha256: &str) -> String {
    format!("legacy-checkpoints-{sha256}.bin")
}

fn archive_chunk_name(sha256: &str, index: usize) -> String {
    if index == 0 {
        archive_name(sha256)
    } else {
        format!("legacy-checkpoints-{sha256}.part-{index:04}.bin")
    }
}

#[cfg(all(test, unix))]
#[path = "activation_state_legacy_roles_tests.rs"]
mod tests;
