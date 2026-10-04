//! Canonical v2 logical-turn history under a held format-ownership boundary.
//!
//! One atomic Session journal owns all of its turn identities. It prevents a
//! second unfinished turn, resolves closed predecessors against retained history,
//! and reserves bounded settlement space before admitting more work. Reopening
//! durably interrupts a running epoch; it never resumes dispatch automatically.
//! This storage capability is not proof that orphaned external work is settled.
//! The live daemon must establish execution readiness separately before using it.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::segment_log::{
    KeyFilter, SegmentCache, SegmentError, SegmentLog, SegmentSpec, SegmentsMarker,
};

use crate::execution_content::{
    DurableExecutionRequest, DurableLegacyHistory, LegacyTurnPredecessor,
};
use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_ownership::{OwnershipError, UpgradedFormatOwnership};
use crate::turn_contract::{
    ActivationState, ClosedTurnRef, CommandId, EffectDisposition, EvidenceRef, LogicalTurnId,
    LogicalTurnState, SessionId, TurnContract, TurnContractEnvelope, TurnContractError,
    TurnContractEvent, MAX_CONTRACT_COMMANDS, MAX_CONTRACT_ENVELOPE_BYTES,
    MAX_RETAINED_CONTRACT_BYTES, TURN_CONTRACT_SCHEMA_VERSION,
};

const FILE: &str = "execution.v2.json";
/// Bounds of the single-file layout written before segmentation, which such
/// a journal still meets when it is read and migrated.
const LEGACY_MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;
const LEGACY_MAX_SESSION_RECORDS: usize = 65_536;
/// Settlement contains bounded identities/references, never raw tool output.
const SMALL_SETTLEMENT_BYTES: usize = 16 * 1024;
/// The records live in a segment log beside the head file; a Session holds
/// any number of turns. Per-turn bounds stay those of the turn contract.
const SPEC: SegmentSpec = SegmentSpec {
    name: "execution",
    kind: "execution.v2",
    segment_bytes: 4 * 1024 * 1024,
    // Unit tests seal often so that every path crosses segments.
    segment_records: if cfg!(test) { 8 } else { 8192 },
    // A record holds one envelope and, for a Begin, its request binding.
    record_bytes: MAX_CONTRACT_ENVELOPE_BYTES + 4096,
};
const CACHED_SEGMENTS: usize = 4;
const CACHED_FOLDS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ExecutionStoreError {
    #[error("execution storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("execution JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("execution ownership: {0}")]
    Ownership(#[from] OwnershipError),
    #[error("execution contract: {0}")]
    Contract(#[from] TurnContractError),
    #[error("legacy history: {0}")]
    LegacyHistory(String),
    #[error("invalid execution journal: {0}")]
    Invalid(&'static str),
    #[error("Session already has an unfinished logical turn")]
    UnfinishedTurn,
    #[error("execution admission would consume reserved settlement capacity")]
    Capacity,
    #[error("execution write failed; reopen before acknowledging or doing more work")]
    RecoveryRequired,
    #[error("execution segments: {0}")]
    Segment(String),
}

impl From<SegmentError> for ExecutionStoreError {
    fn from(error: SegmentError) -> Self {
        match error {
            SegmentError::Io(error) => Self::Io(error),
            SegmentError::Json(error) => Self::Json(error),
            SegmentError::RecoveryRequired => Self::RecoveryRequired,
            error => Self::Segment(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStoreOwner {
    pub workspace_id: String,
    pub session_id: SessionId,
}

/// Identity issued by the held canonical Session journal. This is persistence
/// provenance for related stores, never permission to execute external work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSessionIdentity {
    journal_id: String,
    owner: ExecutionStoreOwner,
}

impl DurableSessionIdentity {
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }

    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.owner
    }
}

/// An immutable legacy history frontier committed by the canonical v2 journal.
/// Keeping legacy turns closed does not establish safe replay of their effects.
#[derive(Debug, Clone, PartialEq)]
pub struct DurableLegacySeal {
    identity: DurableSessionIdentity,
    reference: EvidenceRef,
    last_predecessor: Option<LegacyTurnPredecessor>,
}

impl DurableLegacySeal {
    pub fn identity(&self) -> &DurableSessionIdentity {
        &self.identity
    }

    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }

    pub fn last_predecessor(&self) -> Option<&LegacyTurnPredecessor> {
        self.last_predecessor.as_ref()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySealRecord {
    reference: EvidenceRef,
    last_predecessor: Option<LegacyTurnPredecessor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    turn_id: LogicalTurnId,
    reference: EvidenceRef,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    ownership_id: String,
    journal_id: String,
    owner: ExecutionStoreOwner,
    records: Vec<TurnContractEnvelope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requests: Vec<RequestBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy: Option<LegacySealRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_origin: Option<crate::native_history::NativeHistoryOrigin>,
    /// Present once the records live in the segment log: `records` and
    /// `requests` are then empty, and an older daemon refuses the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    segments: Option<SegmentsMarker>,
}

/// Durable history evidence, not a permission to dispatch an activation/tool.
/// No deserializer or caller-controlled constructor can manufacture this receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableTurnReceipt {
    journal_id: String,
    sequence: u64,
    envelope: TurnContractEnvelope,
}

impl DurableTurnReceipt {
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn envelope(&self) -> &TurnContractEnvelope {
        &self.envelope
    }
    pub fn turn_revision(&self) -> u64 {
        self.envelope.expected_revision + 1
    }
}

/// A projection copied from successfully persisted canonical Session history.
/// Live snapshots can become stale; controllers must still check current
/// revisions before dispatch. A closed snapshot's accepted set is immutable.
#[derive(Debug, Clone)]
pub struct DurableTurnSnapshot {
    journal_id: String,
    owner: ExecutionStoreOwner,
    contract: TurnContract,
    turn_id: LogicalTurnId,
    request_ref: Option<EvidenceRef>,
    legacy_predecessor: Option<LegacyTurnPredecessor>,
}

impl DurableTurnSnapshot {
    pub fn turn_id(&self) -> &LogicalTurnId {
        &self.turn_id
    }
    pub fn request_ref(&self) -> Option<&EvidenceRef> {
        self.request_ref.as_ref()
    }
    pub fn legacy_predecessor(&self) -> Option<&LegacyTurnPredecessor> {
        self.legacy_predecessor.as_ref()
    }
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.owner
    }
    pub fn contract(&self) -> &TurnContract {
        &self.contract
    }
}

/// Room a turn has left within its per-turn bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnRoom {
    pub commands: usize,
    pub bytes: usize,
}

impl TurnRoom {
    /// Tool calls the turn can still record: each takes its intent command
    /// and holds back a bounded outcome until it settles.
    pub fn tool_calls(&self) -> usize {
        (self.commands / 2).min(self.bytes / (2 * (SMALL_SETTLEMENT_BYTES + 1)))
    }
}

/// One canonical record in the segment log. A Begin carries the binding of
/// its retained request, so both are published by one append.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CanonicalRecord {
    Event {
        envelope: TurnContractEnvelope,
    },
    Begin {
        envelope: TurnContractEnvelope,
        request: RequestBinding,
    },
}

impl CanonicalRecord {
    fn envelope(&self) -> &TurnContractEnvelope {
        match self {
            Self::Event { envelope } | Self::Begin { envelope, .. } => envelope,
        }
    }
    fn request(&self) -> Option<&RequestBinding> {
        match self {
            Self::Event { .. } => None,
            Self::Begin { request, .. } => Some(request),
        }
    }
}

/// Where a turn's records are and what later turns check against it. Kept
/// for every turn: a few hundred bytes each.
#[derive(Debug, Clone)]
struct TurnIndex {
    first: u64,
    last: u64,
    request: Option<EvidenceRef>,
    closed: Option<ClosedTurnRef>,
}

#[derive(Clone, Copy)]
struct Limits {
    turn_bytes: usize,
    turn_commands: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turn_bytes: MAX_RETAINED_CONTRACT_BYTES,
            turn_commands: MAX_CONTRACT_COMMANDS,
        }
    }
}

/// What appending one record changes, computed before anything is written.
struct Change {
    turn_id: LogicalTurnId,
    fold: TurnContract,
    new_turn: bool,
    request: Option<EvidenceRef>,
}

