//! Session-owned Ways evidence, pinned before any disposable candidate cleanup.
//!
//! This store does not apply patches or authorize cleanup. The existing Keep
//! transaction supplies its actual result, then records exact cleanup receipts.
//!
//! Layout inside the `ways-decisions` component directory:
//!
//! - `ways-decisions.v1.json` is the head: the store's identity, the
//!   configured retention limits and a [`SegmentsMarker`]. It never grows.
//! - `ways-decisions.active.jsonl` and `segments/` are a [`SegmentLog`] of
//!   small index entries, in order: every state of every decision (the digest
//!   and size of its record and the protected patches it pins) and every
//!   deletion tombstone.
//! - `evidence/` holds the bodies, each named by its SHA-256: the current
//!   record of each retained decision (`decision-<sha256>.json`) and each
//!   protected patch a retained decision pins (`patch-<sha256>.bin`).
//!
//! Every body is written and synced before the index entry that names it, so
//! one synced line publishes a decision with all of its protected patch bytes.
//! A body that no retained decision names (an unacknowledged write, a replaced
//! record, or the evidence of a deleted decision) is removed, at the latest
//! when the store is next opened: deleting a decision removes its record and
//! its unshared patch bytes from disk, and keeps its tombstone.
//!
//! The configured retention limits bound what is retained: the retained
//! decisions, the patches they pin, and the room each unfinished decision
//! reserves for its final receipts. A tombstone is kept for the Session's
//! whole life and is not counted, so deleting a decision frees its room.
//!
//! Memory stays bounded: the store keeps the index of retained decisions
//! (bounded by the retention limits), the active segment's tombstones, one
//! [`KeyFilter`] per sealed segment that holds tombstones, and a
//! [`SegmentCache`] of a few segments. Record and patch bodies are read from
//! disk, and checked against their digests, only by the call that needs them.
//!
//! A store written in the earlier single-file layout (one
//! `ways-decisions.v1.json` holding every record, patch and tombstone) is
//! converted the first time it is opened.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;

use axocoatl_core::SecureEntryType;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use crate::segment_log::{
    KeyFilter, SegmentCache, SegmentError, SegmentLog, SegmentSpec, SegmentsMarker,
};
use crate::turn_contract::EvidenceRef;
use crate::ways_decision::*;

pub const WAYS_ARCHIVE_FILE: &str = "ways-decisions.v1.json";
/// The largest protected patch one candidate can pin. Each patch is stored
/// as its own file.
pub const MAX_WAYS_PATCH_BYTES: usize = 64 * 1024 * 1024;
/// The single-file layout held everything in one file of at most this size.
const MAX_LEGACY_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
/// A record body is bounded by its configured `record_bytes`, which can never
/// be larger than this.
const MAX_RECORD_BODY_BYTES: usize = 64 * 1024 * 1024;
const EVIDENCE_DIR: &str = "evidence";
const CACHED_SEGMENTS: usize = 8;
const SPEC: SegmentSpec = SegmentSpec {
    name: "ways-decisions",
    kind: "ways-decisions",
    segment_bytes: 1024 * 1024,
    segment_records: 4096,
    // One index entry holds identities and at most 100 patch pins (~26 KiB).
    record_bytes: 128 * 1024,
};

