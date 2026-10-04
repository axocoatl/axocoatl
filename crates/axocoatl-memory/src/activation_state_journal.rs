//! The activation-state journal. Inputs, candidate reservations, candidates,
//! promotion reservations and decisions, rewinds and legacy baselines are
//! records in an append-only segmented log beside a small head file, which
//! keeps only the store's identity and the log's marker.
//!
//! Memory stays bounded: the store keeps the active segment's records, one
//! summary per node conversation (owning slot, materialized head, current
//! committed checkpoint), the legacy baselines (bounded by migration), a key
//! filter per sealed segment and a few decoded sealed segments. Every other
//! record is read back from its sealed segment when a lookup needs it.
//!
//! Opening verifies every record by itself and replays the summaries. Every
//! record was checked against the whole history before it was appended, and
//! a sealed segment cannot change without breaking its digest chain; the
//! active segment has no digest yet, so its records are checked against the
//! history again on every open.

use std::collections::HashMap;
use std::sync::Arc;

use axocoatl_session::segment_log::{
    KeyFilter, SegmentCache, SegmentError, SegmentLog, SegmentSpec, SegmentsMarker,
};
use axocoatl_session::turn_contract::{ActivationId, InputManifestId};
use serde::de::IgnoredAny;

use super::rewind::{projection_reference, rewind_id};
use super::*;

/// No record may be longer than the whole single-file state could be.
pub(super) const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub(super) const LOG_SPEC: SegmentSpec = SegmentSpec {
    name: "activation-state",
    kind: "activation-state",
    segment_bytes: 4 * 1024 * 1024,
    segment_records: 4096,
    record_bytes: MAX_RECORD_BYTES,
};
const CACHED_SEGMENTS: usize = 4;
/// Every key goes into a segment's filter twice, the second time with this
/// suffix, and a lookup needs both: it costs twice the bits and squares the
/// chance of reading a segment that does not hold the key.
const FILTER_SALT: &str = "\u{1f}2";

impl From<SegmentError> for ActivationStateError {
    fn from(error: SegmentError) -> Self {
        match error {
            SegmentError::Io(error) => Self::Io(error),
            SegmentError::Json(error) => Self::Json(error),
            SegmentError::RecordTooLarge => Self::Capacity,
            SegmentError::RecoveryRequired => Self::RecoveryRequired,
            other => Self::Invalid(other.to_string()),
        }
    }
}

/// The primary file of a segmented store. Its `segments` marker also makes
/// a daemon that knows only the single-file layout refuse the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoreHead {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub journal: Option<CanonicalJournal>,
    pub segments: SegmentsMarker,
}

/// One journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Event {
    Baseline(BaselineRecord),
    Input(InputRecord),
    CandidateReserved(CandidateReservation),
    Candidate(Candidate),
    PromotionReserved {
        turn_id: LogicalTurnId,
    },
    /// The durable promotion decision, written before any conversation
    /// pointer moves. Nothing but its completion may follow it.
    PromotionPrepared(PromotionManifest),
    /// Every pointer of the decision just before it is materialized.
    PromotionFinished {
        promotion_id: String,
    },
    Rewind(SessionRewind),
}

/// What a record is found by.
#[derive(Debug, Clone, Copy)]
pub(super) enum Key<'a> {
    /// Inputs, candidate reservations and candidates of one activation.
    Activation(&'a ActivationId),
    Manifest(&'a InputManifestId),
    /// Inputs, promotion reservations and promotion decisions of one turn.
    Turn(&'a LogicalTurnId),
    Promotion(&'a str),
    Rewinds,
}

impl Key<'_> {
    fn text(&self) -> String {
        match self {
            Self::Activation(id) => format!("a:{}", id.as_str()),
            Self::Manifest(id) => format!("m:{}", id.as_str()),
            Self::Turn(id) => format!("t:{}", id.as_str()),
            Self::Promotion(id) => format!("p:{id}"),
            Self::Rewinds => "w".into(),
        }
    }
}

fn producer(reference: &CheckpointRef) -> Option<&ActivationRef> {
    match &reference.source {
        CheckpointSource::Accepted { activation } => Some(activation),
        CheckpointSource::Committed { .. } => None,
    }
}

impl Event {
    fn keys(&self) -> Vec<String> {
        let keys: Vec<Key<'_>> = match self {
            Self::Baseline(_) | Self::PromotionFinished { .. } => vec![],
            Self::Input(record) => vec![
                Key::Activation(&record.input.activation.activation_id),
                Key::Manifest(&record.input.manifest_id),
                Key::Turn(&record.input.activation.turn_id),
            ],
            Self::CandidateReserved(record) => {
                vec![Key::Activation(&record.activation.activation_id)]
            }
            Self::Candidate(candidate) => producer(&candidate.reference)
                .map(|activation| Key::Activation(&activation.activation_id))
                .into_iter()
                .collect(),
            Self::PromotionReserved { turn_id } => vec![Key::Turn(turn_id)],
            Self::PromotionPrepared(manifest) => vec![
                Key::Turn(manifest.closure.turn_id()),
                Key::Promotion(&manifest.promotion_id),
            ],
            Self::Rewind(_) => vec![Key::Rewinds],
        };
        keys.iter().map(Key::text).collect()
    }

    fn has(&self, key: Key<'_>) -> bool {
        match (self, key) {
            (Self::Input(record), Key::Activation(id)) => {
                record.input.activation.activation_id == *id
            }
            (Self::Input(record), Key::Manifest(id)) => record.input.manifest_id == *id,
            (Self::Input(record), Key::Turn(id)) => record.input.activation.turn_id == *id,
            (Self::CandidateReserved(record), Key::Activation(id)) => {
                record.activation.activation_id == *id
            }
            (Self::Candidate(candidate), Key::Activation(id)) => producer(&candidate.reference)
                .is_some_and(|activation| activation.activation_id == *id),
            (Self::PromotionReserved { turn_id }, Key::Turn(id)) => turn_id == id,
            (Self::PromotionPrepared(manifest), Key::Turn(id)) => manifest.closure.turn_id() == id,
            (Self::PromotionPrepared(manifest), Key::Promotion(id)) => manifest.promotion_id == id,
            (Self::Rewind(_), Key::Rewinds) => true,
            _ => false,
        }
    }
}

/// What the store keeps about one sealed segment.
#[derive(Debug)]
pub(super) struct SealedSummary {
    filter: KeyFilter,
}

impl SealedSummary {
    fn of<'a>(events: impl IntoIterator<Item = &'a Arc<Event>>) -> Self {
        let keys: Vec<String> = events
            .into_iter()
            .flat_map(|event| event.keys())
            .flat_map(|key| {
                let salted = format!("{key}{FILTER_SALT}");
                [key, salted]
            })
            .collect();
        Self {
            filter: KeyFilter::new(keys.iter().map(String::as_str)),
        }
    }

    fn may_hold(&self, key: &str) -> bool {
        self.filter.may_contain(key) && self.filter.may_contain(&format!("{key}{FILTER_SALT}"))
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.filter.bytes()
    }
}