/// The held root guard excludes legacy format owners for this store's lifetime;
/// the separate Session directory inode lock excludes two writers sharing it.
/// No mutable projection is exposed. All changes pass replay validation and fsync.
///
/// Records live in a segment log beside the head file `execution.v2.json`, so
/// a Session holds any number of turns and records. Memory holds the open
/// turn's fold, the active segment, a small index entry per turn, a key filter
/// per sealed segment, and a few recently read segments and closed folds.
pub struct SessionExecutionStore {
    ownership: Arc<UpgradedFormatOwnership>,
    dir: SecureDir,
    head: Journal,
    log: SegmentLog,
    turns: HashMap<LogicalTurnId, TurnIndex>,
    /// Turn ids in Begin order.
    order: Vec<LogicalTurnId>,
    /// The fold of the latest turn while it is not closed.
    live: Option<(LogicalTurnId, TurnContract)>,
    /// Records of the active segment, in order.
    active: Vec<CanonicalRecord>,
    /// Command ids of the active segment and the sequence of each.
    active_commands: HashMap<CommandId, u64>,
    /// One command-id filter per sealed segment.
    filters: Vec<KeyFilter>,
    /// Each definition snapshot a graph has admitted, and the sequence of
    /// the first record admitting it. One entry per distinct definition.
    definitions: HashMap<EvidenceRef, u64>,
    cache: SegmentCache<CanonicalRecord>,
    folds: Mutex<VecDeque<(LogicalTurnId, Arc<TurnContract>)>>,
    poisoned: bool,
    limits: Limits,
    #[cfg(test)]
    lose_next_ack: bool,
}

impl SessionExecutionStore {
    pub fn open(
        ownership: Arc<UpgradedFormatOwnership>,
        owner: ExecutionStoreOwner,
    ) -> Result<Self, ExecutionStoreError> {
        Self::open_inner(ownership, owner, false)
    }

    /// Restart and explicit reopen must not recreate a missing canonical journal.
    pub fn open_existing(
        ownership: Arc<UpgradedFormatOwnership>,
        owner: ExecutionStoreOwner,
    ) -> Result<Self, ExecutionStoreError> {
        Self::open_inner(ownership, owner, true)
    }

