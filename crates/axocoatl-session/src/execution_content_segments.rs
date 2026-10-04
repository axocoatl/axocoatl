//! The content journal's records in a segment log, found again by key.
//!
//! Every record is findable by `ref:<reference>` and by the identities later
//! records or readers look it up by: its turn, activation, invocation,
//! reservation, condition run, provider definition or admission. Memory holds
//! the active segment with an index of its keys, a key filter per sealed
//! segment and a few decoded sealed segments; older records are read back by
//! key on demand.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::*;
use crate::segment_log::{KeyFilter, SegmentCache, SegmentLog, SegmentSpec, SegmentsMarker};

pub(super) const SPEC: SegmentSpec = SegmentSpec {
    name: "execution-content",
    kind: "execution-content",
    segment_bytes: 8 * 1024 * 1024,
    // Unit tests seal often so that every path crosses segments.
    segment_records: if cfg!(test) { 8 } else { 4096 },
    // One record is bounded by the content encoder.
    record_bytes: MAX_BYTES + 1024,
};
const CACHED_SEGMENTS: usize = 4;

/// The primary file of a segmented content journal: its identity only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContentHead {
    pub(super) schema_version: u32,
    pub(super) journal_id: String,
    pub(super) owner: ExecutionStoreOwner,
    pub(super) segments: SegmentsMarker,
}

impl ContentHead {
    pub(super) fn new(identity: &DurableSessionIdentity) -> Self {
        Self {
            schema_version: SCHEMA,
            journal_id: identity.journal_id().into(),
            owner: identity.owner().clone(),
            segments: SegmentsMarker::of(&SPEC),
        }
    }
    pub(super) fn meta(&self) -> serde_json::Value {
        serde_json::json!({"journal_id": self.journal_id, "owner": self.owner})
    }
}

pub(super) enum StoredContent {
    Head(ContentHead),
    Legacy(Journal),
}

pub(super) fn parse_stored(bytes: &[u8]) -> Result<StoredContent, ExecutionContentError> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        segments: Option<serde::de::IgnoredAny>,
    }
    let probe: Probe = serde_json::from_slice(bytes)?;
    Ok(if probe.segments.is_some() {
        let head: ContentHead = serde_json::from_slice(bytes)?;
        if head.schema_version != SCHEMA || !head.segments.matches(&SPEC) {
            return Err(ExecutionContentError::Invalid("unsupported content schema"));
        }
        StoredContent::Head(head)
    } else {
        StoredContent::Legacy(serde_json::from_slice(bytes)?)
    })
}

fn turn_key(turn: &LogicalTurnId) -> String {
    format!("turn:{}", turn.as_str())
}

pub(super) fn activation_key(activation: &ActivationRef) -> String {
    format!("act:{}", activation.activation_id.as_str())
}

pub(super) fn reference_key(reference: &EvidenceRef) -> String {
    format!("ref:{}", reference.as_str())
}

/// Records settling the reservation `reference`: tool and check results and
/// reserved outputs.
pub(super) fn reserved_key(reference: &EvidenceRef) -> String {
    format!("reserved:{}", reference.as_str())
}

pub(super) fn request_key(turn: &LogicalTurnId) -> String {
    format!("request:{}", turn.as_str())
}

pub(super) fn ways_turn_key(turn: &LogicalTurnId) -> String {
    format!("ways-turn:{}", turn.as_str())
}

pub(super) const WAYS_SELECTION_KEY: &str = "kind:ways-selection";

pub(super) fn tool_key(invocation: &InvocationId) -> String {
    format!("tool:{}", invocation.as_str())
}

pub(super) fn run_key(run: &ConditionRunId) -> String {
    format!("run:{}", run.as_str())
}

pub(super) fn output_key(activation: &ActivationRef) -> String {
    format!("output:{}", activation.activation_id.as_str())
}

pub(super) fn output_reservation_key(activation: &ActivationRef) -> String {
    format!("output-reservation:{}", activation.activation_id.as_str())
}

/// Every stream event of an activation.
pub(super) fn stream_key(activation: &ActivationRef) -> String {
    format!("stream:{}", activation.activation_id.as_str())
}

/// One stream event of an activation, by its sequence.
pub(super) fn stream_event_key(activation: &ActivationRef, sequence: u64) -> String {
    format!("stream:{}:{sequence}", activation.activation_id.as_str())
}