#[derive(Debug, thiserror::Error)]
pub enum WaysArchiveError {
    #[error("Ways evidence storage: {0}")]
    Io(#[from] io::Error),
    #[error("Ways evidence encoding: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Contract(#[from] WaysDecisionError),
    #[error("invalid Ways evidence archive: {0}")]
    Invalid(&'static str),
    #[error("Ways evidence write is uncertain; reopen before cleanup or acknowledgement")]
    RecoveryRequired,
}
type Result<T> = std::result::Result<T, WaysArchiveError>;

impl From<SegmentError> for WaysArchiveError {
    fn from(error: SegmentError) -> Self {
        match error {
            SegmentError::Io(error) => Self::Io(error),
            SegmentError::Json(error) => Self::Json(error),
            SegmentError::Invalid(reason) => Self::Invalid(reason),
            SegmentError::RecordTooLarge => Self::Contract(WaysDecisionError::Capacity),
            SegmentError::RecoveryRequired => Self::RecoveryRequired,
            SegmentError::Changed => {
                Self::Invalid("the Ways evidence log changed while it was read")
            }
        }
    }
}

/// Opaque artifact identity is derived from the exact protected patch bytes.
/// Original Git object identities remain in the candidate record; their patch
/// body is independent of disposable clones and refs after publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedWaysPatch {
    pub reference: EvidenceRef,
    pub sha256: String,
    pub byte_len: u64,
    pub base64: String,
}
impl PinnedWaysPatch {
    pub fn capture(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_WAYS_PATCH_BYTES {
            return Err(WaysDecisionError::Capacity.into());
        }
        let sha256 = sha256_hex(bytes);
        let reference = EvidenceRef::new(format!("ways-patch-{sha256}"))
            .map_err(|_| WaysArchiveError::Invalid("patch identity"))?;
        Ok(Self {
            reference,
            sha256,
            byte_len: bytes.len() as u64,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        if self.base64.len() > MAX_WAYS_PATCH_BYTES.div_ceil(3) * 4 {
            return Err(WaysDecisionError::Capacity.into());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.base64)
            .map_err(|_| WaysArchiveError::Invalid("protected patch encoding"))?;
        if Self::capture(&bytes)? != *self {
            return Err(WaysArchiveError::Invalid(
                "protected patch digest or identity changed",
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletedWaysDecision {
    pub decision_id: DecisionId,
    pub set_id: WaysSetId,
    pub deleted_at_unix_ms: u64,
    pub ownership_receipt: EvidenceRef,
}

/// The primary file of the segmented layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    schema_version: u32,
    journal_id: String,
    owner: ExecutionStoreOwner,
    limits: WaysRetentionLimits,
    segments: SegmentsMarker,
}

/// The earlier single-file layout, read only to convert it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyArchive {
    schema_version: u32,
    journal_id: String,
    owner: ExecutionStoreOwner,
    limits: WaysRetentionLimits,
    records: Vec<WaysDecisionRecord>,
    patches: Vec<PinnedWaysPatch>,
    deleted: Vec<DeletedWaysDecision>,
}

enum Primary {
    Head(Head),
    Legacy(Box<LegacyArchive>),
}

/// The digest and size of one body file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    sha256: String,
    bytes: u64,
}
impl Body {
    fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: sha256_hex(bytes),
            bytes: bytes.len() as u64,
        }
    }
}

/// A protected patch a decision pins, without its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchPin {
    reference: EvidenceRef,
    sha256: String,
    byte_len: u64,
}
impl PatchPin {
    fn of(patch: &PinnedWaysPatch) -> Self {
        Self {
            reference: patch.reference.clone(),
            sha256: patch.sha256.clone(),
            byte_len: patch.byte_len,
        }
    }
    fn body(&self) -> Body {
        Body {
            sha256: self.sha256.clone(),
            bytes: self.byte_len,
        }
    }
}

/// One state of one decision: frozen first, then each recorded progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionEntry {
    decision_id: DecisionId,
    set_id: WaysSetId,
    record: Body,
    /// Cleanup has not completed, so the decision reserves its complete
    /// configured record envelope.
    unfinished: bool,
    /// The distinct protected patches the record's candidates pin.
    patches: Vec<PatchPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Entry {
    Decision(DecisionEntry),
    Deleted(DeletedWaysDecision),
}

#[derive(Clone)]
struct Live {
    /// Sequence of the entry that froze the decision; orders `records`.
    frozen_at: u64,
    entry: DecisionEntry,
}

/// The index of retained decisions and the patches they pin.
#[derive(Clone, Default)]
struct Retained {
    decisions: HashMap<DecisionId, Live>,
    order: BTreeMap<u64, DecisionId>,
    sets: HashSet<WaysSetId>,
    /// Each pinned patch and how many retained decisions pin it.
    patches: HashMap<EvidenceRef, (PatchPin, usize)>,
}

impl Retained {
    /// Apply one decision state: a newly frozen decision, or recorded
    /// progress of a retained one. The caller has already refused a decision
    /// whose identity was deleted.
    fn record(&mut self, sequence: u64, entry: DecisionEntry) -> Result<()> {
        check_entry(&entry)?;
        if let Some(live) = self.decisions.get_mut(&entry.decision_id) {
            if live.entry.set_id != entry.set_id
                || live.entry.patches != entry.patches
                || (!live.entry.unfinished && entry.unfinished)
            {
                return Err(WaysArchiveError::Invalid("frozen decision changed"));
            }
            live.entry = entry;
            return Ok(());
        }
        if self.sets.contains(&entry.set_id) {
            return Err(WaysDecisionError::Invalid("duplicate retained decision or set").into());
        }
        for pin in &entry.patches {
            if self
                .patches
                .get(&pin.reference)
                .is_some_and(|(stored, _)| stored != pin)
            {
                return Err(WaysArchiveError::Invalid("conflicting protected patch"));
            }
        }
        for pin in &entry.patches {
            self.patches
                .entry(pin.reference.clone())
                .or_insert_with(|| (pin.clone(), 0))
                .1 += 1;
        }
        self.sets.insert(entry.set_id.clone());
        self.order.insert(sequence, entry.decision_id.clone());
        self.decisions.insert(
            entry.decision_id.clone(),
            Live {
                frozen_at: sequence,
                entry,
            },
        );
        Ok(())
    }

    /// Remove a deleted decision and return the body files no retained
    /// decision still names. A tombstone may also name a decision this index
    /// never held: the single-file layout kept tombstones without records.
    fn delete(&mut self, tombstone: &DeletedWaysDecision) -> Result<Vec<String>> {
        let Some(live) = self.decisions.remove(&tombstone.decision_id) else {
            if self.sets.contains(&tombstone.set_id) {
                return Err(WaysArchiveError::Invalid("duplicate deleted decision"));
            }
            return Ok(vec![]);
        };
        if live.entry.set_id != tombstone.set_id {
            return Err(WaysArchiveError::Invalid("duplicate deleted decision"));
        }
        self.sets.remove(&live.entry.set_id);
        self.order.remove(&live.frozen_at);
        let mut released = vec![record_file(&live.entry.record.sha256)];
        for pin in &live.entry.patches {
            let Some(slot) = self.patches.get_mut(&pin.reference) else {
                return Err(WaysArchiveError::Invalid("unowned protected artifact"));
            };
            slot.1 -= 1;
            if slot.1 == 0 {
                self.patches.remove(&pin.reference);
                released.push(patch_file(&pin.sha256));
            }
        }
        Ok(released)
    }

    /// The retention limits bound the retained decisions, the patches they
    /// pin (each once) and the reservation of every unfinished decision.
    fn check_capacity(&self, limits: WaysRetentionLimits) -> Result<()> {
        if self.decisions.len() > limits.records {
            return Err(WaysDecisionError::Capacity.into());
        }
        let mut total = 0usize;
        for live in self.decisions.values() {
            let bytes = usize::try_from(live.entry.record.bytes)
                .map_err(|_| WaysDecisionError::Capacity)?;
            // An unfinished decision holds its complete configured envelope,
            // so its later application, link and cleanup receipts never
            // compete with newly frozen decisions.
            let held = if live.entry.unfinished {
                bytes.max(limits.record_bytes)
            } else {
                bytes
            };
            total = total.checked_add(held).ok_or(WaysDecisionError::Capacity)?;
        }
        for (pin, _) in self.patches.values() {
            let bytes = usize::try_from(pin.byte_len).map_err(|_| WaysDecisionError::Capacity)?;
            total = total
                .checked_add(bytes)
                .ok_or(WaysDecisionError::Capacity)?;
        }
        if total > limits.aggregate_bytes {
            return Err(WaysDecisionError::Capacity.into());
        }
        Ok(())
    }