    fn open_inner(
        ownership: Arc<UpgradedFormatOwnership>,
        owner: ExecutionStoreOwner,
        existing_only: bool,
    ) -> Result<Self, ExecutionStoreError> {
        if owner.workspace_id.is_empty()
            || owner.workspace_id.len() > 128
            || owner.workspace_id.chars().any(char::is_control)
        {
            return Err(ExecutionStoreError::Invalid("workspace identity"));
        }
        let dir = if existing_only {
            ownership.existing_session_directory(owner.session_id.as_str())?
        } else {
            ownership.session_directory(owner.session_id.as_str())?
        };
        dir.lock_exclusive_waiting(axocoatl_core::LOCK_INHERITANCE_GRACE)?;
        let mut head = match dir.read_limited(FILE, LEGACY_MAX_SESSION_BYTES) {
            Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if existing_only {
                    return Err(error.into());
                }
                if !dir.entries_limited(1)?.is_empty() {
                    return Err(ExecutionStoreError::Invalid(
                        "missing journal in a nonempty Session namespace",
                    ));
                }
                let head = Journal {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    ownership_id: ownership.manifest().ownership_id.clone(),
                    journal_id: uuid::Uuid::new_v4().to_string(),
                    owner: owner.clone(),
                    records: Vec::new(),
                    requests: Vec::new(),
                    legacy: None,
                    native_origin: None,
                    segments: Some(SegmentsMarker::of(&SPEC)),
                };
                // The log first: an interrupted first open leaves a nonempty
                // namespace without a journal, which fails closed.
                SegmentLog::open(
                    dir.clone(),
                    SPEC,
                    head_meta(&head),
                    true,
                    |_, _: CanonicalRecord| Ok::<(), ExecutionStoreError>(()),
                )?;
                dir.atomic_write(FILE, &serde_json::to_vec(&head)?)?;
                head
            }
            Err(error) => return Err(error.into()),
        };
        if head.schema_version != TURN_CONTRACT_SCHEMA_VERSION
            || head.owner != owner
            || head.ownership_id != ownership.manifest().ownership_id
            || uuid::Uuid::parse_str(&head.journal_id)
                .ok()
                .is_none_or(|id| id.is_nil() || id.to_string() != head.journal_id)
            || head.records.len() > LEGACY_MAX_SESSION_RECORDS
        {
            return Err(ExecutionStoreError::Invalid(
                "unsupported schema, owner, or bounds",
            ));
        }
        if let Some(origin) = &head.native_origin {
            origin.validate(&ownership, &owner)?;
            if head.legacy.is_some() {
                return Err(ExecutionStoreError::Invalid(
                    "native origin and legacy seal are mutually exclusive",
                ));
            }
        }
        match &head.segments {
            None => head = migrate_single_file(&dir, head)?,
            Some(marker) if marker.matches(&SPEC) => {
                if !head.records.is_empty() || !head.requests.is_empty() {
                    return Err(ExecutionStoreError::Invalid(
                        "a segmented journal head holds no records",
                    ));
                }
            }
            Some(_) => return Err(ExecutionStoreError::Invalid("unsupported journal segments")),
        }
        let mut loader = Loader::new(owner.clone(), Limits::default());
        let log = SegmentLog::open_indexed(
            dir.clone(),
            SPEC,
            head_meta(&head),
            false,
            |segment, sequence, record: CanonicalRecord| loader.visit(segment, sequence, record),
        )?;
        let loaded = loader.finish(&log);
        let mut store = Self {
            ownership,
            dir,
            head,
            log,
            turns: loaded.turns,
            order: loaded.order,
            live: loaded.live,
            active: loaded.active,
            active_commands: loaded.active_commands,
            filters: loaded.filters,
            definitions: loaded.definitions,
            cache: SegmentCache::new(CACHED_SEGMENTS),
            folds: Mutex::new(VecDeque::new()),
            poisoned: false,
            limits: Limits::default(),
            #[cfg(test)]
            lose_next_ack: false,
        };
        for (command, segment) in loaded.suspects {
            if store.segment_holds_command(segment, &command)? {
                return Err(ExecutionStoreError::Invalid("duplicate canonical command"));
            }
        }
        store.interrupt_recovered_epoch()?;
        Ok(store)
    }

    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.head.owner
    }

    pub fn identity(&self) -> Result<DurableSessionIdentity, ExecutionStoreError> {
        self.verify()?;
        Ok(DurableSessionIdentity {
            journal_id: self.head.journal_id.clone(),
            owner: self.head.owner.clone(),
        })
    }

    /// Verify the host's retained data-root capability against this store's
    /// held format owner before attaching external execution resources.
    pub fn verify_data_root(&self, root: &SecureDir) -> Result<(), ExecutionStoreError> {
        self.verify()?;
        self.ownership.verify_root(root)?;
        Ok(())
    }

    /// Capture the existing v1 source beneath this exact held data root.
    pub fn legacy_history_snapshot(
        &self,
    ) -> Result<crate::execution_legacy::OwnedLegacyHistorySnapshot, ExecutionStoreError> {
        let identity = self.identity()?;
        if self.head.legacy.is_some()
            || self.head.native_origin.is_some()
            || self.record_count() != 0
        {
            return Err(ExecutionStoreError::Invalid(
                "legacy capture must precede the canonical seal and v2 work",
            ));
        }
        crate::execution_legacy::OwnedLegacyHistorySnapshot::capture(
            self.ownership.clone(),
            identity,
        )
    }

    pub fn component_namespace(
        &self,
        component: ExecutionComponent,
    ) -> Result<OwnedExecutionNamespace, ExecutionStoreError> {
        Ok(OwnedExecutionNamespace::provision(
            self.dir.clone(),
            self.ownership.clone(),
            self.identity()?,
            component,
        )
        .map_err(std::io::Error::from)?)
    }
    /// Recovery opens only existing initialized component journals. This check
    /// precedes their ordinary writer opener, preserving its ownership checks.
    pub fn existing_component_namespace(
        &self,
        component: ExecutionComponent,
        primary: &std::path::Path,
    ) -> Result<OwnedExecutionNamespace, ExecutionStoreError> {
        Ok(OwnedExecutionNamespace::existing(
            self.dir.clone(),
            self.ownership.clone(),
            self.identity()?,
            component,
            primary,
        )
        .map_err(std::io::Error::from)?)
    }

    /// Inspect an existing component without provisioning or reopening any
    /// execution store. The resulting bytes are read evidence, never receipts.
    pub(crate) fn read_existing_component(
        &self,
        component: &ExecutionComponent,
        primary: &std::path::Path,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ExecutionStoreError> {
        self.verify()?;
        let bytes = crate::execution_namespace::read_existing_component(
            &self.dir,
            &self.ownership,
            &self.identity()?,
            component,
            primary,
            max_bytes,
        )?;
        self.verify()?;
        Ok(bytes)
    }

    /// The root of an existing component for a reader of its segmented
    /// journal, without a writer lock. The caller never writes through it.
    pub(crate) fn existing_component_root(
        &self,
        component: &ExecutionComponent,
        primary: &std::path::Path,
    ) -> Result<SecureDir, ExecutionStoreError> {
        self.verify()?;
        let root = crate::execution_namespace::existing_component_root(
            &self.dir,
            &self.ownership,
            &self.identity()?,
            component,
            primary,
        )?;
        self.verify()?;
        Ok(root)
    }

    /// Read one file in a subdirectory of an existing component without a
    /// writer, such as a screenshot kept beside the network record.
    pub(crate) fn read_existing_component_file(
        &self,
        component: &ExecutionComponent,
        primary: &std::path::Path,
        child: &std::path::Path,
        name: &std::path::Path,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ExecutionStoreError> {
        self.verify()?;
        let bytes = crate::execution_namespace::read_existing_component_file(
            &self.dir,
            &self.ownership,
            &self.identity()?,
            component,
            primary,
            child,
            name,
            max_bytes,
        )?;
        self.verify()?;
        Ok(bytes)
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(FILE)
    }

    /// Every file of the journal and its contents: the head, the active
    /// segment and each sealed segment.
    #[cfg(test)]
    pub(crate) fn stored_files_for_test(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = vec![self.path(), self.dir.path().join(SPEC.active_name())];
        if let Ok(entries) = std::fs::read_dir(self.dir.path().join("segments")) {
            let mut sealed: Vec<_> = entries.map(|entry| entry.unwrap().path()).collect();
            sealed.sort();
            files.extend(sealed);
        }
        files
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }

    /// What `id` may still record within its per-turn bounds, after the
    /// settlement it holds back; `None` for a turn this Session lacks.
    pub fn turn_room(&self, id: &LogicalTurnId) -> Result<Option<TurnRoom>, ExecutionStoreError> {
        self.verify()?;
        if !self.turns.contains_key(id) {
            return Ok(None);
        }
        let fold = self.fold(id)?;
        let limits = self.limits;
        let (bytes, records) = settlement_reservation(&fold);
        Ok(Some(TurnRoom {
            commands: limits
                .turn_commands
                .saturating_sub(fold.command_count())
                .saturating_sub(records),
            bytes: limits
                .turn_bytes
                .saturating_sub(fold.retained_event_bytes())
                .saturating_sub(bytes),
        }))
    }

    /// Whether this Session has a turn `id`.
    pub fn contains_turn(&self, id: &LogicalTurnId) -> Result<bool, ExecutionStoreError> {
        self.verify()?;
        Ok(self.turns.contains_key(id))
    }

    /// The fold of turn `id`, replayed from its records when it is closed
    /// and not recently read.
    pub fn turn(&self, id: &LogicalTurnId) -> Result<Option<TurnContract>, ExecutionStoreError> {
        self.verify()?;
        if !self.turns.contains_key(id) {
            return Ok(None);
        }
        Ok(Some(self.fold(id)?.as_ref().clone()))
    }

    pub fn snapshot(&self, id: &LogicalTurnId) -> Result<DurableTurnSnapshot, ExecutionStoreError> {
        self.verify()?;
        let index = self
            .turns
            .get(id)
            .ok_or(ExecutionStoreError::Invalid("logical turn is absent"))?;
        Ok(DurableTurnSnapshot {
            journal_id: self.head.journal_id.clone(),
            owner: self.head.owner.clone(),
            contract: self.fold(id)?.as_ref().clone(),
            turn_id: id.clone(),
            request_ref: index.request.clone(),
            legacy_predecessor: (self.order.first() == Some(id))
                .then(|| {
                    self.head
                        .legacy
                        .as_ref()
                        .and_then(|seal| seal.last_predecessor.clone())
                })
                .flatten(),
        })
    }

    pub fn unfinished_turn(
        &self,
    ) -> Result<Option<(&LogicalTurnId, &TurnContract)>, ExecutionStoreError> {
        self.verify()?;
        Ok(self
            .live
            .as_ref()
            .filter(|(_, turn)| turn.state().is_some_and(|state| !state.is_closed()))
            .map(|(id, turn)| (id, turn)))
    }

    /// How many records this Session has: the sequence of the latest.
    pub fn record_count(&self) -> u64 {
        self.log.next_sequence() - 1
    }

    /// Every turn id, in the order the turns began.
    pub fn turn_ids(&self) -> Result<&[LogicalTurnId], ExecutionStoreError> {
        self.verify()?;
        Ok(&self.order)
    }

    /// The turn that began last.
    pub fn latest_turn(&self) -> Result<Option<&LogicalTurnId>, ExecutionStoreError> {
        self.verify()?;
        Ok(self.order.last())
    }

    /// The sequences of turn `id`'s first and last records.
    pub fn turn_sequences(
        &self,
        id: &LogicalTurnId,
    ) -> Result<Option<(u64, u64)>, ExecutionStoreError> {
        self.verify()?;
        Ok(self.turns.get(id).map(|index| (index.first, index.last)))
    }

    /// Turn `id`'s records, in order.
    pub fn turn_records(
        &self,
        id: &LogicalTurnId,
    ) -> Result<Vec<TurnContractEnvelope>, ExecutionStoreError> {
        self.verify()?;
        let Some(index) = self.turns.get(id) else {
            return Ok(vec![]);
        };
        Ok(self
            .records_between(index.first, index.last)?
            .into_iter()
            .map(|(_, record)| record.envelope().clone())
            .collect())
    }

    /// The records with sequences `first..=last`, in order.
    pub fn records_in(
        &self,
        first: u64,
        last: u64,
    ) -> Result<Vec<(u64, TurnContractEnvelope)>, ExecutionStoreError> {
        self.verify()?;
        Ok(self
            .records_between(first, last)?
            .into_iter()
            .map(|(sequence, record)| (sequence, record.envelope().clone()))
            .collect())
    }

    /// The record of `command`, anywhere in the history, and its sequence.
    pub fn command_record(
        &self,
        command: &CommandId,
    ) -> Result<Option<(u64, TurnContractEnvelope)>, ExecutionStoreError> {
        self.verify()?;
        Ok(self
            .find_command(command)?
            .map(|(sequence, record)| (sequence, record.envelope().clone())))
    }

    /// The 0-based place in the history of the first record whose graph
    /// admits `definition`.
    pub fn first_definition_use(
        &self,
        definition: &EvidenceRef,
    ) -> Result<Option<usize>, ExecutionStoreError> {
        self.verify()?;
        Ok(self
            .definitions
            .get(definition)
            .map(|sequence| (*sequence - 1) as usize))
    }

    /// The whole history, read back from every segment. Memory grows with
    /// it, so this is for export and tests, not for live operation.
    pub fn records(&self) -> Result<Vec<TurnContractEnvelope>, ExecutionStoreError> {
        self.verify()?;
        Ok(self
            .records_between(1, self.record_count())?
            .into_iter()
            .map(|(_, record)| record.envelope().clone())
            .collect())
    }

    /// Commit a retained request in the same atomic write as its Begin. A
    /// published Begin can never acquire different request bytes on a retry.
    pub fn begin_with_request(
        &mut self,
        envelope: TurnContractEnvelope,
        request: &DurableExecutionRequest,
    ) -> Result<DurableTurnReceipt, ExecutionStoreError> {
        self.verify()?;
        if request.journal_id() != self.head.journal_id
            || request.owner() != self.owner()
            || request.turn_id() != &envelope.turn_id
            || !matches!(envelope.event, TurnContractEvent::Begin { .. })
        {
            return Err(ExecutionStoreError::Invalid(
                "request ownership or Begin mismatch",
            ));
        }
        let binding = RequestBinding {
            turn_id: envelope.turn_id.clone(),
            reference: request.reference().clone(),
        };
        if let Some((sequence, existing)) = self.find_command(&envelope.command_id)? {
            if existing.envelope() != &envelope
                || self
                    .turns
                    .get(&binding.turn_id)
                    .and_then(|index| index.request.as_ref())
                    != Some(&binding.reference)
            {
                return Err(TurnContractError::CommandConflict.into());
            }
            return Ok(self.receipt(sequence, existing.envelope().clone()));
        }
        self.commit(CanonicalRecord::Begin {
            envelope,
            request: binding,
        })
    }

    /// Seal supported legacy history before the first v2 turn. The source
    /// receipt comes from retained immutable content, not a caller-made digest.
    pub fn seal_legacy_history(
        &mut self,
        history: &DurableLegacyHistory,
    ) -> Result<DurableLegacySeal, ExecutionStoreError> {
        self.verify()?;
        if history.journal_id() != self.head.journal_id || history.owner() != self.owner() {
            return Err(ExecutionStoreError::Invalid("foreign legacy history"));
        }
        if self.head.native_origin.is_some() {
            return Err(ExecutionStoreError::Invalid(
                "native Session cannot acquire a legacy seal",
            ));
        }
        let seal = LegacySealRecord {
            reference: history.reference().clone(),
            last_predecessor: history.last_predecessor().cloned(),
        };
        if let Some(existing) = &self.head.legacy {
            if existing != &seal {
                return Err(ExecutionStoreError::Invalid("legacy frontier is immutable"));
            }
        } else {
            if self.record_count() != 0 {
                return Err(ExecutionStoreError::Invalid(
                    "legacy frontier must precede v2 work",
                ));
            }
            crate::execution_legacy::verify_source(&self.ownership, history.source())?;
            let mut head = self.head.clone();
            head.legacy = Some(seal);
            self.write_head(head)?;
        }
        self.legacy_seal()?
            .ok_or(ExecutionStoreError::Invalid("missing legacy seal"))
    }

    /// Record only an actual creation receipt before any native work. This
    /// does not infer origin from an empty journal or absent legacy source.
    pub fn record_native_origin(
        &mut self,
        receipt: &crate::native_history::NativeSessionCreationReceipt,
    ) -> Result<(), ExecutionStoreError> {
        self.verify()?;
        receipt.origin.validate(&self.ownership, self.owner())?;
        if self.head.legacy.is_some() {
            return Err(ExecutionStoreError::Invalid(
                "sealed legacy Session cannot become native",
            ));
        }
        if let Some(origin) = &self.head.native_origin {
            return if origin == &receipt.origin {
                Ok(())
            } else {
                Err(ExecutionStoreError::Invalid(
                    "native Session origin is immutable",
                ))
            };
        }
        receipt.verify(&self.ownership, self.owner())?;
        if self.record_count() != 0 {
            return Err(ExecutionStoreError::Invalid(
                "native origin must precede the first Begin",
            ));
        }
        let mut head = self.head.clone();
        head.native_origin = Some(receipt.origin.clone());
        self.write_head(head)
    }

    pub fn native_origin(
        &self,
    ) -> Result<Option<&crate::native_history::NativeHistoryOrigin>, ExecutionStoreError> {
        self.verify()?;
        Ok(self.head.native_origin.as_ref())
    }

    pub fn legacy_seal(&self) -> Result<Option<DurableLegacySeal>, ExecutionStoreError> {
        let identity = self.identity()?;
        Ok(self.head.legacy.as_ref().map(|seal| DurableLegacySeal {
            identity,
            reference: seal.reference.clone(),
            last_predecessor: seal.last_predecessor.clone(),
        }))
    }

    /// Exact repeats return the original durable receipt before checking stale
    /// revisions or current closure. A reused command ID with changed content
    /// fails even when it names a different logical turn in this Session.
    pub fn append(
        &mut self,
        envelope: TurnContractEnvelope,
    ) -> Result<DurableTurnReceipt, ExecutionStoreError> {
        self.verify()?;
        if let Some((sequence, existing)) = self.find_command(&envelope.command_id)? {
            if existing.envelope() != &envelope {
                return Err(TurnContractError::CommandConflict.into());
            }
            return Ok(self.receipt(sequence, existing.envelope().clone()));
        }
        self.commit(CanonicalRecord::Event { envelope })
    }

    fn receipt(&self, sequence: u64, envelope: TurnContractEnvelope) -> DurableTurnReceipt {
        DurableTurnReceipt {
            journal_id: self.head.journal_id.clone(),
            sequence,
            envelope,
        }
    }

    fn interrupt_recovered_epoch(&mut self) -> Result<(), ExecutionStoreError> {
        let recovered = self.live.as_ref().and_then(|(id, turn)| {
            (turn.state() == Some(LogicalTurnState::Running)).then(|| {
                (
                    id.clone(),
                    turn.revision(),
                    turn.epochs()
                        .last()
                        .expect("validated live epoch")
                        .id
                        .clone(),
                )
            })
        });
        if let Some((turn_id, expected_revision, epoch_id)) = recovered {
            self.append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!("recovery:{}", uuid::Uuid::new_v4()))?,
                expected_revision,
                session_id: self.head.owner.session_id.clone(),
                turn_id,
                event: TurnContractEvent::InterruptEpoch { epoch_id },
            })?;
        }
        Ok(())
    }

    fn verify(&self) -> Result<(), ExecutionStoreError> {
        if self.poisoned {
            return Err(ExecutionStoreError::RecoveryRequired);
        }
        self.ownership.verify_installed()?;
        self.dir.verify_ambient_identity()?;
        Ok(())
    }

    fn write_head(&mut self, head: Journal) -> Result<(), ExecutionStoreError> {
        self.verify()?;
        if let Err(error) = self.dir.atomic_write(FILE, &serde_json::to_vec(&head)?) {
            self.poisoned = true;
            return Err(error.into());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.lose_next_ack) {
            self.poisoned = true;
            return Err(std::io::Error::other(
                "injected acknowledgement loss after durable publication",
            )
            .into());
        }
        self.head = head;
        Ok(())
    }

    /// Validate `record`, append it as one synced line, then apply it.
    fn commit(
        &mut self,
        record: CanonicalRecord,
    ) -> Result<DurableTurnReceipt, ExecutionStoreError> {
        let sequence = self.log.next_sequence();
        let change = self.change(&record)?;
        let line = self
            .log
            .encode_record(&record)
            .map_err(|error| match error {
                SegmentError::RecordTooLarge => {
                    ExecutionStoreError::Invalid("canonical record exceeds its bound")
                }
                error => error.into(),
            })?;
        if let Err(error) = self.log.append_line(&line) {
            self.poisoned = true;
            return Err(error.into());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.lose_next_ack) {
            self.poisoned = true;
            return Err(std::io::Error::other(
                "injected acknowledgement loss after durable publication",
            )
            .into());
        }
        let envelope = record.envelope().clone();
        self.apply_change(change, sequence)?;
        note_definitions(&mut self.definitions, &envelope, sequence);
        self.active_commands
            .insert(envelope.command_id.clone(), sequence);
        self.active.push(record);
        if self.log.should_seal() {
            let filter = command_filter(self.active_commands.keys());
            if self.log.seal().is_err() {
                // The record is durable; reopening completes the seal.
                self.poisoned = true;
            } else {
                self.filters.push(filter);
                self.active.clear();
                self.active_commands.clear();
            }
        }
        Ok(self.receipt(sequence, envelope))
    }

    /// What appending `record` would change, or why it is refused.
    fn change(&self, record: &CanonicalRecord) -> Result<Change, ExecutionStoreError> {
        let envelope = record.envelope();
        check_record(&self.head.owner, record)?;
        let live = self
            .live
            .as_ref()
            .filter(|(id, _)| id == &envelope.turn_id)
            .map(|(_, fold)| fold);
        if let TurnContractEvent::Begin { predecessor, .. } = &envelope.event {
            if self.unfinished_turn()?.is_some() {
                return Err(ExecutionStoreError::UnfinishedTurn);
            }
            if let Some(previous) = predecessor {
                let index =
                    self.turns
                        .get(previous.turn_id())
                        .ok_or(ExecutionStoreError::Invalid(
                            "predecessor absent from canonical Session history",
                        ))?;
                let canonical =
                    index
                        .closed
                        .clone()
                        .ok_or(TurnContractError::InvalidTransition(
                            "predecessor is not closed",
                        ))?;
                if &canonical != previous {
                    return Err(ExecutionStoreError::Invalid(
                        "predecessor differs from canonical closed history",
                    ));
                }
            }
        }
        let new_turn = !self.turns.contains_key(&envelope.turn_id);
        let mut fold = match live {
            Some(fold) => fold.clone(),
            None if new_turn => TurnContract::default(),
            None => self.fold(&envelope.turn_id)?.as_ref().clone(),
        };
        if !fold.apply(envelope)? {
            return Err(ExecutionStoreError::Invalid("duplicate canonical event"));
        }
        check_turn_capacity(&fold, self.limits)?;
        Ok(Change {
            turn_id: envelope.turn_id.clone(),
            fold,
            new_turn,
            request: record.request().map(|binding| binding.reference.clone()),
        })
    }

    fn apply_change(&mut self, change: Change, sequence: u64) -> Result<(), ExecutionStoreError> {
        let Change {
            turn_id,
            fold,
            new_turn,
            request,
        } = change;
        if new_turn {
            self.order.push(turn_id.clone());
            self.turns.insert(
                turn_id.clone(),
                TurnIndex {
                    first: sequence,
                    last: sequence,
                    request,
                    closed: None,
                },
            );
        }
        let index = self
            .turns
            .get_mut(&turn_id)
            .ok_or(ExecutionStoreError::Invalid("logical turn is absent"))?;
        index.last = sequence;
        if fold.state().is_some_and(LogicalTurnState::is_closed) {
            index.closed = Some(fold.closed_reference()?);
            if self.live.as_ref().is_some_and(|(id, _)| id == &turn_id) {
                self.live = None;
            }
            self.remember_fold(&turn_id, Arc::new(fold));
        } else {
            self.live = Some((turn_id, fold));
        }
        Ok(())
    }

    /// The fold of turn `id`: the live one, a recently read one, or replayed.
    fn fold(&self, id: &LogicalTurnId) -> Result<Arc<TurnContract>, ExecutionStoreError> {
        if let Some((_, fold)) = self.live.as_ref().filter(|(live, _)| live == id) {
            return Ok(Arc::new(fold.clone()));
        }
        if let Some(fold) = self
            .folds
            .lock()
            .map_err(|_| ExecutionStoreError::Invalid("fold cache lock poisoned"))?
            .iter()
            .find(|(turn, _)| turn == id)
            .map(|(_, fold)| fold.clone())
        {
            return Ok(fold);
        }
        let index = self
            .turns
            .get(id)
            .ok_or(ExecutionStoreError::Invalid("logical turn is absent"))?;
        let mut fold = TurnContract::default();
        for (_, record) in self.records_between(index.first, index.last)? {
            let envelope = record.envelope();
            if &envelope.turn_id == id {
                fold.apply(envelope)?;
            }
        }
        let fold = Arc::new(fold);
        self.remember_fold(id, fold.clone());
        Ok(fold)
    }

    fn remember_fold(&self, id: &LogicalTurnId, fold: Arc<TurnContract>) {
        if let Ok(mut folds) = self.folds.lock() {
            folds.retain(|(turn, _)| turn != id);
            folds.push_front((id.clone(), fold));
            folds.truncate(CACHED_FOLDS);
        }
    }

    /// Records with sequences `first..=last`, from the segments holding them.
    fn records_between(
        &self,
        first: u64,
        last: u64,
    ) -> Result<Vec<(u64, CanonicalRecord)>, ExecutionStoreError> {
        let mut records = Vec::new();
        if first > last {
            return Ok(records);
        }
        for segment in self.log.sealed() {
            let end = segment.first_sequence + segment.records - 1;
            if end < first || segment.first_sequence > last {
                continue;
            }
            for (offset, record) in self.cache.get(&self.log, segment)?.iter().enumerate() {
                let sequence = segment.first_sequence + offset as u64;
                if (first..=last).contains(&sequence) {
                    records.push((sequence, record.as_ref().clone()));
                }
            }
        }
        let active_first = self.log.active_first_sequence();
        for (offset, record) in self.active.iter().enumerate() {
            let sequence = active_first + offset as u64;
            if (first..=last).contains(&sequence) {
                records.push((sequence, record.clone()));
            }
        }
        Ok(records)
    }

    fn find_command(
        &self,
        command: &CommandId,
    ) -> Result<Option<(u64, CanonicalRecord)>, ExecutionStoreError> {
        if let Some(sequence) = self.active_commands.get(command) {
            let at = (sequence - self.log.active_first_sequence()) as usize;
            return Ok(Some((*sequence, self.active[at].clone())));
        }
        let key = command_key(command);
        for (segment, filter) in self.log.sealed().iter().zip(&self.filters).rev() {
            if !filter.may_contain(&key) {
                continue;
            }
            for (offset, record) in self.cache.get(&self.log, segment)?.iter().enumerate() {
                if &record.envelope().command_id == command {
                    return Ok(Some((
                        segment.first_sequence + offset as u64,
                        record.as_ref().clone(),
                    )));
                }
            }
        }
        Ok(None)
    }

    fn segment_holds_command(
        &self,
        index: u64,
        command: &str,
    ) -> Result<bool, ExecutionStoreError> {
        let segment = &self.log.sealed()[index as usize];
        Ok(self
            .cache
            .get(&self.log, segment)?
            .iter()
            .any(|record| command_key(&record.envelope().command_id) == command))
    }
}