/// The terminal settlement of an activation's reserved output.
pub(super) fn settlement_key(activation: &ActivationRef) -> String {
    format!("settlement:{}", activation.activation_id.as_str())
}

/// Repository snapshots taken for an activation.
pub(super) fn repository_snapshot_key(activation: &ActivationRef) -> String {
    format!("repository-snapshot:{}", activation.activation_id.as_str())
}

/// Repository snapshots taken in a turn.
pub(super) fn turn_repository_snapshot_key(turn: &LogicalTurnId) -> String {
    format!("repository-snapshot-turn:{}", turn.as_str())
}

/// Repository reattachment proofs of a turn.
pub(super) fn reattachment_key(turn: &LogicalTurnId) -> String {
    format!("reattachment:{}", turn.as_str())
}

/// Every key `record` is found by.
pub(super) fn record_keys(record: &Record) -> Vec<String> {
    let mut keys = vec![reference_key(&record.reference)];
    let activation = |keys: &mut Vec<String>, activation: &ActivationRef| {
        keys.push(activation_key(activation));
        keys.push(turn_key(&activation.turn_id));
    };
    match &record.body {
        Body::Request(content) => {
            keys.push(request_key(&content.turn_id));
            keys.push(turn_key(&content.turn_id));
        }
        Body::WaysSelection {
            selection,
            activation: selected,
        } => {
            keys.push(WAYS_SELECTION_KEY.into());
            keys.push(ways_turn_key(&selection.turn_id));
            keys.push(turn_key(&selection.turn_id));
            activation(&mut keys, selected);
        }
        Body::ActivationStream(content) => {
            keys.push(stream_key(&content.activation));
            keys.push(stream_event_key(&content.activation, content.sequence));
            activation(&mut keys, &content.activation);
        }
        Body::Output(content) => {
            keys.push(output_key(&content.activation));
            activation(&mut keys, &content.activation);
        }
        Body::ActivationOutputReservation(content) => {
            keys.push(output_reservation_key(&content.activation));
            activation(&mut keys, &content.activation);
        }
        Body::ReservedOutput(content) => {
            keys.push(reserved_key(&content.reservation_ref));
            if content.slot == ActivationOutputSlot::Settlement {
                keys.push(settlement_key(&content.output.activation));
            }
            activation(&mut keys, &content.output.activation);
        }
        Body::ToolReservation(content) => {
            keys.push(tool_key(&content.invocation_id));
            activation(&mut keys, &content.activation);
        }
        Body::ToolResult(content) => keys.push(reserved_key(&content.reservation_ref)),
        Body::ConditionArguments(content) => {
            keys.push(run_key(&content.run.run_id));
            keys.push(turn_key(&content.run.turn_id));
        }
        Body::ConditionResult(content) => keys.push(reserved_key(&content.reservation_ref)),
        Body::LegacyHistory(_) => keys.push("kind:legacy-history".into()),
        Body::ProviderProfile(profile) => keys.push(provider::profile_key(profile)),
        Body::RepositorySnapshot(snapshot) => {
            keys.push(repository_snapshot_key(&snapshot.activation));
            keys.push(turn_repository_snapshot_key(&snapshot.activation.turn_id));
            activation(&mut keys, &snapshot.activation);
        }
        Body::RepositoryReattachment(proof) => {
            keys.push(reattachment_key(&proof.turn_id));
            keys.push(turn_key(&proof.turn_id));
        }
        Body::TurnAdmission(content) => {
            keys.extend(turn_admission::admission_keys(content));
            keys.push(turn_key(&content.turn_id));
        }
        Body::DriverHandoff(handoff) => keys.extend(turn_admission::driver_handoff_keys(handoff)),
        Body::ControlDriverHandoff(handoff) => {
            keys.extend(turn_admission::control_handoff_keys(handoff))
        }
        Body::ActivationEvidence(_)
        | Body::RepositoryCheckDefinition(_)
        | Body::StandingRepositoryCheck(_) => {}
    }
    keys
}