/// The bounded summary of one node conversation.
#[derive(Debug, Clone, Default)]
pub(super) struct ConversationState {
    /// The one team slot that owns it, from its inputs or legacy baseline.
    pub slot_id: Option<SessionTeamSlotId>,
    /// The latest promoted selection, which its materialized pointer holds.
    pub head: Option<PromotedConversation>,
    /// The latest promotion or rewind decision. Before any, its committed
    /// checkpoint is its legacy baseline, if it has one.
    pub decided: Option<Option<CheckpointRef>>,
}

/// Everything replay derives and live operations need, in bounded memory.
#[derive(Debug, Clone, Default)]
pub(super) struct Projection {
    pub baselines: Arc<Vec<BaselineRecord>>,
    pub conversations: HashMap<NodeConversationId, ConversationState>,
    /// Completed promotions.
    pub promotions: usize,
    pub pending: Option<PromotionManifest>,
    /// Any input, candidate or promotion record exists.
    pub v2: bool,
}

impl Projection {
    /// The current committed checkpoint of a conversation.
    pub fn effective(&self, conversation: &NodeConversationId) -> Option<CheckpointRef> {
        match self
            .conversations
            .get(conversation)
            .and_then(|state| state.decided.clone())
        {
            Some(decided) => decided,
            None => self
                .baseline(conversation)
                .map(|baseline| baseline.reference.clone()),
        }
    }

    pub fn baseline(&self, conversation: &NodeConversationId) -> Option<&BaselineRecord> {
        self.baselines
            .iter()
            .find(|baseline| baseline.reference.conversation_id == *conversation)
    }

    fn bind(&mut self, slot: &SessionTeamSlotId, conversation: &NodeConversationId) -> Result<()> {
        let state = self.conversations.entry(conversation.clone()).or_default();
        match &state.slot_id {
            Some(owner) if owner != slot => {
                invalid("Session conversation identity is shared by different slots")
            }
            Some(_) => Ok(()),
            None => {
                state.slot_id = Some(slot.clone());
                Ok(())
            }
        }
    }

    /// Apply one record in order. A record that cannot follow the ones
    /// before it is refused.
    pub fn apply(&mut self, event: &Event) -> Result<()> {
        if self.pending.is_some() && !matches!(event, Event::PromotionFinished { .. }) {
            return invalid("a promotion decision must be completed before any other record");
        }
        match event {
            Event::Baseline(record) => {
                if self.baselines.len() >= MAX_BASELINES {
                    return Err(ActivationStateError::Capacity);
                }
                if self.baselines.iter().any(|existing| {
                    existing.slot_id == record.slot_id
                        || existing.reference.conversation_id == record.reference.conversation_id
                }) {
                    return invalid("legacy baseline identity, ownership, or projection mismatch");
                }
                self.bind(&record.slot_id, &record.reference.conversation_id)?;
                let baselines = Arc::make_mut(&mut self.baselines);
                baselines.push(record.clone());
                baselines.sort_by(|a, b| {
                    a.reference
                        .conversation_id
                        .as_str()
                        .cmp(b.reference.conversation_id.as_str())
                });
            }
            Event::Input(record) => {
                self.v2 = true;
                self.bind(&record.slot_id, &record.input.conversation_id)?;
            }
            Event::CandidateReserved(_) | Event::Candidate(_) | Event::PromotionReserved { .. } => {
                self.v2 = true;
            }
            Event::PromotionPrepared(manifest) => {
                self.v2 = true;
                for selected in &manifest.selected {
                    if self.effective(&selected.committed.conversation_id)
                        != selected.previous_committed
                    {
                        return invalid(
                            "promotion does not follow the prior committed conversation",
                        );
                    }
                }
                self.pending = Some(manifest.clone());
            }
            Event::PromotionFinished { promotion_id } => {
                let Some(manifest) = self
                    .pending
                    .take_if(|manifest| &manifest.promotion_id == promotion_id)
                else {
                    return invalid("promotion completion has no matching durable decision");
                };
                for selected in manifest.selected {
                    let state = self
                        .conversations
                        .entry(selected.committed.conversation_id.clone())
                        .or_default();
                    state.decided = Some(Some(selected.committed.clone()));
                    state.head = Some(selected);
                }
                self.promotions += 1;
            }
            Event::Rewind(rewind) => {
                if rewind.after_promotions != self.promotions {
                    return invalid("rewind journal identity, ordering or bounds differ");
                }
                for entry in &rewind.conversations {
                    self.conversations
                        .entry(entry.conversation_id.clone())
                        .or_default()
                        .decided = Some(entry.checkpoint.clone());
                }
            }
        }
        Ok(())
    }
}