fn head_meta(head: &Journal) -> serde_json::Value {
    serde_json::json!({
        "journal_id": head.journal_id,
        "ownership_id": head.ownership_id,
        "owner": head.owner,
    })
}

fn command_key(command: &CommandId) -> String {
    format!("cmd:{}", command.as_str())
}

fn command_filter<'a>(commands: impl Iterator<Item = &'a CommandId>) -> KeyFilter {
    let keys: Vec<String> = commands.map(command_key).collect();
    KeyFilter::new(keys.iter().map(String::as_str))
}

/// Record checks that need no other record.
fn check_record(
    owner: &ExecutionStoreOwner,
    record: &CanonicalRecord,
) -> Result<(), ExecutionStoreError> {
    let envelope = record.envelope();
    if envelope.session_id != owner.session_id {
        return Err(ExecutionStoreError::Invalid("foreign Session event"));
    }
    if let Some(binding) = record.request() {
        if binding.turn_id != envelope.turn_id
            || !matches!(envelope.event, TurnContractEvent::Begin { .. })
        {
            return Err(ExecutionStoreError::Invalid(
                "duplicate or absent request owner",
            ));
        }
    }
    let size = serde_json::to_vec(envelope)?.len();
    if small_settlement(&envelope.event) && size > SMALL_SETTLEMENT_BYTES {
        return Err(ExecutionStoreError::Invalid(
            "settlement must use bounded evidence references",
        ));
    }
    Ok(())
}

