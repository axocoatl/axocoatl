//! Explicit conversation rewind retains raw evidence and incurred usage.
//! One journal record selects prior heads and hides exact rows.
use super::*;
use axocoatl_session::session_history::{HistoryVisibility, SessionHistory, SessionHistoryEntry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRewind {
    pub rewind_id: String,
    pub keep_through_turn_id: Option<String>,
    pub superseded_turn_ids: Vec<String>,
    pub(super) after_promotions: usize,
    pub(super) conversations: Vec<RewindConversation>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RewindConversation {
    pub conversation_id: NodeConversationId,
    pub checkpoint: Option<CheckpointRef>,
    pub projection: Option<RewindProjection>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RewindProjection {
    pub retained_turn_ids: Vec<String>,
    pub payload_sha256: String,
    pub payload_bytes: usize,
}

pub(super) fn rewind_id(
    session_id: &SessionId,
    journal: Option<&CanonicalJournal>,
    rewind: &SessionRewind,
) -> Result<String> {
    Ok(format!(
        "rewind:{}",
        digest(&(
            session_id,
            journal,
            &rewind.keep_through_turn_id,
            &rewind.superseded_turn_ids,
            rewind.after_promotions,
            &rewind.conversations
        ))?
    ))
}
pub(super) fn projection_reference(
    session_id: &SessionId,
    journal: Option<&CanonicalJournal>,
    conversation: &NodeConversationId,
    projection: &RewindProjection,
) -> Result<CheckpointRef> {
    let key = digest(&(session_id, journal, conversation, projection))?;
    Ok(CheckpointRef {
        checkpoint_id: CheckpointId::new(format!("rewind-baseline:{key}"))
            .map_err(|error| invalid_error(error.to_string()))?,
        session_id: session_id.clone(),
        conversation_id: conversation.clone(),
        source: CheckpointSource::Committed {
            evidence: EvidenceRef::new(format!("rewind-baseline:{key}"))
                .map_err(|error| invalid_error(error.to_string()))?,
        },
    })
}
impl ActivationStateStore {
    /// Exact imported identities, for a migrated Session before its first Team Apply.
    pub fn legacy_conversations(&self) -> Result<Vec<NodeConversationId>> {
        self.ready()?;
        Ok(self
            .projection
            .baselines
            .iter()
            .map(|baseline| baseline.reference.conversation_id.clone())
            .collect())
    }
    pub fn superseded_turn_ids(&self) -> Result<Vec<String>> {
        self.ready()?;
        let mut result = vec![];
        let mut seen = HashSet::new();
        for rewind in self.rewinds()? {
            for turn in rewind.superseded_turn_ids {
                if seen.insert(turn.clone()) {
                    result.push(turn);
                }
            }
        }
        Ok(result)
    }
    pub fn committed_activation(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<Option<ActivationRef>> {
        let Some(reference) = self.committed_reference(conversation)? else {
            return Ok(None);
        };
        let Some(promotion) = journal::committed_promotion(&reference) else {
            return Ok(None);
        };
        Ok(self
            .promotion_by_id(promotion)?
            .into_iter()
            .flat_map(|manifest| manifest.selected)
            .find(|selected| selected.committed == reference)
            .and_then(|selected| match selected.accepted.source {
                CheckpointSource::Accepted { activation } => Some(activation),
                _ => None,
            }))
    }
    pub fn rewind_session(
        &mut self,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        keep_through: Option<&str>,
        conversations: &[NodeConversationId],
        policy: ToolReplayPolicy,
    ) -> Result<SessionRewind> {
        self.verify_canonical_owner(canonical)?;
        content
            .verify_canonical_owner(canonical)
            .map_err(|error| invalid_error(error.to_string()))?;
        if canonical.unfinished_turn()?.is_some() || self.projection.pending.is_some() {
            return invalid("rewind requires settled canonical work and conversation promotion");
        }
        if conversations.len() > MAX_BASELINES
            || conversations.iter().collect::<HashSet<_>>().len() != conversations.len()
        {
            return invalid("rewind requires exact unique conversation identities");
        }
        let mut history = SessionHistory::from_upgraded(canonical, content)
            .map_err(|error| invalid_error(error.to_string()))?;
        history
            .apply_superseded(&self.superseded_turn_ids()?)
            .map_err(|error| invalid_error(error.to_string()))?;
        for entry in history.entries(HistoryVisibility::IncludingSuperseded) {
            if let SessionHistoryEntry::ExecutionV2(turn) = entry {
                if self
                    .promotion(&canonical.snapshot(&turn.turn_id)?)?
                    .is_none()
                {
                    return invalid(
                        "rewind requires every closed turn's acknowledged conversation promotion",
                    );
                }
            }
        }
        let visible = history.entries(HistoryVisibility::Visible);
        let keep = match keep_through {
            None => 0,
            Some(id) => {
                visible
                    .iter()
                    .position(|entry| entry.turn_id() == id)
                    .ok_or_else(|| invalid_error("rewind target is not a visible Session turn"))?
                    + 1
            }
        };
        let superseded = visible[keep..]
            .iter()
            .map(|entry| entry.turn_id().to_owned())
            .collect::<Vec<_>>();
        let rewinds = self.rewinds()?;
        if superseded.is_empty() {
            if let Some(previous) = rewinds
                .iter()
                .rev()
                .find(|entry| entry.keep_through_turn_id.as_deref() == keep_through)
            {
                return Ok(previous.clone());
            }
        }
        let kept = visible[..keep]
            .iter()
            .map(|entry| entry.turn_id())
            .collect::<HashSet<_>>();
        let legacy = visible[..keep]
            .iter()
            .filter_map(|entry| match entry {
                SessionHistoryEntry::LegacyV1(turn) => Some((**turn).clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut selected = vec![];
        let mut payloads = vec![];
        let journal = self.bound_journal()?;
        let mut kept_selections = self.latest_kept_selections(&kept, conversations)?;
        for conversation in conversations {
            if let Some(reference) = kept_selections.remove(conversation) {
                self.load_reference(&reference)?;
                selected.push(RewindConversation {
                    conversation_id: conversation.clone(),
                    checkpoint: Some(reference),
                    projection: None,
                });
                continue;
            }
            if !legacy.is_empty() {
                if self.projection.baselines.iter().any(|baseline| {
                    &baseline.reference.conversation_id == conversation
                        && baseline
                            .checkpoint_projection
                            .as_ref()
                            .is_some_and(|details| {
                                details.policy
                                    == LegacyActorProjectionPolicy::UnknownRoleHistoryOnly
                            })
                }) {
                    return invalid("This historical Agent role was not recorded. Its history remains readable, but rewind cannot turn its archived state into model context; start future work with a reviewed Team.");
                }
                let current = self.committed_checkpoint(conversation)?;
                let usage = current
                    .as_ref()
                    .map(|checkpoint| checkpoint.cumulative_token_usage.clone())
                    .unwrap_or_default();
                let known = current
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.cumulative_token_usage_known);
                let bounded = bounded_history_checkpoint(
                    &|text| text.len(),
                    &legacy,
                    1,
                    conversation.as_str().into(),
                    0,
                    usage,
                    known,
                    None,
                    policy,
                )
                .map_err(|error| invalid_error(error.to_string()))?;
                let payload = encode_current(&bounded.checkpoint)
                    .map_err(|error| invalid_error(error.to_string()))?;
                let projection = RewindProjection {
                    retained_turn_ids: bounded.retained_turn_ids,
                    payload_sha256: digest_bytes(&payload),
                    payload_bytes: payload.len(),
                };
                let reference = projection_reference(
                    self.session_id(),
                    Some(&journal),
                    conversation,
                    &projection,
                )?;
                payloads.push((reference.clone(), payload));
                selected.push(RewindConversation {
                    conversation_id: conversation.clone(),
                    checkpoint: Some(reference),
                    projection: Some(projection),
                });
            } else {
                selected.push(RewindConversation {
                    conversation_id: conversation.clone(),
                    checkpoint: None,
                    projection: None,
                });
            }
        }
        let mut rewind = SessionRewind {
            rewind_id: String::new(),
            keep_through_turn_id: keep_through.map(str::to_owned),
            superseded_turn_ids: superseded,
            after_promotions: self.projection.promotions,
            conversations: selected,
        };
        rewind.rewind_id = rewind_id(self.session_id(), Some(&journal), &rewind)?;
        if let Some(previous) = rewinds
            .into_iter()
            .find(|entry| entry.rewind_id == rewind.rewind_id)
        {
            return Ok(previous);
        }
        let event = Event::Rewind(rewind.clone());
        let admitted = self.admit(&event, &journal)?;
        self.uncertain = true;
        for (reference, payload) in payloads {
            let name = object_name(&reference);
            if self.objects.is_file(&name)?
                && self.objects.read_limited(&name, MAX_CHECKPOINT_BYTES)? != payload
            {
                return invalid("rewind checkpoint object already has different bytes");
            }
            self.objects.atomic_write(name, &payload)?;
        }
        self.append(event, admitted)?;
        Ok(rewind)
    }
}