pub(super) struct Identity<'a> {
    pub session_id: &'a SessionId,
    pub journal: Option<&'a CanonicalJournal>,
}

/// Check what one record proves about itself: ownership, identities derived
/// from its content, digests and per-record bounds.
pub(super) fn check_record(event: &Event, identity: &Identity<'_>) -> Result<()> {
    let session = identity.session_id;
    let journal = identity.journal.ok_or_else(|| {
        invalid_error("populated artifact namespace has no canonical journal binding")
    })?;
    match event {
        Event::Baseline(baseline) => {
            if baseline.reference.session_id != *session
                || baseline.reference != baseline_reference(journal, baseline)?
                || !valid_baseline_policy(baseline)
                || baseline.original_agent_id.is_empty()
                || baseline.original_agent_id.len() > 256
                || baseline.visible_turns.len() > MAX_BASELINE_TURNS
                || baseline.visible_turns.iter().collect::<HashSet<_>>().len()
                    != baseline.visible_turns.len()
                || !is_digest(&baseline.payload_sha256)
                || baseline.payload_bytes > MAX_CHECKPOINT_BYTES
            {
                return invalid("legacy baseline identity, ownership, or projection mismatch");
            }
        }
        Event::Input(record) => {
            if record.input.activation.session_id != *session
                || record.sha256 != digest(&(&record.slot_id, &record.input))?
            {
                return invalid("input identity, ownership, or digest mismatch");
            }
        }
        Event::CandidateReserved(reservation) => {
            if reservation.activation.session_id != *session
                || reservation.max_checkpoint_bytes != MAX_CHECKPOINT_BYTES
                || !is_digest(&reservation.input_sha256)
            {
                return invalid(
                    "candidate reservation identity, capacity, or single settlement mismatch",
                );
            }
        }
        Event::Candidate(candidate) => {
            let activation = producer(&candidate.reference)
                .ok_or_else(|| invalid_error("candidate has non-activation source"))?;
            let key = digest(&(
                journal,
                activation,
                &candidate.reference.conversation_id,
                &candidate.input_sha256,
                &candidate.payload_sha256,
                candidate.payload_bytes,
            ))?;
            if candidate.reference.session_id != *session
                || candidate.reference.checkpoint_id.as_str() != format!("checkpoint:{key}")
                || candidate.payload_bytes > MAX_CHECKPOINT_BYTES
                || !is_digest(&candidate.payload_sha256)
                || !is_digest(&candidate.input_sha256)
            {
                return invalid("candidate identity, ownership, or digest mismatch");
            }
        }
        Event::PromotionReserved { .. } => {}
        Event::PromotionPrepared(manifest) => check_promotion(journal, session, manifest)?,
        Event::PromotionFinished { promotion_id } => {
            if !is_digest(promotion_id) {
                return invalid("promotion identity or closure mismatch");
            }
        }
        Event::Rewind(rewind) => check_rewind(session, journal, rewind)?,
    }
    Ok(())
}

pub(super) fn check_promotion(
    journal: &CanonicalJournal,
    session: &SessionId,
    manifest: &PromotionManifest,
) -> Result<()> {
    if manifest.journal_id != journal.journal_id
        || manifest.workspace_id != journal.workspace_id
        || manifest.closure.session_id() != session
        || manifest.closure.closure_revision() == 0
        || !is_digest(&manifest.contract_sha256)
        || manifest.promotion_id
            != promotion_id(
                journal,
                &manifest.closure,
                &manifest.contract_sha256,
                &manifest.selected,
            )?
    {
        return invalid("promotion identity or closure mismatch");
    }
    let mut nodes = HashSet::new();
    let mut conversations = HashSet::new();
    for entry in &manifest.selected {
        let activation = producer(&entry.accepted)
            .ok_or_else(|| invalid_error("promotion lacks exact activation source"))?;
        if activation.turn_id != *manifest.closure.turn_id()
            || activation.node_id != entry.node_id
            || !nodes.insert(&entry.node_id)
            || !conversations.insert(&entry.accepted.conversation_id)
            || entry.committed != committed_ref(&manifest.promotion_id, &entry.accepted)?
        {
            return invalid("promotion selects a foreign or missing checkpoint");
        }
    }
    Ok(())
}