/// Builds the store's state while its log is opened, one record at a time:
/// only the latest turn is folded, and each sealed segment leaves a filter.
struct Loader {
    owner: ExecutionStoreOwner,
    limits: Limits,
    segment: u64,
    segment_commands: HashMap<CommandId, u64>,
    segment_records: Vec<CanonicalRecord>,
    filters: Vec<KeyFilter>,
    suspects: Vec<(String, u64)>,
    turns: HashMap<LogicalTurnId, TurnIndex>,
    order: Vec<LogicalTurnId>,
    live: Option<(LogicalTurnId, TurnContract)>,
    definitions: HashMap<EvidenceRef, u64>,
}

struct Loaded {
    turns: HashMap<LogicalTurnId, TurnIndex>,
    order: Vec<LogicalTurnId>,
    live: Option<(LogicalTurnId, TurnContract)>,
    active: Vec<CanonicalRecord>,
    active_commands: HashMap<CommandId, u64>,
    filters: Vec<KeyFilter>,
    suspects: Vec<(String, u64)>,
    definitions: HashMap<EvidenceRef, u64>,
}

impl Loader {
    fn new(owner: ExecutionStoreOwner, limits: Limits) -> Self {
        Self {
            owner,
            limits,
            segment: 0,
            segment_commands: HashMap::new(),
            segment_records: Vec::new(),
            filters: Vec::new(),
            suspects: Vec::new(),
            turns: HashMap::new(),
            order: Vec::new(),
            live: None,
            definitions: HashMap::new(),
        }
    }