/// The keys of every earlier record `validate_next` may consult for `body`:
/// those it may conflict with and those it must find by reference.
pub(super) fn dependency_keys(body: &Body) -> Vec<String> {
    match body {
        Body::Request(content) => vec![request_key(&content.turn_id)],
        Body::WaysSelection { selection, .. } => vec![
            ways_turn_key(&selection.turn_id),
            reference_key(&selection.transcript_receipt_ref),
        ],
        // The stream check needs the event before this one, a conflicting
        // event at this sequence and the activation's terminal records, never
        // the whole stream.
        Body::ActivationStream(content) => {
            let mut keys = vec![
                stream_event_key(&content.activation, content.sequence),
                output_key(&content.activation),
                settlement_key(&content.activation),
            ];
            if let Some(previous) = content.sequence.checked_sub(1) {
                keys.push(stream_event_key(&content.activation, previous));
            }
            keys
        }
        Body::Output(content) => vec![output_reservation_key(&content.activation)],
        Body::ActivationOutputReservation(content) => vec![
            output_reservation_key(&content.activation),
            output_key(&content.activation),
        ],
        Body::ReservedOutput(content) => vec![
            reserved_key(&content.reservation_ref),
            reference_key(&content.reservation_ref),
        ],
        Body::ToolReservation(content) => vec![tool_key(&content.invocation_id)],
        Body::ToolResult(content) => vec![
            reserved_key(&content.reservation_ref),
            reference_key(&content.reservation_ref),
        ],
        Body::ConditionArguments(content) => {
            let mut keys = vec![
                run_key(&content.run.run_id),
                reference_key(&content.definition_ref),
                reference_key(&content.repository_ref),
            ];
            keys.extend(
                content
                    .inputs
                    .iter()
                    .map(|input| reference_key(&input.output)),
            );
            keys
        }
        Body::ConditionResult(content) => vec![
            reserved_key(&content.reservation_ref),
            reference_key(&content.reservation_ref),
        ],
        Body::RepositoryReattachment(proof) => {
            vec![
                reference_key(&proof.original),
                reference_key(&proof.acquired),
            ]
        }
        Body::ProviderProfile(profile) => provider::profile_dependency_keys(profile),
        Body::TurnAdmission(content) => turn_admission::admission_dependency_keys(content),
        Body::DriverHandoff(handoff) => turn_admission::driver_handoff_dependency_keys(handoff),
        Body::ControlDriverHandoff(handoff) => {
            turn_admission::control_handoff_dependency_keys(handoff)
        }
        Body::LegacyHistory(_)
        | Body::ActivationEvidence(_)
        | Body::RepositoryCheckDefinition(_)
        | Body::RepositorySnapshot(_)
        | Body::StandingRepositoryCheck(_) => vec![],
    }
}

/// The records themselves: the log, the active segment with an index of its
/// keys, a filter per sealed segment and a cache of decoded segments.
pub(super) struct ContentRecords {
    log: SegmentLog,
    active: Vec<Arc<Record>>,
    index: HashMap<String, Vec<usize>>,
    filters: Vec<KeyFilter>,
    cache: SegmentCache<Record>,
    /// Tests: the next append is written, then reported as failed.
    #[cfg(test)]
    pub(super) lose_next_ack: bool,
    /// Tests: records reads behave as if they were missing.
    #[cfg(test)]
    pub(super) hidden: HashSet<EvidenceRef>,
}

impl ContentRecords {
    /// Open the log and check every record on its own; the active segment's
    /// records are also checked against the history before them.
    pub(super) fn open(
        dir: SecureDir,
        identity: &DurableSessionIdentity,
        head: &ContentHead,
    ) -> Result<Self, ExecutionContentError> {
        let mut loader = Loader::default();
        let log = SegmentLog::open_indexed(
            dir,
            SPEC,
            head.meta(),
            false,
            |segment, _sequence, record: Record| loader.visit(identity, segment, record),
        )?;
        let (active, filters, suspects) = loader.finish(&log);
        let mut records = Self {
            log,
            active: vec![],
            index: HashMap::new(),
            filters,
            cache: SegmentCache::new(CACHED_SEGMENTS),
            #[cfg(test)]
            lose_next_ack: false,
            #[cfg(test)]
            hidden: HashSet::new(),
        };
        for (key, segment) in suspects {
            if records.segment_has_key(segment, &key)? {
                return Err(ExecutionContentError::Invalid(
                    "content reference mismatch or duplicate",
                ));
            }
        }
        for record in active {
            let relevant = records.relevant(&dependency_keys(&record.body))?;
            validate_next(&relevant, &record.body)?;
            records.remember(Arc::new(record));
        }
        Ok(records)
    }