fn check_rewind(
    session: &SessionId,
    journal: &CanonicalJournal,
    rewind: &SessionRewind,
) -> Result<()> {
    if rewind.rewind_id != rewind_id(session, Some(journal), rewind)?
        || rewind.conversations.len() > MAX_BASELINES
        || rewind
            .superseded_turn_ids
            .iter()
            .collect::<HashSet<_>>()
            .len()
            != rewind.superseded_turn_ids.len()
    {
        return invalid("rewind journal identity, ordering or bounds differ");
    }
    let mut conversations = HashSet::new();
    for entry in &rewind.conversations {
        if !conversations.insert(&entry.conversation_id) {
            return invalid("rewind repeats a conversation");
        }
        match (&entry.checkpoint, &entry.projection) {
            (Some(reference), projection) => {
                if reference.session_id != *session
                    || reference.conversation_id != entry.conversation_id
                {
                    return invalid("rewind checkpoint belongs to another conversation");
                }
                if let Some(projection) = projection {
                    if reference
                        != &projection_reference(
                            session,
                            Some(journal),
                            &entry.conversation_id,
                            projection,
                        )?
                        || !is_digest(&projection.payload_sha256)
                        || projection.payload_bytes > MAX_CHECKPOINT_BYTES
                        || projection.retained_turn_ids.len() > MAX_BASELINE_TURNS
                    {
                        return invalid("rewind projection differs from its retained source");
                    }
                }
            }
            (None, Some(_)) => return invalid("empty rewind cannot carry checkpoint payload"),
            (None, None) => {}
        }
    }
    Ok(())
}

/// Records indexed under a key, below a sequence, in log order.
pub(super) trait RecordSource {
    fn find(&self, key: Key<'_>, before: u64) -> Result<Vec<Arc<Event>>>;
}

/// Check a record against every record before it: references must name
/// existing records and identities must stay unique across the whole
/// history, whichever segment holds them.
pub(super) fn check_history(
    event: &Event,
    before: u64,
    source: &dyn RecordSource,
    baselines: &[BaselineRecord],
) -> Result<()> {
    match event {
        Event::Baseline(_) | Event::PromotionFinished { .. } => {}
        Event::Input(record) => {
            let input = &record.input;
            if source
                .find(Key::Activation(&input.activation.activation_id), before)?
                .iter()
                .chain(&source.find(Key::Manifest(&input.manifest_id), before)?)
                .any(|event| matches!(&**event, Event::Input(_)))
            {
                return invalid("input identity, ownership, or digest mismatch");
            }
        }
        Event::CandidateReserved(reservation) => {
            let records = source.find(
                Key::Activation(&reservation.activation.activation_id),
                before,
            )?;
            let input = records.iter().find_map(|event| match &**event {
                Event::Input(record) if record.input.activation == reservation.activation => {
                    Some(record)
                }
                _ => None,
            });
            let Some(input) = input else {
                return invalid("candidate reservation has no immutable input");
            };
            if reservation.input_sha256 != input.sha256
                || records
                    .iter()
                    .any(|event| matches!(&**event, Event::CandidateReserved(_)))
                || records
                    .iter()
                    .filter(|event| {
                        matches!(&***event, Event::Candidate(candidate)
                            if producer(&candidate.reference) == Some(&reservation.activation))
                    })
                    .count()
                    > 1
            {
                return invalid(
                    "candidate reservation identity, capacity, or single settlement mismatch",
                );
            }
        }
        Event::Candidate(candidate) => {
            let activation = producer(&candidate.reference)
                .ok_or_else(|| invalid_error("candidate has non-activation source"))?;
            let records = source.find(Key::Activation(&activation.activation_id), before)?;
            let input = records.iter().find_map(|event| match &**event {
                Event::Input(record) if record.input.activation == *activation => Some(record),
                _ => None,
            });
            let Some(input) = input else {
                return invalid("candidate has no immutable input");
            };
            if candidate.reference.conversation_id != input.input.conversation_id
                || candidate.input_sha256 != input.sha256
                || records.iter().any(|event| {
                    matches!(&**event, Event::Candidate(existing)
                        if existing.reference.checkpoint_id == candidate.reference.checkpoint_id)
                })
            {
                return invalid("candidate identity, ownership, or digest mismatch");
            }
            let reserved = records.iter().any(|event| {
                matches!(&**event, Event::CandidateReserved(reservation)
                    if reservation.activation == *activation)
            });
            if reserved
                && records.iter().any(|event| {
                    matches!(&**event, Event::Candidate(existing)
                        if producer(&existing.reference) == Some(activation))
                })
            {
                return invalid(
                    "candidate reservation identity, capacity, or single settlement mismatch",
                );
            }
        }
        Event::PromotionReserved { turn_id } => {
            if source
                .find(Key::Turn(turn_id), before)?
                .iter()
                .any(|event| {
                    matches!(
                        &**event,
                        Event::PromotionReserved { .. } | Event::PromotionPrepared(_)
                    )
                })
            {
                return invalid("invalid or already completed promotion reservation");
            }
        }
        Event::PromotionPrepared(manifest) => {
            if source
                .find(Key::Turn(manifest.closure.turn_id()), before)?
                .iter()
                .any(|event| matches!(&**event, Event::PromotionPrepared(_)))
            {
                return invalid("duplicate promotion for closed turn");
            }
            for entry in &manifest.selected {
                let activation = producer(&entry.accepted)
                    .ok_or_else(|| invalid_error("promotion lacks exact activation source"))?;
                let records = source.find(Key::Activation(&activation.activation_id), before)?;
                let input = records.iter().any(|event| {
                    matches!(&**event, Event::Input(record)
                        if record.input.activation == *activation && record.slot_id == entry.slot_id)
                });
                let candidate = records.iter().any(|event| {
                    matches!(&**event, Event::Candidate(candidate) if candidate.reference == entry.accepted)
                });
                if !input || !candidate {
                    return invalid("promotion selects a foreign or missing checkpoint");
                }
            }
        }
        Event::Rewind(rewind) => {
            let prior: Vec<SessionRewind> = source
                .find(Key::Rewinds, before)?
                .iter()
                .filter_map(|event| match &**event {
                    Event::Rewind(rewind) => Some(rewind.clone()),
                    _ => None,
                })
                .collect();
            if prior.iter().any(|item| item.rewind_id == rewind.rewind_id) {
                return invalid("rewind journal identity, ordering or bounds differ");
            }
            for entry in &rewind.conversations {
                let (Some(reference), None) = (&entry.checkpoint, &entry.projection) else {
                    continue;
                };
                let promoted = match committed_promotion(reference) {
                    Some(promotion) => source
                        .find(Key::Promotion(promotion), before)?
                        .iter()
                        .any(|event| {
                            matches!(&**event, Event::PromotionPrepared(manifest)
                                if manifest.selected.iter().any(|selected| &selected.committed == reference))
                        }),
                    None => false,
                };
                if !promoted
                    && !baselines
                        .iter()
                        .any(|baseline| &baseline.reference == reference)
                    && !prior
                        .iter()
                        .flat_map(|item| &item.conversations)
                        .any(|item| item.checkpoint.as_ref() == Some(reference))
                {
                    return invalid("rewind names an uncommitted or future checkpoint");
                }
            }
        }
    }
    Ok(())
}

/// The promotion a committed checkpoint was selected by, if any.
pub(super) fn committed_promotion(reference: &CheckpointRef) -> Option<&str> {
    match &reference.source {
        CheckpointSource::Committed { evidence } => evidence.as_str().strip_prefix("promotion:"),
        CheckpointSource::Accepted { .. } => None,
    }
}

/// Records held in memory, for checking a converted single-file store.
struct MemorySource<'a> {
    events: &'a [Arc<Event>],
    index: HashMap<String, Vec<usize>>,
}

