//! Explicit conversation rewind retains raw evidence and incurred usage.
//! One atomic memory-journal write selects prior heads and hides exact rows.
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

pub(super) fn effective_reference<'a>(
    state: &'a StoreState,
    conversation: &NodeConversationId,
    count: usize,
) -> Option<&'a CheckpointRef> {
    let latest = state.rewinds.iter().rev().find_map(|rewind| {
        (rewind.after_promotions <= count)
            .then(|| {
                rewind
                    .conversations
                    .iter()
                    .find(|entry| &entry.conversation_id == conversation)
                    .map(|entry| (rewind.after_promotions, entry))
            })
            .flatten()
    });
    let start = latest.map_or(0, |(start, _)| start);
    let promoted = state.promotions[..count]
        .iter()
        .skip(start)
        .rev()
        .flat_map(|promotion| promotion.selected.iter())
        .find(|entry| &entry.committed.conversation_id == conversation)
        .map(|entry| &entry.committed);
    promoted.or_else(|| match latest {
        Some((_, entry)) => entry.checkpoint.as_ref(),
        None => state
            .baselines
            .iter()
            .find(|baseline| &baseline.reference.conversation_id == conversation)
            .map(|baseline| &baseline.reference),
    })
}
fn rewind_id(state: &StoreState, rewind: &SessionRewind) -> Result<String> {
    Ok(format!(
        "rewind:{}",
        digest(&(
            &state.session_id,
            &state.journal,
            &rewind.keep_through_turn_id,
            &rewind.superseded_turn_ids,
            rewind.after_promotions,
            &rewind.conversations
        ))?
    ))
}
fn projection_reference(
    state: &StoreState,
    conversation: &NodeConversationId,
    projection: &RewindProjection,
) -> Result<CheckpointRef> {
    let key = digest(&(&state.session_id, &state.journal, conversation, projection))?;
    Ok(CheckpointRef {
        checkpoint_id: CheckpointId::new(format!("rewind-baseline:{key}"))
            .map_err(|error| invalid_error(error.to_string()))?,
        session_id: state.session_id.clone(),
        conversation_id: conversation.clone(),
        source: CheckpointSource::Committed {
            evidence: EvidenceRef::new(format!("rewind-baseline:{key}"))
                .map_err(|error| invalid_error(error.to_string()))?,
        },
    })
}
pub(super) fn validate_rewinds(state: &StoreState) -> Result<()> {
    if state.rewinds.len() > MAX_PROMOTIONS {
        return Err(ActivationStateError::Capacity);
    }
    let mut last = 0;
    let mut ids = HashSet::new();
    for rewind in &state.rewinds {
        if rewind.after_promotions < last
            || rewind.after_promotions > state.promotions.len()
            || rewind.rewind_id != rewind_id(state, rewind)?
            || !ids.insert(&rewind.rewind_id)
            || rewind.conversations.len() > MAX_BASELINES
            || rewind.superseded_turn_ids.len() > MAX_INPUTS
            || rewind
                .superseded_turn_ids
                .iter()
                .collect::<HashSet<_>>()
                .len()
                != rewind.superseded_turn_ids.len()
        {
            return invalid("rewind journal identity, ordering or bounds differ");
        }
        last = rewind.after_promotions;
        let mut conversations = HashSet::new();
        for entry in &rewind.conversations {
            if !conversations.insert(&entry.conversation_id) {
                return invalid("rewind repeats a conversation");
            }
            if let Some(reference) = &entry.checkpoint {
                if reference.session_id != state.session_id
                    || reference.conversation_id != entry.conversation_id
                {
                    return invalid("rewind checkpoint belongs to another conversation");
                }
                if let Some(projection) = &entry.projection {
                    if reference
                        != &projection_reference(state, &entry.conversation_id, projection)?
                        || !is_digest(&projection.payload_sha256)
                        || projection.payload_bytes > MAX_CHECKPOINT_BYTES
                        || projection.retained_turn_ids.len() > MAX_BASELINE_TURNS
                    {
                        return invalid("rewind projection differs from its retained source");
                    }
                } else if !state.promotions[..rewind.after_promotions]
                    .iter()
                    .flat_map(|promotion| &promotion.selected)
                    .any(|selected| &selected.committed == reference)
                    && !state
                        .baselines
                        .iter()
                        .any(|baseline| &baseline.reference == reference)
                    && !state
                        .rewinds
                        .iter()
                        .take_while(|prior| prior.rewind_id != rewind.rewind_id)
                        .flat_map(|prior| &prior.conversations)
                        .any(|prior| prior.checkpoint.as_ref() == Some(reference))
                {
                    return invalid("rewind names an uncommitted or future checkpoint");
                }
            } else if entry.projection.is_some() {
                return invalid("empty rewind cannot carry checkpoint payload");
            }
        }
    }
    Ok(())
}
impl ActivationStateStore {
    /// Exact imported identities, for a migrated Session before its first Team Apply.
    pub fn legacy_conversations(&self) -> Result<Vec<NodeConversationId>> {
        self.ready()?;
        Ok(self
            .state
            .baselines
            .iter()
            .map(|baseline| baseline.reference.conversation_id.clone())
            .collect())
    }
    pub fn superseded_turn_ids(&self) -> Result<Vec<String>> {
        self.ready()?;
        let mut result = vec![];
        for rewind in &self.state.rewinds {
            for turn in &rewind.superseded_turn_ids {
                if !result.contains(turn) {
                    result.push(turn.clone());
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
        Ok(self
            .state
            .promotions
            .iter()
            .flat_map(|promotion| &promotion.selected)
            .find(|selected| selected.committed == reference)
            .and_then(|selected| match &selected.accepted.source {
                CheckpointSource::Accepted { activation } => Some(activation.clone()),
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
        if canonical.unfinished_turn()?.is_some() || self.state.pending.is_some() {
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
        if superseded.is_empty() {
            if let Some(previous) = self
                .state
                .rewinds
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
        for conversation in conversations {
            let reference = self
                .state
                .promotions
                .iter()
                .rev()
                .filter(|promotion| kept.contains(promotion.closure.turn_id().as_str()))
                .flat_map(|promotion| &promotion.selected)
                .find(|entry| &entry.committed.conversation_id == conversation)
                .map(|entry| entry.committed.clone());
            if let Some(reference) = reference {
                self.load_reference(&reference)?;
                selected.push(RewindConversation {
                    conversation_id: conversation.clone(),
                    checkpoint: Some(reference),
                    projection: None,
                });
                continue;
            }
            if !legacy.is_empty() {
                if self.state.baselines.iter().any(|baseline| {
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
                let reference = projection_reference(&self.state, conversation, &projection)?;
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
            after_promotions: self.state.promotions.len(),
            conversations: selected,
        };
        rewind.rewind_id = rewind_id(&self.state, &rewind)?;
        if let Some(previous) = self
            .state
            .rewinds
            .iter()
            .find(|entry| entry.rewind_id == rewind.rewind_id)
        {
            return Ok(previous.clone());
        }
        let mut next = self.state.clone();
        next.rewinds.push(rewind.clone());
        self.admit(&next)?;
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
        self.persist(next)?;
        Ok(rewind)
    }
}