    /// The names of every body file a retained decision names.
    fn body_files(&self) -> HashSet<String> {
        self.decisions
            .values()
            .map(|live| record_file(&live.entry.record.sha256))
            .chain(
                self.patches
                    .values()
                    .map(|(pin, _)| patch_file(&pin.sha256)),
            )
            .collect()
    }
}

/// Rebuilds the index while the log is opened.
#[derive(Default)]
struct Replay {
    retained: Retained,
    /// Keys of every tombstone, held only while opening.
    deleted: HashSet<String>,
    tombstones: Vec<(u64, DeletedWaysDecision)>,
}

impl Replay {
    fn apply(&mut self, sequence: u64, entry: Entry) -> Result<()> {
        match entry {
            Entry::Decision(entry) => {
                if !self.retained.decisions.contains_key(&entry.decision_id)
                    && (self.deleted.contains(&decision_key(&entry.decision_id))
                        || self.deleted.contains(&set_key(&entry.set_id)))
                {
                    return Err(WaysArchiveError::Invalid(
                        "a deleted decision cannot be recreated",
                    ));
                }
                self.retained.record(sequence, entry)
            }
            Entry::Deleted(tombstone) => {
                let [decision, set] = tombstone_keys(&tombstone);
                if !self.deleted.insert(decision) || !self.deleted.insert(set) {
                    return Err(WaysArchiveError::Invalid("duplicate deleted decision"));
                }
                self.retained.delete(&tombstone)?;
                self.tombstones.push((sequence, tombstone));
                Ok(())
            }
        }
    }
}

/// Deletion tombstones: those of the active segment in memory, and a key
/// filter for each sealed segment that holds any.
#[derive(Default)]
struct Tombstones {
    active: Vec<DeletedWaysDecision>,
    sealed: Vec<(usize, KeyFilter)>,
}

impl Tombstones {
    fn index(all: Vec<(u64, DeletedWaysDecision)>, log: &SegmentLog) -> Self {
        let mut result = Self::default();
        let mut group: Option<(usize, Vec<String>)> = None;
        for (sequence, tombstone) in all {
            let Some(segment) = log.segment_of(sequence) else {
                result.active.push(tombstone);
                continue;
            };
            let index = segment.index as usize;
            match &mut group {
                Some((at, keys)) if *at == index => keys.extend(tombstone_keys(&tombstone)),
                _ => {
                    result.flush(group.take());
                    group = Some((index, tombstone_keys(&tombstone).into()));
                }
            }
        }
        result.flush(group);
        result
    }

    fn flush(&mut self, group: Option<(usize, Vec<String>)>) {
        if let Some((index, keys)) = group {
            self.sealed
                .push((index, KeyFilter::new(keys.iter().map(String::as_str))));
        }
    }