impl<'a> MemorySource<'a> {
    fn new(events: &'a [Arc<Event>]) -> Self {
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (at, event) in events.iter().enumerate() {
            for key in event.keys() {
                index.entry(key).or_default().push(at);
            }
        }
        Self { events, index }
    }
}

impl RecordSource for MemorySource<'_> {
    fn find(&self, key: Key<'_>, before: u64) -> Result<Vec<Arc<Event>>> {
        Ok(self
            .index
            .get(&key.text())
            .into_iter()
            .flatten()
            .filter(|at| (**at as u64) + 1 < before)
            .map(|at| self.events[*at].clone())
            .filter(|event| event.has(key))
            .collect())
    }
}

/// The single-file layout written by earlier versions, read only to migrate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyState {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub journal: Option<CanonicalJournal>,
    #[serde(default)]
    pub baselines: Vec<BaselineRecord>,
    pub inputs: Vec<InputRecord>,
    pub candidates: Vec<Candidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_reservations: Vec<CandidateReservation>,
    pub heads: Vec<PromotedConversation>,
    pub promotions: Vec<PromotionManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewinds: Vec<SessionRewind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promotion_reservations: Vec<LogicalTurnId>,
    pub pending: Option<PromotionManifest>,
}

/// The records of a single-file state, in an order whose replay reproduces
/// it: baselines, inputs, reservations and candidates before any decision
/// that names them, each rewind after exactly the promotions it followed,
/// open promotion reservations, and last an unfinished decision.
fn legacy_events(state: &LegacyState) -> Result<Vec<Arc<Event>>> {
    if state
        .rewinds
        .windows(2)
        .any(|pair| pair[0].after_promotions > pair[1].after_promotions)
        || state
            .rewinds
            .iter()
            .any(|rewind| rewind.after_promotions > state.promotions.len())
    {
        return invalid("rewind journal identity, ordering or bounds differ");
    }
    let mut events = Vec::new();
    events.extend(state.baselines.iter().cloned().map(Event::Baseline));
    events.extend(state.inputs.iter().cloned().map(Event::Input));
    events.extend(
        state
            .candidate_reservations
            .iter()
            .cloned()
            .map(Event::CandidateReserved),
    );
    events.extend(state.candidates.iter().cloned().map(Event::Candidate));
    let mut rewinds = state.rewinds.iter().peekable();
    for (index, promotion) in state.promotions.iter().enumerate() {
        while let Some(rewind) = rewinds.next_if(|rewind| rewind.after_promotions == index) {
            events.push(Event::Rewind(rewind.clone()));
        }
        events.push(Event::PromotionPrepared(promotion.clone()));
        events.push(Event::PromotionFinished {
            promotion_id: promotion.promotion_id.clone(),
        });
    }
    events.extend(rewinds.cloned().map(Event::Rewind));
    events.extend(
        state
            .promotion_reservations
            .iter()
            .map(|turn_id| Event::PromotionReserved {
                turn_id: turn_id.clone(),
            }),
    );
    events.extend(state.pending.iter().cloned().map(Event::PromotionPrepared));
    Ok(events.into_iter().map(Arc::new).collect())
}