    fn visit(
        &mut self,
        segment: u64,
        sequence: u64,
        record: CanonicalRecord,
    ) -> Result<(), ExecutionStoreError> {
        if segment != self.segment {
            self.close_segment();
            self.segment = segment;
        }
        check_record(&self.owner, &record)?;
        let envelope = record.envelope();
        let key = command_key(&envelope.command_id);
        if self.segment_commands.contains_key(&envelope.command_id) {
            return Err(ExecutionStoreError::Invalid("duplicate canonical command"));
        }
        for (earlier, filter) in self.filters.iter().enumerate() {
            if filter.may_contain(&key) {
                self.suspects.push((key.clone(), earlier as u64));
            }
        }
        let live = self.live.take().filter(|(id, _)| id == &envelope.turn_id);
        if let TurnContractEvent::Begin { predecessor, .. } = &envelope.event {
            if self
                .live
                .as_ref()
                .is_some_and(|(_, turn)| turn.state().is_some_and(|state| !state.is_closed()))
            {
                return Err(ExecutionStoreError::UnfinishedTurn);
            }
            if let Some(previous) = predecessor {
                let canonical = self
                    .turns
                    .get(previous.turn_id())
                    .ok_or(ExecutionStoreError::Invalid(
                        "predecessor absent from canonical Session history",
                    ))?
                    .closed
                    .clone()
                    .ok_or(TurnContractError::InvalidTransition(
                        "predecessor is not closed",
                    ))?;
                if &canonical != previous {
                    return Err(ExecutionStoreError::Invalid(
                        "predecessor differs from canonical closed history",
                    ));
                }
            }
        }
        let new_turn = !self.turns.contains_key(&envelope.turn_id);
        let mut fold = match live {
            Some((_, fold)) => fold,
            None if new_turn => TurnContract::default(),
            // Only the latest turn may change, and a closed one never does.
            None => {
                return Err(ExecutionStoreError::Invalid(
                    "a record changes a turn that already ended",
                ))
            }
        };
        if !fold.apply(envelope)? {
            return Err(ExecutionStoreError::Invalid("duplicate canonical event"));
        }
        check_turn_capacity(&fold, self.limits)?;
        if new_turn {
            self.order.push(envelope.turn_id.clone());
            self.turns.insert(
                envelope.turn_id.clone(),
                TurnIndex {
                    first: sequence,
                    last: sequence,
                    request: record.request().map(|binding| binding.reference.clone()),
                    closed: None,
                },
            );
        }
        let index = self.turns.get_mut(&envelope.turn_id).unwrap();
        index.last = sequence;
        if fold.state().is_some_and(LogicalTurnState::is_closed) {
            index.closed = Some(fold.closed_reference()?);
        }
        note_definitions(&mut self.definitions, envelope, sequence);
        self.live = Some((envelope.turn_id.clone(), fold));
        self.segment_commands
            .insert(envelope.command_id.clone(), sequence);
        self.segment_records.push(record);
        Ok(())
    }

    fn close_segment(&mut self) {
        self.filters
            .push(command_filter(self.segment_commands.keys()));
        self.segment_commands.clear();
        self.segment_records.clear();
    }

    fn finish(mut self, log: &SegmentLog) -> Loaded {
        let (active, active_commands) = if self.segment == log.active_index() {
            (
                std::mem::take(&mut self.segment_records),
                std::mem::take(&mut self.segment_commands),
            )
        } else {
            if !self.segment_commands.is_empty() {
                self.close_segment();
            }
            (vec![], HashMap::new())
        };
        debug_assert_eq!(self.filters.len(), log.sealed().len());
        let live = self
            .live
            .filter(|(_, turn)| !turn.state().is_some_and(LogicalTurnState::is_closed));
        Loaded {
            turns: self.turns,
            order: self.order,
            live,
            active,
            active_commands,
            filters: self.filters,
            suspects: self.suspects,
            definitions: self.definitions,
        }
    }
}

/// Remember the first record whose graph admits each definition snapshot.
fn note_definitions(
    definitions: &mut HashMap<EvidenceRef, u64>,
    envelope: &TurnContractEnvelope,
    sequence: u64,
) {
    if let TurnContractEvent::Begin { graph, .. } | TurnContractEvent::ReviseGraph { graph, .. } =
        &envelope.event
    {
        for node in &graph.nodes {
            definitions
                .entry(node.definition.snapshot.clone())
                .or_insert(sequence);
        }
    }
}

/// Validate a single-file journal with the rules it was written under, move
/// its records into a segment log, then replace the file with the head. A
/// crash before the head is written leaves the old file, and the next open
/// converts it again from the start.
fn migrate_single_file(dir: &SecureDir, journal: Journal) -> Result<Journal, ExecutionStoreError> {
    let mut requests: HashMap<LogicalTurnId, RequestBinding> = HashMap::new();
    for binding in &journal.requests {
        if requests
            .insert(binding.turn_id.clone(), binding.clone())
            .is_some()
        {
            return Err(ExecutionStoreError::Invalid(
                "duplicate or absent request owner",
            ));
        }
    }
    let mut records = Vec::with_capacity(journal.records.len());
    for envelope in &journal.records {
        let begin = matches!(envelope.event, TurnContractEvent::Begin { .. });
        records.push(match requests.remove(&envelope.turn_id).filter(|_| begin) {
            Some(request) => CanonicalRecord::Begin {
                envelope: envelope.clone(),
                request,
            },
            None => CanonicalRecord::Event {
                envelope: envelope.clone(),
            },
        });
    }
    if !requests.is_empty() {
        return Err(ExecutionStoreError::Invalid(
            "duplicate or absent request owner",
        ));
    }
    if journal
        .legacy
        .as_ref()
        .and_then(|seal| seal.last_predecessor.as_ref())
        .is_some_and(|predecessor| !predecessor.status.is_terminal())
    {
        return Err(ExecutionStoreError::Invalid(
            "legacy predecessor is unfinished",
        ));
    }
    let mut loader = Loader::new(journal.owner.clone(), Limits::default());
    for (index, record) in records.iter().enumerate() {
        loader.visit(0, index as u64 + 1, record.clone())?;
    }
    let head = Journal {
        records: Vec::new(),
        requests: Vec::new(),
        segments: Some(SegmentsMarker::of(&SPEC)),
        ..journal
    };
    SegmentLog::remove(dir, &SPEC)?;
    let mut log = SegmentLog::open(
        dir.clone(),
        SPEC,
        head_meta(&head),
        true,
        |_, _: CanonicalRecord| Ok::<(), ExecutionStoreError>(()),
    )?;
    for record in &records {
        log.append_line(&log.encode_record(record)?)?;
        if log.should_seal() {
            log.seal()?;
        }
    }
    drop(log);
    dir.atomic_write(FILE, &serde_json::to_vec(&head)?)?;
    Ok(head)
}

fn small_settlement(event: &TurnContractEvent) -> bool {
    matches!(
        event,
        TurnContractEvent::RequestTurnStop { .. }
            | TurnContractEvent::StartPreparedActivation { .. }
            | TurnContractEvent::AcceptActivation { .. }
            | TurnContractEvent::FailActivation { .. }
            | TurnContractEvent::RecordOutcome { .. }
            | TurnContractEvent::ProveNotDispatched { .. }
            | TurnContractEvent::ResolveConditionIntent { .. }
            | TurnContractEvent::ResolveBlocker { .. }
            | TurnContractEvent::AbandonBlocker { .. }
            | TurnContractEvent::InterruptEpoch { .. }
            | TurnContractEvent::PauseEpoch { .. }
            | TurnContractEvent::Close { .. }
    )
}

fn check_turn_capacity(
    turn: &TurnContract,
    limits: Limits,
) -> Result<(usize, usize), ExecutionStoreError> {
    let (bytes, records) = settlement_reservation(turn);
    if turn.retained_event_bytes().saturating_add(bytes) > limits.turn_bytes
        || turn.command_count().saturating_add(records) > limits.turn_commands
    {
        return Err(ExecutionStoreError::Capacity);
    }
    Ok((bytes, records))
}