    /// Create an empty log.
    pub(super) fn create(dir: SecureDir, head: &ContentHead) -> Result<(), ExecutionContentError> {
        SegmentLog::open(dir, SPEC, head.meta(), true, |_, _: Record| {
            Ok::<(), ExecutionContentError>(())
        })?;
        Ok(())
    }

    /// Records holding `key`, oldest first.
    pub(super) fn keyed(&self, key: &str) -> Result<Vec<Arc<Record>>, ExecutionContentError> {
        let mut records = Vec::new();
        for (segment, filter) in self.log.sealed().iter().zip(&self.filters) {
            if !filter.may_contain(key) {
                continue;
            }
            for record in self.cache.get(&self.log, segment)?.iter() {
                if record_keys(record).iter().any(|held| held == key) {
                    records.push(record.clone());
                }
            }
        }
        if let Some(positions) = self.index.get(key) {
            records.extend(positions.iter().map(|at| self.active[*at].clone()));
        }
        #[cfg(test)]
        records.retain(|record| !self.hidden.contains(&record.reference));
        Ok(records)
    }

    /// The record named `reference`.
    pub(super) fn record(
        &self,
        reference: &EvidenceRef,
    ) -> Result<Option<Arc<Record>>, ExecutionContentError> {
        Ok(self
            .keyed(&reference_key(reference))?
            .into_iter()
            .find(|record| &record.reference == reference))
    }

    /// Every record holding any of `keys`, once each, oldest first.
    pub(super) fn relevant(&self, keys: &[String]) -> Result<Vec<Record>, ExecutionContentError> {
        let mut seen = HashSet::new();
        let mut records: Vec<(u64, Record)> = Vec::new();
        for key in keys {
            for (segment, filter) in self.log.sealed().iter().zip(&self.filters) {
                if !filter.may_contain(key) {
                    continue;
                }
                for (offset, record) in self.cache.get(&self.log, segment)?.iter().enumerate() {
                    let sequence = segment.first_sequence + offset as u64;
                    if !seen.contains(&sequence)
                        && record_keys(record).iter().any(|held| held == key)
                    {
                        seen.insert(sequence);
                        records.push((sequence, record.as_ref().clone()));
                    }
                }
            }
            for at in self.index.get(key).into_iter().flatten() {
                let sequence = self.log.active_first_sequence() + *at as u64;
                if seen.insert(sequence) {
                    records.push((sequence, self.active[*at].as_ref().clone()));
                }
            }
        }
        records.sort_by_key(|(sequence, _)| *sequence);
        #[cfg(test)]
        records.retain(|(_, record)| !self.hidden.contains(&record.reference));
        Ok(records.into_iter().map(|(_, record)| record).collect())
    }

    /// Append `record` as one synced line. Only an `Ok` acknowledges it; any
    /// failure leaves the log refusing writes until it is reopened.
    pub(super) fn push(&mut self, record: Record) -> Result<(), ExecutionContentError> {
        let line = self
            .log
            .encode_record(&record)
            .map_err(|error| match error {
                crate::segment_log::SegmentError::RecordTooLarge => ExecutionContentError::Capacity,
                error => error.into(),
            })?;
        if self.log.append_line(&line).is_err() {
            return Err(ExecutionContentError::RecoveryRequired);
        }
        #[cfg(test)]
        if std::mem::take(&mut self.lose_next_ack) {
            return Err(ExecutionContentError::RecoveryRequired);
        }
        self.remember(Arc::new(record));
        if self.log.should_seal() {
            let filter = self.active_filter();
            // The record is durable; a failed seal is completed on reopen.
            self.log
                .seal()
                .map_err(|_| ExecutionContentError::RecoveryRequired)?;
            self.filters.push(filter);
            self.active.clear();
            self.index.clear();
        }
        Ok(())
    }