/// Validate a single-file state as fully as any new record is validated,
/// and return its records.
fn validate_legacy(state: &LegacyState) -> Result<Vec<Arc<Event>>> {
    if state.schema_version != SCHEMA {
        return invalid("unsupported activation-state schema");
    }
    let events = legacy_events(state)?;
    let identity = Identity {
        session_id: &state.session_id,
        journal: state.journal.as_ref(),
    };
    let source = MemorySource::new(&events);
    let mut projection = Projection::default();
    for (at, event) in events.iter().enumerate() {
        check_record(event, &identity)?;
        check_history(event, at as u64 + 1, &source, &projection.baselines)?;
        projection.apply(event)?;
    }
    let mut heads: Vec<_> = projection
        .conversations
        .values()
        .filter_map(|conversation| conversation.head.clone())
        .collect();
    heads.sort_by(|a, b| {
        a.committed
            .conversation_id
            .as_str()
            .cmp(b.committed.conversation_id.as_str())
    });
    if heads != state.heads {
        return invalid("conversation heads do not match exact promotion history");
    }
    Ok(events)
}

fn log_meta(session_id: &SessionId, owned: Option<&CanonicalJournal>) -> serde_json::Value {
    serde_json::json!({ "session_id": session_id, "journal": owned })
}

impl StoreDirectory {
    fn open_log<R, F>(
        &self,
        spec: SegmentSpec,
        meta: serde_json::Value,
        create: bool,
        visit: F,
    ) -> Result<SegmentLog>
    where
        R: serde::de::DeserializeOwned,
        F: FnMut(u64, R) -> Result<()>,
    {
        match self {
            Self::Isolated(dir) => SegmentLog::open(dir.clone(), spec, meta, create, visit),
            Self::Owned(dir) => dir.open_segment_log(spec, meta, create, visit),
        }
    }
    fn log_exists(&self, spec: &SegmentSpec) -> io::Result<bool> {
        match self {
            Self::Isolated(dir) => SegmentLog::exists(dir, spec),
            Self::Owned(dir) => dir.segment_log_exists(spec),
        }
    }
    fn remove_log(&self, spec: &SegmentSpec) -> io::Result<()> {
        match self {
            Self::Isolated(dir) => SegmentLog::remove(dir, spec),
            Self::Owned(dir) => dir.remove_segment_log(spec),
        }
    }
}

/// Convert a single-file store: write every record into a fresh log, then
/// replace the primary file with the head. A crash before the head is
/// written leaves the single file in place, and the next open converts it
/// again from the start.
fn migrate(
    root: &StoreDirectory,
    state: &LegacyState,
    spec: SegmentSpec,
    meta: serde_json::Value,
) -> Result<StoreHead> {
    let events = validate_legacy(state)?;
    if root.log_exists(&spec)? {
        root.remove_log(&spec)?;
    }
    let mut log = root.open_log(spec, meta, true, |_, _: IgnoredAny| Ok(()))?;
    for event in &events {
        let line = log.encode_record(&**event)?;
        log.append_line(&line)?;
        if log.should_seal() {
            log.seal()?;
        }
    }
    drop(log);
    let head = StoreHead {
        schema_version: SCHEMA,
        session_id: state.session_id.clone(),
        journal: state.journal.clone(),
        segments: SegmentsMarker::of(&spec),
    };
    root.atomic_write(STATE_FILE, &serde_json::to_vec(&head)?)?;
    Ok(head)
}