    /// The active segment was sealed as segment `index`.
    fn sealed_as(&mut self, index: usize) {
        if !self.active.is_empty() {
            let keys: Vec<String> = self.active.iter().flat_map(tombstone_keys).collect();
            self.flush(Some((index, keys)));
            self.active.clear();
        }
    }
}

pub struct WaysDecisionStore {
    namespace: OwnedExecutionNamespace,
    evidence: OwnedExecutionNamespace,
    head: Head,
    log: SegmentLog,
    retained: Retained,
    tombstones: Tombstones,
    cache: SegmentCache<Entry>,
    poisoned: bool,
}
impl WaysDecisionStore {
    pub fn open_configured(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
    ) -> Result<Option<Self>> {
        Self::open_configured_with(namespace, canonical, SPEC)
    }
    fn open_configured_with(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        spec: SegmentSpec,
    ) -> Result<Option<Self>> {
        namespace.require_root(&ExecutionComponent::WaysDecisions)?;
        require_canonical(&namespace, canonical)?;
        match read_primary(&namespace)? {
            Some(primary) => Self::open_existing(namespace, primary, None, spec).map(Some),
            None => {
                namespace.check_journal_creation(WAYS_ARCHIVE_FILE)?;
                Ok(None)
            }
        }
    }
    pub fn limits(&self) -> WaysRetentionLimits {
        self.head.limits
    }
    pub fn configure_limits(&mut self, limits: WaysRetentionLimits) -> Result<()> {
        self.healthy()?;
        validate_ways_retention(&[], limits)?;
        // The same validation covers every retained record, protected patch
        // and unfinished reservation. Reconfiguration never evicts.
        for live in self.retained.decisions.values() {
            self.read_record(&live.entry, limits)?;
        }
        self.retained.check_capacity(limits)?;
        if limits == self.head.limits {
            return Ok(());
        }
        let mut head = self.head.clone();
        head.limits = limits;
        let encoded = serde_json::to_vec(&head)?;
        self.poisoned = true;
        self.namespace.atomic_write(WAYS_ARCHIVE_FILE, &encoded)?;
        self.head = head;
        self.poisoned = false;
        Ok(())
    }
    /// Limits are explicit host configuration. Reopening never silently reduces
    /// them or evicts evidence to accommodate a newer configuration.
    pub fn open_owned(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        limits: WaysRetentionLimits,
    ) -> Result<Self> {
        Self::open_owned_with(namespace, canonical, limits, SPEC)
    }
    fn open_owned_with(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        limits: WaysRetentionLimits,
        spec: SegmentSpec,
    ) -> Result<Self> {
        namespace.require_root(&ExecutionComponent::WaysDecisions)?;
        require_canonical(&namespace, canonical)?;
        validate_ways_retention(&[], limits)?;
        match read_primary(&namespace)? {
            Some(primary) => Self::open_existing(namespace, primary, Some(limits), spec),
            None => Self::create(namespace, limits, spec),
        }
    }
    fn create(
        namespace: OwnedExecutionNamespace,
        limits: WaysRetentionLimits,
        spec: SegmentSpec,
    ) -> Result<Self> {
        namespace.check_journal_creation(WAYS_ARCHIVE_FILE)?;
        let head = Head {
            schema_version: 1,
            journal_id: namespace.identity().journal_id().into(),
            owner: namespace.identity().owner().clone(),
            limits,
            segments: SegmentsMarker::of(&spec),
        };
        let encoded = serde_json::to_vec(&head)?;
        // Marker first: an interrupted first open stays fail-closed instead
        // of later looking like a new, empty store.
        namespace.mark_journal_initialized(WAYS_ARCHIVE_FILE)?;
        let evidence = namespace.child(EVIDENCE_DIR)?;
        let log = SegmentLog::open(
            namespace.secure_dir()?,
            spec,
            log_meta(&namespace),
            true,
            |_, _: Entry| Ok::<(), WaysArchiveError>(()),
        )?;
        namespace.atomic_write(WAYS_ARCHIVE_FILE, &encoded)?;
        Ok(Self {
            namespace,
            evidence,
            head,
            log,
            retained: Retained::default(),
            tombstones: Tombstones::default(),
            cache: SegmentCache::new(CACHED_SEGMENTS),
            poisoned: false,
        })
    }
    fn open_existing(
        namespace: OwnedExecutionNamespace,
        primary: Primary,
        expected: Option<WaysRetentionLimits>,
        spec: SegmentSpec,
    ) -> Result<Self> {
        let head = match primary {
            Primary::Head(head) => head,
            Primary::Legacy(legacy) => {
                check_identity(
                    &namespace,
                    legacy.schema_version,
                    &legacy.owner,
                    &legacy.journal_id,
                    legacy.limits,
                    expected,
                )?;
                convert_legacy(&namespace, *legacy, spec)?;
                match read_primary(&namespace)? {
                    Some(Primary::Head(head)) => head,
                    _ => {
                        return Err(WaysArchiveError::Invalid(
                            "converted Ways evidence head is unavailable",
                        ))
                    }
                }
            }
        };
        check_identity(
            &namespace,
            head.schema_version,
            &head.owner,
            &head.journal_id,
            head.limits,
            expected,
        )?;
        if !head.segments.matches(&spec) {
            return Err(WaysArchiveError::Invalid(
                "archive identity, version or configured limits changed",
            ));
        }
        let evidence = namespace.child(EVIDENCE_DIR)?;
        let mut replay = Replay::default();
        let log = SegmentLog::open(
            namespace.secure_dir()?,
            spec,
            log_meta(&namespace),
            false,
            |sequence, entry: Entry| replay.apply(sequence, entry),
        )?;
        let Replay {
            retained,
            tombstones,
            ..
        } = replay;
        retained.check_capacity(head.limits)?;
        let tombstones = Tombstones::index(tombstones, &log);
        let store = Self {
            namespace,
            evidence,
            head,
            log,
            retained,
            tombstones,
            cache: SegmentCache::new(CACHED_SEGMENTS),
            poisoned: false,
        };
        store.verify_bodies()?;
        store
            .namespace
            .mark_journal_initialized(WAYS_ARCHIVE_FILE)?;
        store.remove_unretained_bodies()?;
        Ok(store)
    }
    /// Every retained decision, in the order it was frozen. Each record is
    /// read from disk and checked against its digest.
    pub fn records(&self) -> Result<Vec<WaysDecisionRecord>> {
        self.healthy()?;
        self.retained
            .order
            .values()
            .map(|id| {
                let live = self
                    .retained
                    .decisions
                    .get(id)
                    .ok_or(WaysArchiveError::Invalid("retained decision index"))?;
                self.read_record(&live.entry, self.head.limits)
            })
            .collect()
    }
    /// Every deletion tombstone, oldest first. Tombstones in sealed segments
    /// are read back from disk.
    pub fn deleted(&self) -> Result<Vec<DeletedWaysDecision>> {
        self.healthy()?;
        let mut deleted = Vec::new();
        for (index, _) in &self.tombstones.sealed {
            let segment = self
                .log
                .sealed()
                .get(*index)
                .ok_or(WaysArchiveError::Invalid("tombstone segment index"))?;
            for entry in self.cache.get(&self.log, segment)?.iter() {
                if let Entry::Deleted(tombstone) = entry.as_ref() {
                    deleted.push(tombstone.clone());
                }
            }
        }
        deleted.extend(self.tombstones.active.iter().cloned());
        Ok(deleted)
    }
    /// The tombstone of an explicitly deleted decision, if it was deleted.
    pub fn deleted_decision(&self, id: &DecisionId) -> Result<Option<DeletedWaysDecision>> {
        self.healthy()?;
        self.find_deleted(&[decision_key(id)], |tombstone| {
            tombstone.decision_id == *id
        })
    }
    pub fn get(&self, id: &DecisionId) -> Result<Option<WaysDecisionRecord>> {
        self.healthy()?;
        self.retained
            .decisions
            .get(id)
            .map(|live| self.read_record(&live.entry, self.head.limits))
            .transpose()
    }
    pub fn patch(&self, reference: &EvidenceRef) -> Result<Option<Vec<u8>>> {
        self.healthy()?;
        self.retained
            .patches
            .get(reference)
            .map(|(pin, _)| self.read_body(&patch_file(&pin.sha256), &pin.body()))
            .transpose()
    }
    /// All candidate evidence and patch bodies are durable together before the
    /// caller can begin applying a selected candidate or disposing any clone.
    pub fn freeze(
        &mut self,
        record: WaysDecisionRecord,
        patches: Vec<PinnedWaysPatch>,
    ) -> Result<()> {
        self.healthy()?;
        if let Some(saved) = self.get(&record.decision_id)? {
            for patch in &patches {
                patch.bytes()?;
                if self
                    .retained
                    .patches
                    .get(&patch.reference)
                    .map(|(pin, _)| pin)
                    != Some(&PatchPin::of(patch))
                {
                    return Err(WaysArchiveError::Invalid(
                        "retry supplied different protected patch bytes",
                    ));
                }
            }
            return if frozen_record(&saved) == frozen_record(&record) {
                Ok(())
            } else {
                Err(WaysArchiveError::Invalid(
                    "decision identity was reused with different evidence",
                ))
            };
        }
        if self
            .find_deleted(
                &[decision_key(&record.decision_id), set_key(&record.set_id)],
                |item| item.decision_id == record.decision_id || item.set_id == record.set_id,
            )?
            .is_some()
        {
            return Err(WaysArchiveError::Invalid(
                "a deleted decision cannot be recreated",
            ));
        }
        if !matches!(record.application, WaysApplicationOutcome::NotStarted)
            || record.selected_session_turn.is_some()
            || record.cleanup.completed_at_unix_ms.is_some()
            || record
                .cleanup
                .targets
                .iter()
                .any(|target| !matches!(target.outcome, WaysCleanupOutcome::Pending))
        {
            return Err(WaysArchiveError::Invalid(
                "initial evidence already claims application or cleanup",
            ));
        }
        let limits = self.head.limits;
        require_completion_capacity(&record, limits.record_bytes)?;
        validate_ways_retention(std::slice::from_ref(&record), limits)?;
        if record.session_id != self.namespace.identity().owner().session_id {
            return Err(WaysArchiveError::Invalid("foreign Session decision"));
        }
        // Supplied patches are intact and never conflict with a pinned one.
        // Each is decoded again only when it is written, one at a time.
        let mut supplied: Vec<(PatchPin, &PinnedWaysPatch)> = Vec::new();
        for patch in &patches {
            patch.bytes()?;
            let pin = PatchPin::of(patch);
            let known = self
                .retained
                .patches
                .get(&pin.reference)
                .map(|(stored, _)| stored)
                .or(supplied
                    .iter()
                    .map(|(item, _)| item)
                    .find(|item| item.reference == pin.reference));
            match known {
                Some(known) if *known != pin => {
                    return Err(WaysArchiveError::Invalid("conflicting protected patch"))
                }
                Some(_) => (),
                None => supplied.push((pin, patch)),
            }
        }
        // Every patch the record names is pinned exactly, and nothing new is
        // pinned that the record does not name.
        let pins = record_pins(&record);
        for pin in &pins {
            let found = self
                .retained
                .patches
                .get(&pin.reference)
                .map(|(stored, _)| stored)
                .or(supplied
                    .iter()
                    .map(|(item, _)| item)
                    .find(|item| item.reference == pin.reference))
                .ok_or(WaysArchiveError::Invalid(
                    "required protected patch missing",
                ))?;
            if found != pin {
                return Err(WaysArchiveError::Invalid(
                    "candidate patch identity mismatch",
                ));
            }
        }
        if supplied.iter().any(|(pin, _)| !pins.contains(pin)) {
            return Err(WaysArchiveError::Invalid("unowned protected artifact"));
        }
        let body = serde_json::to_vec(&record)?;
        let entry = DecisionEntry {
            decision_id: record.decision_id.clone(),
            set_id: record.set_id.clone(),
            record: Body::of(&body),
            unfinished: record.cleanup.completed_at_unix_ms.is_none(),
            patches: pins,
        };
        let mut next = self.retained.clone();
        next.record(self.log.next_sequence(), entry.clone())?;
        next.check_capacity(limits)?;
        let line = self.log.encode_record(&Entry::Decision(entry.clone()))?;
        self.poisoned = true;
        for (pin, patch) in &supplied {
            self.evidence
                .atomic_write(patch_file(&pin.sha256), &patch.bytes()?)?;
        }
        self.evidence
            .atomic_write(record_file(&entry.record.sha256), &body)?;
        self.append_line(&line, None)?;
        self.retained = next;
        self.poisoned = false;
        Ok(())
    }
    /// Only monotonic execution/cleanup evidence can change. Candidate bytes,
    /// user choice and targets remain the exact previously pinned decision.
    pub fn record_progress(&mut self, record: WaysDecisionRecord) -> Result<()> {
        self.healthy()?;
        let saved_entry = self
            .retained
            .decisions
            .get(&record.decision_id)
            .map(|live| live.entry.clone())
            .ok_or(WaysArchiveError::Invalid(
                "decision evidence was not frozen",
            ))?;
        let limits = self.head.limits;
        let saved = self.read_record(&saved_entry, limits)?;
        if frozen_record(&saved) != frozen_record(&record) {
            return Err(WaysArchiveError::Invalid("frozen decision changed"));
        }
        validate_progress(&saved, &record)?;
        validate_ways_retention(std::slice::from_ref(&record), limits)?;
        let body = serde_json::to_vec(&record)?;
        let entry = DecisionEntry {
            record: Body::of(&body),
            unfinished: record.cleanup.completed_at_unix_ms.is_none(),
            ..saved_entry.clone()
        };
        if entry == saved_entry {
            return Ok(());
        }
        let mut next = self.retained.clone();
        next.record(self.log.next_sequence(), entry.clone())?;
        next.check_capacity(limits)?;
        let line = self.log.encode_record(&Entry::Decision(entry.clone()))?;
        self.poisoned = true;
        self.evidence
            .atomic_write(record_file(&entry.record.sha256), &body)?;
        self.append_line(&line, None)?;
        self.retained = next;
        self.poisoned = false;
        self.release(&[record_file(&saved_entry.record.sha256)]);
        Ok(())
    }
    /// The host must verify all external reference ownership before invoking
    /// this explicit deletion. The tombstone survives patch release so History
    /// and reference resolvers can distinguish deletion from missing storage.
    pub fn delete_verified<F>(&mut self, id: &DecisionId, at: u64, verify: F) -> Result<()>
    where
        F: FnOnce(&WaysDecisionRecord) -> Result<EvidenceRef>,
    {
        self.healthy()?;
        let entry = self
            .retained
            .decisions
            .get(id)
            .map(|live| live.entry.clone())
            .ok_or(WaysArchiveError::Invalid("decision not found"))?;
        let record = self.read_record(&entry, self.head.limits)?;
        if record.cleanup.completed_at_unix_ms.is_none() {
            return Err(WaysArchiveError::Invalid(
                "decision still owns disposable resources",
            ));
        }
        let receipt = verify(&record)?;
        let tombstone = DeletedWaysDecision {
            decision_id: record.decision_id,
            set_id: record.set_id,
            deleted_at_unix_ms: at,
            ownership_receipt: receipt,
        };
        let mut next = self.retained.clone();
        let released = next.delete(&tombstone)?;
        let line = self.log.encode_record(&Entry::Deleted(tombstone.clone()))?;
        self.poisoned = true;
        self.append_line(&line, Some(tombstone))?;
        self.retained = next;
        self.poisoned = false;
        self.release(&released);
        Ok(())
    }
    fn healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(WaysArchiveError::RecoveryRequired);
        }
        self.namespace.verify_ambient_identity()?;
        Ok(())
    }
    /// Append one synced index line, sealing the active segment when full.
    fn append_line(&mut self, line: &[u8], tombstone: Option<DeletedWaysDecision>) -> Result<()> {
        self.log.append_line(line)?;
        if let Some(tombstone) = tombstone {
            self.tombstones.active.push(tombstone);
        }
        if self.log.should_seal() {
            let index = self.log.sealed().len();
            self.log.seal()?;
            self.tombstones.sealed_as(index);
        }
        Ok(())
    }
    /// Remove body files that no retained decision names any longer. A file
    /// left behind by a failure here is removed when the store next opens.
    fn release(&self, names: &[String]) {
        let Ok(dir) = self.evidence.secure_dir() else {
            return;
        };
        for name in names {
            let _ = dir.remove_file(name);
        }
        let _ = dir.sync_all();
    }
    fn find_deleted(
        &self,
        keys: &[String],
        matches: impl Fn(&DeletedWaysDecision) -> bool,
    ) -> Result<Option<DeletedWaysDecision>> {
        if let Some(found) = self.tombstones.active.iter().find(|item| matches(item)) {
            return Ok(Some(found.clone()));
        }
        for (index, filter) in &self.tombstones.sealed {
            if !keys.iter().any(|key| filter.may_contain(key)) {
                continue;
            }
            let segment = self
                .log
                .sealed()
                .get(*index)
                .ok_or(WaysArchiveError::Invalid("tombstone segment index"))?;
            for entry in self.cache.get(&self.log, segment)?.iter() {
                if let Entry::Deleted(tombstone) = entry.as_ref() {
                    if matches(tombstone) {
                        return Ok(Some(tombstone.clone()));
                    }
                }
            }
        }
        Ok(None)
    }
    fn read_body(&self, name: &str, body: &Body) -> Result<Vec<u8>> {
        let len = usize::try_from(body.bytes)
            .ok()
            .filter(|len| *len <= MAX_RECORD_BODY_BYTES.max(MAX_WAYS_PATCH_BYTES))
            .ok_or(WaysArchiveError::Invalid(
                "retained Ways evidence is too large",
            ))?;
        let bytes = match self.evidence.read_limited(name, len) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(WaysArchiveError::Invalid(
                    "retained Ways evidence is missing",
                ))
            }
            Err(error) => return Err(error.into()),
        };
        if bytes.len() != len || sha256_hex(&bytes) != body.sha256 {
            return Err(WaysArchiveError::Invalid(
                "retained Ways evidence changed on disk",
            ));
        }
        Ok(bytes)
    }
    /// Read one retained record and require that it is exactly the decision
    /// its index entry names, valid under `limits`.
    fn read_record(
        &self,
        entry: &DecisionEntry,
        limits: WaysRetentionLimits,
    ) -> Result<WaysDecisionRecord> {
        let bytes = self.read_body(&record_file(&entry.record.sha256), &entry.record)?;
        let record: WaysDecisionRecord = serde_json::from_slice(&bytes)?;
        validate_ways_retention(std::slice::from_ref(&record), limits)?;
        if record.session_id != self.namespace.identity().owner().session_id {
            return Err(WaysArchiveError::Invalid("foreign Session decision"));
        }
        if record.decision_id != entry.decision_id
            || record.set_id != entry.set_id
            || record.cleanup.completed_at_unix_ms.is_none() != entry.unfinished
            || record_pins(&record) != entry.patches
        {
            return Err(WaysArchiveError::Invalid(
                "retained decision differs from its index",
            ));
        }
        Ok(record)
    }
    /// Every body a retained decision names exists with its recorded size.
    /// Digests are checked whenever a body is read.
    fn verify_bodies(&self) -> Result<()> {
        let present = |name: &str, bytes: u64| match self.evidence.file_len(name) {
            Ok(len) => Ok(len == bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(WaysArchiveError::from(error)),
        };
        for live in self.retained.decisions.values() {
            let body = &live.entry.record;
            if !present(&record_file(&body.sha256), body.bytes)? {
                return Err(WaysArchiveError::Invalid(
                    "retained decision evidence is missing",
                ));
            }
        }
        for (pin, _) in self.retained.patches.values() {
            if !present(&patch_file(&pin.sha256), pin.byte_len)? {
                return Err(WaysArchiveError::Invalid(
                    "required protected patch missing",
                ));
            }
        }
        Ok(())
    }
    /// Finish interrupted writes and deletions: remove every body file that
    /// no retained decision names. Nothing named by the index is touched.
    fn remove_unretained_bodies(&self) -> Result<()> {
        let retained = self.retained.body_files();
        let dir = self.evidence.secure_dir()?;
        let mut removed = false;
        for entry in dir.entries()? {
            let name = entry
                .name
                .to_str()
                .filter(|name| entry.file_type == SecureEntryType::File && is_body_file(name))
                .ok_or(WaysArchiveError::Invalid(
                    "unexpected entry among Ways evidence",
                ))?;
            if !retained.contains(name) {
                dir.remove_file(name)?;
                removed = true;
            }
        }
        if removed {
            dir.sync_all()?;
        }
        Ok(())
    }
}