    /// The whole history, read back from every segment. Memory grows with
    /// it, so this is for tests, not for live operation.
    #[cfg(test)]
    pub(super) fn all(&self) -> Result<Vec<Arc<Record>>, ExecutionContentError> {
        let mut records = Vec::new();
        for segment in self.log.sealed() {
            records.extend(self.cache.get(&self.log, segment)?.iter().cloned());
        }
        records.extend(self.active.iter().cloned());
        Ok(records)
    }

    /// How many records the journal holds.
    pub(super) fn len(&self) -> u64 {
        self.log.next_sequence() - 1
    }

    /// Sealed segments and the bytes of memory their key filters hold.
    pub(super) fn sealed_segments(&self) -> (usize, usize) {
        (
            self.filters.len(),
            self.filters.iter().map(KeyFilter::bytes).sum(),
        )
    }

    #[cfg(test)]
    pub(super) fn active_len(&self) -> usize {
        self.active.len()
    }

    fn remember(&mut self, record: Arc<Record>) {
        let at = self.active.len();
        for key in record_keys(&record) {
            self.index.entry(key).or_default().push(at);
        }
        self.active.push(record);
    }

    fn active_filter(&self) -> KeyFilter {
        KeyFilter::new(self.index.keys().map(String::as_str))
    }

    fn segment_has_key(&self, index: u64, key: &str) -> Result<bool, ExecutionContentError> {
        let segment = &self.log.sealed()[index as usize];
        Ok(self
            .cache
            .get(&self.log, segment)?
            .iter()
            .any(|record| record_keys(record).iter().any(|held| held == key)))
    }
}

/// Checks each record on its own while the log is opened, one segment at a
/// time, and leaves a filter for each sealed segment.
#[derive(Default)]
struct Loader {
    segment: u64,
    keys: HashSet<String>,
    references: HashSet<EvidenceRef>,
    records: Vec<Record>,
    filters: Vec<KeyFilter>,
    suspects: Vec<(String, u64)>,
}

impl Loader {
    fn visit(
        &mut self,
        identity: &DurableSessionIdentity,
        segment: u64,
        record: Record,
    ) -> Result<(), ExecutionContentError> {
        if segment != self.segment {
            self.close_segment();
            self.segment = segment;
        }
        validate_body(&record.body, identity.owner())?;
        if !self.references.insert(record.reference.clone())
            || content_reference(identity, &record.body)? != record.reference
        {
            return Err(ExecutionContentError::Invalid(
                "content reference mismatch or duplicate",
            ));
        }
        let key = reference_key(&record.reference);
        for (earlier, filter) in self.filters.iter().enumerate() {
            if filter.may_contain(&key) {
                self.suspects.push((key.clone(), earlier as u64));
            }
        }
        self.keys.extend(record_keys(&record));
        self.records.push(record);
        Ok(())
    }

    fn close_segment(&mut self) {
        self.filters
            .push(KeyFilter::new(self.keys.iter().map(String::as_str)));
        self.keys.clear();
        self.references.clear();
        self.records.clear();
    }

    fn finish(mut self, log: &SegmentLog) -> (Vec<Record>, Vec<KeyFilter>, Vec<(String, u64)>) {
        let active = if self.segment == log.active_index() {
            std::mem::take(&mut self.records)
        } else {
            if !self.records.is_empty() {
                self.close_segment();
            }
            vec![]
        };
        debug_assert_eq!(self.filters.len(), log.sealed().len());
        (active, self.filters, self.suspects)
    }
}

/// Move a single-file journal's records into a segment log and replace its
/// file with the head. A crash before the head is written leaves the old
/// file, and the next open converts it again from the start.
pub(super) fn migrate(
    storage: &Storage,
    dir: &SecureDir,
    identity: &DurableSessionIdentity,
    journal: Journal,
) -> Result<ContentHead, ExecutionContentError> {
    let head = ContentHead::new(identity);
    SegmentLog::remove(dir, &SPEC)?;
    let mut log = SegmentLog::open(dir.clone(), SPEC, head.meta(), true, |_, _: Record| {
        Ok::<(), ExecutionContentError>(())
    })?;
    for record in &journal.records {
        log.append_line(&log.encode_record(record)?)?;
        if log.should_seal() {
            log.seal()?;
        }
    }
    drop(log);
    storage.mark_initialized(identity)?;
    storage.write(&serde_json::to_vec(&head)?)?;
    Ok(head)
}