impl ActivationStateStore {
    pub(super) fn open_directory(
        root: StoreDirectory,
        session_id: SessionId,
        journal: Option<CanonicalJournal>,
        spec: SegmentSpec,
    ) -> Result<Self> {
        root.verify_ambient_identity()?;
        // Only an owned store knows its canonical journal before any record;
        // its log binds it in every segment.
        let meta = log_meta(&session_id, journal.as_ref());
        let check_identity = |found: &SessionId, bound: Option<&CanonicalJournal>| {
            if *found != session_id
                || journal
                    .as_ref()
                    .is_some_and(|expected| bound != Some(expected))
            {
                return invalid("store belongs to another Session or canonical journal");
            }
            Ok(())
        };
        let head = match root.read_limited(STATE_FILE, MAX_RECORD_BYTES) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                if value.get("segments").is_some() {
                    let head: StoreHead = serde_json::from_value(value)?;
                    check_identity(&head.session_id, head.journal.as_ref())?;
                    if head.schema_version != SCHEMA || !head.segments.matches(&spec) {
                        return invalid("unsupported activation-state schema");
                    }
                    head
                } else {
                    let state: LegacyState = serde_json::from_slice(&bytes)?;
                    check_identity(&state.session_id, state.journal.as_ref())?;
                    migrate(&root, &state, spec, meta.clone())?
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                root.check_journal_creation(STATE_FILE)?;
                if !root.entries_limited(1)?.is_empty() {
                    return invalid("missing store identity in a nonempty directory");
                }
                let head = StoreHead {
                    schema_version: SCHEMA,
                    session_id: session_id.clone(),
                    journal: journal.clone(),
                    segments: SegmentsMarker::of(&spec),
                };
                // The marker is published first, so losing the head after
                // this point fails closed instead of looking like a new store.
                root.mark_journal_initialized(STATE_FILE)?;
                drop(root.open_log(spec, meta.clone(), true, |_, _: IgnoredAny| Ok(()))?);
                root.atomic_write(STATE_FILE, &serde_json::to_vec(&head)?)?;
                head
            }
            Err(error) => return Err(error.into()),
        };
        // First learn where every sealed segment ends; opening also removes
        // a torn last line and completes an interrupted seal.
        let boundary = root.open_log(spec, meta.clone(), false, |_, _: IgnoredAny| Ok(()))?;
        let recovery = boundary.recovery();
        let boundaries = boundary.sealed().to_vec();
        let active_first = boundary.active_first_sequence();
        drop(boundary);
        let identity = Identity {
            session_id: &head.session_id,
            journal: head.journal.as_ref(),
        };
        let mut projection = Projection::default();
        let mut active = Vec::new();
        let mut sealed = Vec::with_capacity(boundaries.len());
        let mut segment_events: Vec<Arc<Event>> = Vec::new();
        let log = root.open_log(spec, meta, false, |sequence, event: Event| {
            check_record(&event, &identity)?;
            projection.apply(&event)?;
            let event = Arc::new(event);
            if sequence >= active_first {
                active.push(event);
                return Ok(());
            }
            while boundaries
                .get(sealed.len())
                .is_some_and(|segment| segment.first_sequence + segment.records <= sequence)
            {
                sealed.push(SealedSummary::of(&segment_events));
                segment_events.clear();
            }
            segment_events.push(event);
            if boundaries
                .get(sealed.len())
                .is_some_and(|segment| segment.first_sequence + segment.records == sequence + 1)
            {
                sealed.push(SealedSummary::of(&segment_events));
                segment_events.clear();
            }
            Ok(())
        })?;
        while sealed.len() < boundaries.len() {
            sealed.push(SealedSummary::of(&segment_events));
            segment_events.clear();
        }
        if log.sealed() != boundaries.as_slice() || log.active_first_sequence() != active_first {
            return invalid("activation-state log changed while it was opened");
        }
        // Persist identity before creating subordinate namespaces. A crash can
        // resume missing empty namespaces, but never reassign a populated store.
        root.mark_journal_initialized(STATE_FILE)?;
        let objects = root.child("objects")?;
        let heads = root.child("heads")?;
        objects.sync_all()?;
        heads.sync_all()?;
        root.sync_all()?;
        let mut store = Self {
            root,
            objects,
            heads,
            head,
            log,
            active,
            sealed,
            cache: SegmentCache::new(CACHED_SEGMENTS),
            projection,
            uncertain: false,
            recovery,
        };
        let active = store.active.clone();
        for (offset, event) in active.iter().enumerate() {
            check_history(
                event,
                active_first + offset as u64,
                &store,
                &store.projection.baselines,
            )?;
        }
        if store.log.should_seal() {
            // A crash came between an append and its seal.
            store.uncertain = true;
            store.seal_active()?;
            store.uncertain = false;
        }
        if store.projection.pending.is_some() {
            store.finish_pending()?;
        }
        store.verify_heads()?;
        Ok(store)
    }

    /// Check a new record against the whole history and the current
    /// summaries, and encode it, without writing anything.
    pub(super) fn admit(
        &self,
        event: &Event,
        journal: &CanonicalJournal,
    ) -> Result<(Projection, Vec<u8>)> {
        if self
            .head
            .journal
            .as_ref()
            .is_some_and(|bound| bound != journal)
        {
            return invalid("snapshot belongs to another canonical journal or workspace");
        }
        check_record(
            event,
            &Identity {
                session_id: &self.head.session_id,
                journal: Some(journal),
            },
        )?;
        check_history(
            event,
            self.log.next_sequence(),
            self,
            &self.projection.baselines,
        )?;
        let mut next = self.projection.clone();
        next.apply(event)?;
        let line = self.log.encode_record(event)?;
        Ok((next, line))
    }

    /// Append an admitted record. Only an `Ok` acknowledges it; any failure
    /// leaves the store refusing work until it is reopened.
    pub(super) fn append(
        &mut self,
        event: Event,
        (next, line): (Projection, Vec<u8>),
    ) -> Result<()> {
        self.root.verify_ambient_identity()?;
        self.uncertain = true;
        self.log.append_line(&line)?;
        self.active.push(Arc::new(event));
        self.projection = next;
        if self.log.should_seal() {
            self.seal_active()?;
        }
        self.uncertain = false;
        Ok(())
    }

    /// An isolated store learns its canonical journal from its first record
    /// that carries one; the head records it before that record.
    pub(super) fn bind_journal(&mut self, journal: CanonicalJournal) -> Result<()> {
        if self.head.journal.is_some() {
            return Ok(());
        }
        let mut head = self.head.clone();
        head.journal = Some(journal);
        self.uncertain = true;
        self.root
            .atomic_write(STATE_FILE, &serde_json::to_vec(&head)?)?;
        self.head = head;
        self.uncertain = false;
        Ok(())
    }

    fn seal_active(&mut self) -> Result<()> {
        let summary = SealedSummary::of(&self.active);
        self.log.seal()?;
        self.sealed.push(summary);
        self.active.clear();
        Ok(())
    }

    /// Every record indexed under `key`, from all segments.
    pub(super) fn lookup(&self, key: Key<'_>) -> Result<Vec<Arc<Event>>> {
        self.find(key, u64::MAX)
    }

    pub(super) fn input(&self, activation: &ActivationRef) -> Result<Option<InputRecord>> {
        Ok(self
            .lookup(Key::Activation(&activation.activation_id))?
            .iter()
            .find_map(|event| match &**event {
                Event::Input(record) if record.input.activation == *activation => {
                    Some(record.clone())
                }
                _ => None,
            }))
    }

    pub(super) fn reservation(
        &self,
        activation: &ActivationRef,
    ) -> Result<Option<CandidateReservation>> {
        Ok(self
            .lookup(Key::Activation(&activation.activation_id))?
            .iter()
            .find_map(|event| match &**event {
                Event::CandidateReserved(record) if record.activation == *activation => {
                    Some(record.clone())
                }
                _ => None,
            }))
    }

    /// Candidates retained for one producing activation.
    pub(super) fn candidates_for(&self, activation: &ActivationRef) -> Result<Vec<Candidate>> {
        Ok(self
            .lookup(Key::Activation(&activation.activation_id))?
            .iter()
            .filter_map(|event| match &**event {
                Event::Candidate(candidate)
                    if producer(&candidate.reference) == Some(activation) =>
                {
                    Some(candidate.clone())
                }
                _ => None,
            })
            .collect())
    }

    pub(super) fn candidate(&self, reference: &CheckpointRef) -> Result<Option<Candidate>> {
        let Some(activation) = producer(reference) else {
            return Ok(None);
        };
        Ok(self
            .candidates_for(activation)?
            .into_iter()
            .find(|candidate| candidate.reference == *reference))
    }

    /// The completed promotion decision of a turn.
    pub(super) fn promotion_of_turn(
        &self,
        turn_id: &LogicalTurnId,
    ) -> Result<Option<PromotionManifest>> {
        Ok(self
            .lookup(Key::Turn(turn_id))?
            .iter()
            .find_map(|event| match &**event {
                Event::PromotionPrepared(manifest)
                    if manifest.closure.turn_id() == turn_id
                        && self.projection.pending.as_ref() != Some(manifest) =>
                {
                    Some(manifest.clone())
                }
                _ => None,
            }))
    }

    /// A completed promotion decision by its identity.
    pub(super) fn promotion_by_id(&self, promotion_id: &str) -> Result<Option<PromotionManifest>> {
        Ok(self
            .lookup(Key::Promotion(promotion_id))?
            .iter()
            .find_map(|event| match &**event {
                Event::PromotionPrepared(manifest)
                    if manifest.promotion_id == promotion_id
                        && self.projection.pending.as_ref() != Some(manifest) =>
                {
                    Some(manifest.clone())
                }
                _ => None,
            }))
    }

    /// Whether a turn already has a promotion reservation or an input,
    /// either of which reserves its final promotion.
    pub(super) fn turn_reserved(&self, turn_id: &LogicalTurnId) -> Result<bool> {
        Ok(self
            .lookup(Key::Turn(turn_id))?
            .iter()
            .any(|event| matches!(&**event, Event::PromotionReserved { .. } | Event::Input(_))))
    }

    pub(super) fn rewinds(&self) -> Result<Vec<SessionRewind>> {
        Ok(self
            .lookup(Key::Rewinds)?
            .iter()
            .filter_map(|event| match &**event {
                Event::Rewind(rewind) => Some(rewind.clone()),
                _ => None,
            })
            .collect())
    }

    /// For each conversation, the committed checkpoint of the latest
    /// completed promotion of a kept turn that selected it. This reads the
    /// history backwards from the newest record and stops once every
    /// conversation is found.
    pub(super) fn latest_kept_selections(
        &self,
        kept: &HashSet<&str>,
        conversations: &[NodeConversationId],
    ) -> Result<HashMap<NodeConversationId, CheckpointRef>> {
        let mut found = HashMap::new();
        let mut visit = |event: &Event| {
            if let Event::PromotionPrepared(manifest) = event {
                if self.projection.pending.as_ref() != Some(manifest)
                    && kept.contains(manifest.closure.turn_id().as_str())
                {
                    for selected in manifest.selected.iter().rev() {
                        let conversation = &selected.committed.conversation_id;
                        if conversations.contains(conversation) && !found.contains_key(conversation)
                        {
                            found.insert(conversation.clone(), selected.committed.clone());
                        }
                    }
                }
            }
            found.len() == conversations.len()
        };
        for event in self.active.iter().rev() {
            if visit(event) {
                return Ok(found);
            }
        }
        for segment in self.log.sealed().iter().rev() {
            // A full scan reads past the cache so that it keeps the
            // segments live operations use.
            for event in self.log.read_sealed::<Event>(segment)?.iter().rev() {
                if visit(event) {
                    return Ok(found);
                }
            }
        }
        Ok(found)
    }
}

impl RecordSource for ActivationStateStore {
    fn find(&self, key: Key<'_>, before: u64) -> Result<Vec<Arc<Event>>> {
        let text = key.text();
        let mut found = Vec::new();
        for (summary, segment) in self.sealed.iter().zip(self.log.sealed()) {
            if segment.first_sequence >= before {
                break;
            }
            if !summary.may_hold(&text) {
                continue;
            }
            let records = self.cache.get(&self.log, segment)?;
            for (offset, event) in records.iter().enumerate() {
                if segment.first_sequence + offset as u64 >= before {
                    break;
                }
                if event.has(key) {
                    found.push(event.clone());
                }
            }
        }
        let first = self.log.active_first_sequence();
        for (offset, event) in self.active.iter().enumerate() {
            if first + offset as u64 >= before {
                break;
            }
            if event.has(key) {
                found.push(event.clone());
            }
        }
        Ok(found)
    }
}

#[cfg(all(test, unix))]
#[path = "activation_state_journal_tests.rs"]
mod tests;