fn require_canonical(
    namespace: &OwnedExecutionNamespace,
    canonical: &SessionExecutionStore,
) -> Result<()> {
    if canonical
        .identity()
        .map_err(|_| WaysArchiveError::Invalid("canonical owner unavailable"))?
        != *namespace.identity()
    {
        return Err(WaysArchiveError::Invalid("foreign canonical owner"));
    }
    Ok(())
}

fn check_identity(
    namespace: &OwnedExecutionNamespace,
    schema_version: u32,
    owner: &ExecutionStoreOwner,
    journal_id: &str,
    limits: WaysRetentionLimits,
    expected: Option<WaysRetentionLimits>,
) -> Result<()> {
    validate_ways_retention(&[], limits)?;
    if schema_version != 1
        || owner != namespace.identity().owner()
        || journal_id != namespace.identity().journal_id()
        || expected.is_some_and(|expected| expected != limits)
    {
        return Err(WaysArchiveError::Invalid(
            "archive identity, version or configured limits changed",
        ));
    }
    Ok(())
}

fn read_primary(namespace: &OwnedExecutionNamespace) -> Result<Option<Primary>> {
    let bytes = match namespace.read_limited(WAYS_ARCHIVE_FILE, MAX_LEGACY_ARCHIVE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    #[derive(Deserialize)]
    struct Layout {
        #[serde(default)]
        segments: Option<serde::de::IgnoredAny>,
    }
    let layout: Layout = serde_json::from_slice(&bytes)?;
    Ok(Some(if layout.segments.is_some() {
        Primary::Head(serde_json::from_slice(&bytes)?)
    } else {
        Primary::Legacy(Box::new(serde_json::from_slice(&bytes)?))
    }))
}

fn log_meta(namespace: &OwnedExecutionNamespace) -> serde_json::Value {
    serde_json::json!({
        "journal_id": namespace.identity().journal_id(),
        "owner": namespace.identity().owner(),
    })
}

/// Convert a single-file store. Its content is validated exactly as before,
/// then written as bodies and index entries; the head replaces the single
/// file last, so an interrupted conversion is redone from the intact file.
fn convert_legacy(
    namespace: &OwnedExecutionNamespace,
    legacy: LegacyArchive,
    spec: SegmentSpec,
) -> Result<()> {
    validate_legacy(&legacy, namespace.identity().owner())?;
    // Unmarked legacy data acquires its marker only after validation.
    namespace.mark_journal_initialized(WAYS_ARCHIVE_FILE)?;
    let dir = namespace.secure_dir()?;
    // Whatever an interrupted conversion left is produced again below.
    SegmentLog::remove(&dir, &spec)?;
    if dir.has_exact_directory(EVIDENCE_DIR)? {
        dir.remove_dir_all(EVIDENCE_DIR)?;
        dir.sync_all()?;
    }
    let evidence = namespace.child(EVIDENCE_DIR)?;
    for patch in &legacy.patches {
        evidence.atomic_write(patch_file(&patch.sha256), &patch.bytes()?)?;
    }
    let mut log = SegmentLog::open(dir, spec, log_meta(namespace), true, |_, _: Entry| {
        Ok::<(), WaysArchiveError>(())
    })?;
    let append = |log: &mut SegmentLog, entry: Entry| -> Result<()> {
        let line = log.encode_record(&entry)?;
        log.append_line(&line)?;
        if log.should_seal() {
            log.seal()?;
        }
        Ok(())
    };
    // The single file kept tombstones apart from records; their order
    // relative to the retained records was not recorded.
    for tombstone in legacy.deleted {
        append(&mut log, Entry::Deleted(tombstone))?;
    }
    for record in &legacy.records {
        let body = serde_json::to_vec(record)?;
        let entry = DecisionEntry {
            decision_id: record.decision_id.clone(),
            set_id: record.set_id.clone(),
            record: Body::of(&body),
            unfinished: record.cleanup.completed_at_unix_ms.is_none(),
            patches: record_pins(record),
        };
        evidence.atomic_write(record_file(&entry.record.sha256), &body)?;
        append(&mut log, Entry::Decision(entry))?;
    }
    drop(log);
    let head = Head {
        schema_version: 1,
        journal_id: legacy.journal_id,
        owner: legacy.owner,
        limits: legacy.limits,
        segments: SegmentsMarker::of(&spec),
    };
    namespace.atomic_write(WAYS_ARCHIVE_FILE, &serde_json::to_vec(&head)?)?;
    Ok(())
}

/// The single-file layout's validation of its records, patches and
/// tombstones. Capacity is checked by the segmented store once converted.
fn validate_legacy(archive: &LegacyArchive, owner: &ExecutionStoreOwner) -> Result<()> {
    validate_ways_retention(&archive.records, archive.limits)?;
    let mut references = HashSet::new();
    for patch in &archive.patches {
        if !references.insert(&patch.reference) {
            return Err(WaysArchiveError::Invalid("duplicate patch"));
        }
        patch.bytes()?;
    }
    let mut ids = HashSet::new();
    let mut sets = HashSet::new();
    let mut used = HashSet::new();
    for record in &archive.records {
        if record.session_id != owner.session_id {
            return Err(WaysArchiveError::Invalid("foreign Session decision"));
        }
        ids.insert(&record.decision_id);
        sets.insert(&record.set_id);
        for expected in record_patches(record) {
            let patch = archive
                .patches
                .iter()
                .find(|patch| patch.reference == expected.protected_artifact_ref)
                .ok_or(WaysArchiveError::Invalid(
                    "required protected patch missing",
                ))?;
            if patch.sha256 != expected.patch_sha256 || patch.byte_len != expected.patch_bytes {
                return Err(WaysArchiveError::Invalid(
                    "candidate patch identity mismatch",
                ));
            }
            used.insert(&patch.reference);
        }
    }
    if used != references {
        return Err(WaysArchiveError::Invalid("unowned protected artifact"));
    }
    for deleted in &archive.deleted {
        if !ids.insert(&deleted.decision_id) || !sets.insert(&deleted.set_id) {
            return Err(WaysArchiveError::Invalid("duplicate deleted decision"));
        }
    }
    Ok(())
}

fn record_patches(record: &WaysDecisionRecord) -> impl Iterator<Item = &ProtectedWaysPatch> {
    record
        .candidates
        .iter()
        .filter_map(|candidate| match &candidate.patch {
            Recorded::Available { value } => Some(value),
            _ => None,
        })
}
/// The distinct patch pins a record's candidates name, in candidate order.
fn record_pins(record: &WaysDecisionRecord) -> Vec<PatchPin> {
    let mut pins: Vec<PatchPin> = Vec::new();
    for patch in record_patches(record) {
        let pin = PatchPin {
            reference: patch.protected_artifact_ref.clone(),
            sha256: patch.patch_sha256.clone(),
            byte_len: patch.patch_bytes,
        };
        if !pins.contains(&pin) {
            pins.push(pin);
        }
    }
    pins
}
/// The shape every index entry must have, whoever wrote it.
fn check_entry(entry: &DecisionEntry) -> Result<()> {
    if !is_digest(&entry.record.sha256)
        || entry.record.bytes == 0
        || entry.record.bytes > MAX_RECORD_BODY_BYTES as u64
    {
        return Err(WaysArchiveError::Invalid("retained decision body"));
    }
    let mut references = HashSet::new();
    for pin in &entry.patches {
        if !references.insert(&pin.reference) {
            return Err(WaysArchiveError::Invalid("conflicting protected patch"));
        }
        if !is_digest(&pin.sha256)
            || pin.reference.as_str() != format!("ways-patch-{}", pin.sha256)
            || pin.byte_len > MAX_WAYS_PATCH_BYTES as u64
        {
            return Err(WaysArchiveError::Invalid(
                "candidate patch identity mismatch",
            ));
        }
    }
    Ok(())
}
fn record_file(sha256: &str) -> String {
    format!("decision-{sha256}.json")
}
fn patch_file(sha256: &str) -> String {
    format!("patch-{sha256}.bin")
}
/// A body file name, or the temporary file of an atomic write that never
/// published (it holds nothing acknowledged).
fn is_body_file(name: &str) -> bool {
    let digest_between = |prefix: &str, suffix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .is_some_and(is_digest)
    };
    digest_between("decision-", ".json")
        || digest_between("patch-", ".bin")
        || (name.starts_with(".axocoatl-") && name.ends_with(".tmp"))
}
fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn decision_key(id: &DecisionId) -> String {
    format!("decision:{}", id.0.as_str())
}
fn set_key(id: &WaysSetId) -> String {
    format!("set:{}", id.0.as_str())
}
fn tombstone_keys(tombstone: &DeletedWaysDecision) -> [String; 2] {
    [
        decision_key(&tombstone.decision_id),
        set_key(&tombstone.set_id),
    ]
}
/// Check the largest successful completion metadata before permitting any
/// apply/cleanup. This temporary shape is never persisted as execution evidence.
fn require_completion_capacity(record: &WaysDecisionRecord, limit: usize) -> Result<()> {
    let mut completed = record.clone();
    let reference = EvidenceRef::new("r".repeat(128))
        .map_err(|_| WaysArchiveError::Invalid("completion reserve identity"))?;
    completed.application = match &record.human_decision.choice {
        WaysHumanChoice::NoKeep => WaysApplicationOutcome::NoKeepRecorded {
            receipt_ref: reference.clone(),
            recorded_at_unix_ms: u64::MAX,
        },
        WaysHumanChoice::Keep { patch } => {
            completed.selected_session_turn = Some(WaysSelectedSessionTurn {
                session_id: record.session_id.clone(),
                turn_id: crate::turn_contract::LogicalTurnId::new("t".repeat(128))
                    .map_err(|_| WaysArchiveError::Invalid("completion reserve turn"))?,
                transcript_receipt_ref: reference.clone(),
            });
            WaysApplicationOutcome::Applied {
                identity: WaysApplicationIdentity {
                    operation_id: reference.clone(),
                    patch: patch.clone(),
                    preimage_tree_oid: "f".repeat(40),
                    postimage_tree_oid: "f".repeat(40),
                },
                receipt_ref: reference.clone(),
                applied_at_unix_ms: u64::MAX,
            }
        }
    };
    for target in &mut completed.cleanup.targets {
        target.outcome = WaysCleanupOutcome::Completed {
            receipt_ref: reference.clone(),
            completed_at_unix_ms: u64::MAX,
        };
    }
    completed.cleanup.completed_at_unix_ms = Some(u64::MAX);
    if serde_json::to_vec(&completed)?.len() > limit {
        return Err(WaysDecisionError::Capacity.into());
    }
    Ok(())
}