fn settlement_reservation(turn: &TurnContract) -> (usize, usize) {
    let mut small = match turn.state() {
        Some(LogicalTurnState::Running) => 2, // interrupt, then explicit closure
        Some(LogicalTurnState::NeedsAttention) => 1,
        _ => return (0, 0), // late external evidence belongs to the separate audit
    };
    // Reserve the exact human Stop request in addition to interruption/closure.
    // Recording the intent consumes this reservation; no limit is increased.
    if turn.stop_requested().is_none() {
        small += 1;
    }
    for activation in turn.activations() {
        // Replacement retires never-started work, retaining its row as history.
        // There can be no later start/terminal event for that removed node.
        if turn
            .replaced_nodes()
            .iter()
            .any(|node| node.previous == activation.activation.node_id)
        {
            continue;
        }
        small += match activation.state {
            ActivationState::Unstarted => 2, // begin prepared, then accept/fail
            ActivationState::Running => 1,
            _ => 0,
        };
    }
    small += turn
        .invocations()
        .iter()
        .filter(|invocation| invocation.evidence.disposition() == EffectDisposition::OutcomeUnknown)
        .count();
    // A typed durable blocker reserves its exact response independently from
    // activation/effect settlement. An epoch interruption retires the wait.
    small += turn.pending_blocker_count();
    // Resolving an undispatched check leaves its readiness observation absent.
    // Reserve this terminal effect record separately from the observation below.
    small += turn
        .condition_runs()
        .iter()
        .filter(|run| run.resolution.is_none())
        .count();
    // A passed or failed exact-scope observation settles a check. Invalidation
    // makes historical observations stale, reserving space before revised work.
    let conditions = turn.graph().map_or(0, |graph| {
        graph
            .conditions
            .iter()
            .filter(|condition| turn.current_condition(&condition.condition_id).is_none())
            .count()
    });
    // Include the per-record JSON separator in the Session-wide reservation.
    (
        small * (SMALL_SETTLEMENT_BYTES + 1) + conditions * (MAX_CONTRACT_ENVELOPE_BYTES + 1),
        small + conditions,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution_ownership::LegacyFormatOwnership;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, SessionExecutionStore, serde_json::Value) {
        let root = tempfile::tempdir().unwrap();
        let guard = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let owner = ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session-a").unwrap(),
        };
        let mut store = SessionExecutionStore::open(guard, owner).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/interrupted_epoch_preserves_acceptance.json"
        ))
        .unwrap();
        for index in 0..2 {
            store
                .append(
                    serde_json::from_value(fixture["steps"][index]["envelope"].clone()).unwrap(),
                )
                .unwrap();
        }
        let activation = fixture["steps"][1]["envelope"]["event"]["input"]["activation"].clone();
        (root, store, activation)
    }

    fn envelope(revision: u64, event: serde_json::Value) -> TurnContractEnvelope {
        serde_json::from_value(json!({
            "schema_version": 2, "command_id": format!("capacity-{revision}"),
            "expected_revision": revision, "session_id": "session-a", "turn_id": "turn-a", "event": event,
        })).unwrap()
    }

    #[test]
    fn replacement_releases_only_impossible_prepared_activation_settlement_slots() {
        let fixture: Vec<TurnContractEnvelope> = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/dynamic_graph_blocker_contract.json"
        ))
        .unwrap();
        let mut turn = TurnContract::default();
        turn.apply(&fixture[0]).unwrap();
        turn.apply(&envelope(
            1,
            json!({"kind":"pause_epoch","epoch_id":"epoch-1"}),
        ))
        .unwrap();
        let mut input = serde_json::to_value(&fixture[2]).unwrap()["event"]["input"].clone();
        input["activation"]["execution_epoch_id"] = "epoch-2".into();
        let previous_activation = input["activation"].clone();
        turn.apply(&envelope(
            2,
            json!({"kind":"continue","plan":{
            "source_epoch_id":"epoch-1","epoch_id":"epoch-2","condition_runs":[],
            "selections":[{"kind":"prepare_unmaterialized","input":input},
                {"kind":"await_dependencies","node_id":"b"}]}}),
        ))
        .unwrap();
        let before = settlement_reservation(&turn);
        let mut graph = serde_json::to_value(turn.graph().unwrap()).unwrap();
        graph["snapshot_id"] = "graph-replaced".into();
        graph["revision"] = 2.into();
        graph["nodes"][0]["node_id"] = "x".into();
        graph["nodes"][0]["slot_id"] = "slot-x".into();
        graph["nodes"][0]["conversation_id"] = "conversation-x".into();
        graph["dependencies"][0]["parent"] = "x".into();
        turn.apply(&envelope(3,json!({"kind":"revise_graph","epoch_id":"epoch-2", "previous_graph":"graph-1",
            "graph":graph,"mutation":{"kind":"replace_future","previous":"a","replacement":"x","rewire_dependents":["b"]},
            "admission_evidence":"replace-prepared-a"}))).unwrap();
        let after = settlement_reservation(&turn);
        assert_eq!(before.1 - after.1, 2);
        assert_eq!(before.0 - after.0, 2 * (SMALL_SETTLEMENT_BYTES + 1));
        assert_eq!(
            turn.activations()[0].state,
            ActivationState::Unstarted,
            "historical input remains retained"
        );
        let frozen = turn.clone();
        assert!(turn
            .apply(&envelope(
                4,
                json!({"kind":"start_prepared_activation","activation":previous_activation})
            ))
            .is_err());
        assert_eq!(turn, frozen);
    }

    #[test]
    fn typed_blocker_admission_reserves_response_before_more_waits() {
        for byte_limit in [false, true] {
            let (_root, mut store, activation) = setup();
            let grant = serde_json::to_value(
                &store.unfinished_turn().unwrap().unwrap().1.activations()[0]
                    .input
                    .grant,
            )
            .unwrap();
            let open = json!({"kind":"open_blocker","blocker":{
                "schema_version":1,"blocker_id":"wait-one","activation":activation,
                "kind":{"kind":"human_approval","approval_request":"request-one"},
                "command_id":"requested-command","invocation_id":null,"grant":grant,
                "parameters":"exact-parameters","safe_boundary":"safe-boundary-one","evidence":"wait-evidence"}});
            store.append(envelope(2, open.clone())).unwrap();
            let (reserved_bytes, reserved_commands) =
                settlement_reservation(store.unfinished_turn().unwrap().unwrap().1);
            if byte_limit {
                store.limits.turn_bytes = store
                    .unfinished_turn()
                    .unwrap()
                    .unwrap()
                    .1
                    .retained_event_bytes()
                    + reserved_bytes;
            } else {
                store.limits.turn_commands = 3 + reserved_commands;
            }
            let before = store.stored_files_for_test();
            let mut another = open;
            another["blocker"]["blocker_id"] = "wait-two".into();
            assert!(matches!(
                store.append(envelope(3, another)),
                Err(ExecutionStoreError::Capacity)
            ));
            assert_eq!(store.stored_files_for_test(), before);
            store.append(envelope(3,json!({"kind":"resolve_blocker","blocker_id":"wait-one","activation":activation,
                "response":{"kind":"human_approval","approval_request":"request-one","approval_evidence":"verified-human"}}))).unwrap();
            assert_eq!(
                store
                    .unfinished_turn()
                    .unwrap()
                    .unwrap()
                    .1
                    .pending_blocker_count(),
                0
            );
            for (revision, event) in [
                (
                    4,
                    json!({"kind":"fail_activation","activation":activation,"evidence":"failed-after-response"}),
                ),
                (5, json!({"kind":"interrupt_epoch","epoch_id":"epoch-1"})),
                (6, json!({"kind":"close","closure":"finished"})),
            ] {
                store.append(envelope(revision, event)).unwrap();
            }
            assert!(store.unfinished_turn().unwrap().is_none());
        }
    }

    #[test]
    fn admission_reserves_command_and_byte_capacity_for_outcome_failure_interruption_and_closure() {
        for bytes_limit in [false, true] {
            let (_root, mut store, activation) = setup();
            store.append(envelope(2, json!({"kind":"record_intent", "invocation_id":"tool-1", "activation":activation}))).unwrap();
            if bytes_limit {
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                store.limits.turn_bytes =
                    turn.retained_event_bytes() + settlement_reservation(turn).0;
            } else {
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                store.limits.turn_commands = turn.command_count() + settlement_reservation(turn).1;
            }
            let before = store.stored_files_for_test();
            assert!(matches!(store.append(envelope(3, json!({"kind":"record_intent", "invocation_id":"tool-2", "activation":activation}))), Err(ExecutionStoreError::Capacity)));
            assert_eq!(store.stored_files_for_test(), before);
            for (revision, event) in [
                (
                    3,
                    json!({"kind":"record_outcome", "invocation_id":"tool-1", "outcome":"failed", "evidence":"executor-result"}),
                ),
                (
                    4,
                    json!({"kind":"fail_activation", "activation":activation, "evidence":"check-failed"}),
                ),
                (5, json!({"kind":"interrupt_epoch", "epoch_id":"epoch-1"})),
                (6, json!({"kind":"close", "closure":"finished"})),
            ] {
                store.append(envelope(revision, event)).unwrap();
            }
            assert!(store.unfinished_turn().unwrap().is_none());
            assert_eq!(store.records().unwrap().len(), 7);
        }
    }

    #[test]
    fn lost_acknowledgement_retains_unknown_intent_and_recovers_original_receipt() {
        let (_root, mut store, activation) = setup();
        let guard = store.ownership.clone();
        let owner = store.owner().clone();
        let request = envelope(
            2,
            json!({"kind":"record_intent", "invocation_id":"tool-1", "activation":activation}),
        );
        let expected = DurableTurnReceipt {
            journal_id: store.head.journal_id.clone(),
            sequence: 3,
            envelope: request.clone(),
        };
        store.lose_next_ack = true;
        assert!(store.append(request.clone()).is_err());
        assert!(matches!(
            store.records(),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        assert!(matches!(
            store.append(request.clone()),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(store);
        let mut recovered = SessionExecutionStore::open(guard, owner).unwrap();
        assert_eq!(recovered.append(request).unwrap(), expected);
        let (_, turn) = recovered.unfinished_turn().unwrap().unwrap();
        assert_eq!(turn.state(), Some(LogicalTurnState::NeedsAttention));
        assert_eq!(
            turn.invocations()[0].evidence.disposition(),
            EffectDisposition::OutcomeUnknown
        );
        assert_eq!(turn.revision(), 4);
        assert_eq!(recovered.records().unwrap().len(), 4);
    }

    fn condition_setup() -> (tempfile::TempDir, SessionExecutionStore, serde_json::Value) {
        let root = tempfile::tempdir().unwrap();
        let guard = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let mut store = SessionExecutionStore::open(
            guard,
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: SessionId::new("session-a").unwrap(),
            },
        )
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
        )).unwrap();
        let mut begin = fixture["steps"][0]["envelope"].clone();
        let mut other = begin["event"]["graph"]["conditions"][0].clone();
        other["condition_id"] = json!("second-review");
        begin["event"]["graph"]["conditions"]
            .as_array_mut()
            .unwrap()
            .push(other);
        store
            .append(serde_json::from_value(begin).unwrap())
            .unwrap();
        for step in fixture["steps"].as_array().unwrap().iter().skip(1).take(2) {
            store
                .append(serde_json::from_value(step["envelope"].clone()).unwrap())
                .unwrap();
        }
        let run = json!({
            "session_id":"session-a", "turn_id":"turn-a", "epoch_id":"epoch-1",
            "condition_id":"review", "run_id":"check-run-1",
            "activations":[fixture["steps"][1]["envelope"]["event"]["input"]["activation"].clone()]
        });
        (root, store, run)
    }

    #[test]
    fn condition_intent_reserves_resolution_separately_from_unrun_observation() {
        for capacity in ["turn-records", "turn-bytes"] {
            for resolution in ["outcome_recorded", "not_dispatched"] {
                let (_root, mut store, run) = condition_setup();
                store.append(envelope(3, json!({
                    "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
                }))).unwrap();
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                let (reserved_bytes, reserved_records) = settlement_reservation(turn);
                assert_eq!(reserved_records, 6); // resolution, two observations, Stop, interruption, closure
                match capacity {
                    "turn-records" => {
                        store.limits.turn_commands = turn.command_count() + reserved_records
                    }
                    "turn-bytes" => {
                        store.limits.turn_bytes = turn.retained_event_bytes() + reserved_bytes
                    }
                    _ => unreachable!(),
                }
                let mut competing = run.clone();
                competing["condition_id"] = json!("second-review");
                competing["run_id"] = json!("check-run-2");
                let before = store.stored_files_for_test();
                assert!(matches!(store.append(envelope(4, json!({
                    "kind":"record_condition_intent", "run":competing, "intent":"other-arguments"
                }))), Err(ExecutionStoreError::Capacity)));
                assert_eq!(store.stored_files_for_test(), before);
                store
                    .append(envelope(
                        4,
                        json!({
                            "kind":"resolve_condition_intent", "run_id":"check-run-1",
                            "resolution":{"kind":resolution, "evidence":"actual-executor-evidence"}
                        }),
                    ))
                    .unwrap();
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                assert_eq!(settlement_reservation(turn).1, 5);
                assert!(turn.conditions().is_empty());
                assert!(!turn.has_unknown_effects());
                let mut revision = 5;
                if resolution == "outcome_recorded" {
                    store.append(envelope(revision, json!({
                        "kind":"record_condition", "epoch_id":"epoch-1", "condition_id":"review",
                        "activations":run["activations"], "outcome":"failed", "evidence":"actual-failed-verdict"
                    }))).unwrap();
                    revision += 1;
                }
                store
                    .append(envelope(
                        revision,
                        json!({"kind":"pause_epoch", "epoch_id":"epoch-1"}),
                    ))
                    .unwrap();
                store
                    .append(envelope(
                        revision + 1,
                        json!({"kind":"close", "closure":"finished"}),
                    ))
                    .unwrap();
                assert!(store.unfinished_turn().unwrap().is_none());
            }
        }
    }

    #[test]
    fn lost_condition_intent_acknowledgement_recovers_unknown_without_replay() {
        let (_root, mut store, run) = condition_setup();
        let guard = store.ownership.clone();
        let owner = store.owner().clone();
        let intent = envelope(
            3,
            json!({
                "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
            }),
        );
        store.lose_next_ack = true;
        assert!(matches!(
            store.append(intent.clone()),
            Err(ExecutionStoreError::Io(_))
        ));
        assert!(matches!(
            store.records(),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(store);
        let mut recovered = SessionExecutionStore::open(guard.clone(), owner.clone()).unwrap();
        let receipt = recovered.append(intent.clone()).unwrap();
        let (_, turn) = recovered.unfinished_turn().unwrap().unwrap();
        assert_eq!(turn.state(), Some(LogicalTurnState::NeedsAttention));
        assert!(turn.has_unknown_effects());
        assert_eq!(turn.condition_runs().len(), 1);
        assert_eq!(
            turn.condition_runs()[0].run.activations[0],
            turn.activations()[0].activation
        );
        assert!(turn.conditions().is_empty());
        assert_eq!(recovered.records().unwrap().len(), 5);
        drop(recovered);
        let mut again = SessionExecutionStore::open(guard, owner).unwrap();
        assert_eq!(again.records().unwrap().len(), 5);
        assert_eq!(again.append(intent).unwrap(), receipt);
        again
            .append(envelope(
                5,
                json!({
                    "kind":"resolve_condition_intent", "run_id":"check-run-1",
                    "resolution":{"kind":"outcome_recorded", "evidence":"late-actual-result"}
                }),
            ))
            .unwrap();
        let turn = again.unfinished_turn().unwrap().unwrap().1;
        assert!(!turn.has_unknown_effects());
        assert!(turn.conditions().is_empty());
    }

    #[test]
    fn lost_begin_acknowledgement_recovers_the_same_request_binding() {
        use crate::execution_content::{ExecutionContentStore, ExecutionRequestContent};

        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let owner = ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session-a").unwrap(),
        };
        let mut store = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
        let mut content = ExecutionContentStore::open_owned(
            store
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/interrupted_epoch_preserves_acceptance.json"
        ))
        .unwrap();
        let envelope: TurnContractEnvelope =
            serde_json::from_value(fixture["steps"][0]["envelope"].clone()).unwrap();
        let request = content
            .retain_request(ExecutionRequestContent {
                turn_id: envelope.turn_id.clone(),
                recorded_at_unix_ms: 42,
                display_input: "Original user request".into(),
                effective_input: "Original user request and supplied context".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        store.lose_next_ack = true;
        assert!(store
            .begin_with_request(envelope.clone(), &request)
            .is_err());
        assert!(matches!(
            store.snapshot(&envelope.turn_id),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(content);
        drop(store);
        let mut recovered = SessionExecutionStore::open(ownership, owner).unwrap();
        let snapshot = recovered.snapshot(&envelope.turn_id).unwrap();
        assert_eq!(snapshot.request_ref(), Some(request.reference()));
        assert_eq!(
            snapshot.contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        let receipt = recovered.begin_with_request(envelope, &request).unwrap();
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(recovered.records().unwrap().len(), 2);
    }

    #[test]
    fn lost_legacy_seal_acknowledgement_recovers_the_same_frontier() {
        use crate::execution_content::ExecutionContentStore;
        use crate::SessionTurnStore;

        let root = tempfile::tempdir().unwrap();
        SessionTurnStore::open(root.path().join("session-history")).unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let owner = ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session-a").unwrap(),
        };
        let mut store = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
        let mut content = ExecutionContentStore::open_owned(
            store
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let history = content
            .retain_legacy_history(&store.legacy_history_snapshot().unwrap())
            .unwrap();
        store.lose_next_ack = true;
        assert!(store.seal_legacy_history(&history).is_err());
        assert!(matches!(
            store.legacy_seal(),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(content);
        drop(store);
        let mut recovered = SessionExecutionStore::open(ownership, owner).unwrap();
        let seal = recovered.seal_legacy_history(&history).unwrap();
        assert_eq!(seal.reference(), history.reference());
        assert!(seal.last_predecessor().is_none());
        assert!(recovered.records().unwrap().is_empty());
    }
}

#[cfg(all(test, unix))]
#[path = "execution_store_segment_tests.rs"]
mod segment_tests;

#[cfg(all(test, unix))]
#[path = "execution_store_turn_stop_tests.rs"]
mod turn_stop_tests;