fn frozen_record(record: &WaysDecisionRecord) -> WaysDecisionRecord {
    let mut frozen = record.clone();
    frozen.application = WaysApplicationOutcome::NotStarted;
    frozen.selected_session_turn = None;
    frozen.cleanup.completed_at_unix_ms = None;
    for target in &mut frozen.cleanup.targets {
        target.outcome = WaysCleanupOutcome::Pending;
    }
    frozen
}
fn validate_progress(saved: &WaysDecisionRecord, next: &WaysDecisionRecord) -> Result<()> {
    use WaysApplicationOutcome::*;
    let identity = |state: &WaysApplicationOutcome| match state {
        Pending { identity }
        | Failed { identity, .. }
        | ReconciliationRequired { identity, .. }
        | Applied { identity, .. } => Some(identity.clone()),
        _ => None,
    };
    let valid = match (&saved.application, &next.application) {
        (NotStarted, NotStarted | Pending { .. } | NoKeepRecorded { .. }) => true,
        (
            Pending { .. } | Failed { .. } | ReconciliationRequired { .. },
            Pending { .. } | Failed { .. } | ReconciliationRequired { .. } | Applied { .. },
        ) => identity(&saved.application) == identity(&next.application),
        (Applied { .. }, Applied { .. }) | (NoKeepRecorded { .. }, NoKeepRecorded { .. }) => {
            saved.application == next.application
        }
        _ => false,
    };
    if !valid
        || (saved.selected_session_turn.is_some()
            && saved.selected_session_turn != next.selected_session_turn)
        || (saved.cleanup.completed_at_unix_ms.is_some() && saved.cleanup != next.cleanup)
    {
        return Err(WaysArchiveError::Invalid(
            "application or cleanup moved backwards",
        ));
    }
    for (before, after) in saved.cleanup.targets.iter().zip(&next.cleanup.targets) {
        if matches!(before.outcome, WaysCleanupOutcome::Completed { .. })
            && before.outcome != after.outcome
        {
            return Err(WaysArchiveError::Invalid("completed cleanup changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "ways_decision_store_tests.rs"]
mod tests;
