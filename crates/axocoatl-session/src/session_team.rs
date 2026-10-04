//! Versioned future Session configuration under its actual canonical owner.
//!
//! Applying a team revision never rewrites a turn graph, promotes a checkpoint,
//! grants execution authority, or mutates a reusable Agent definition. The host
//! selects one immutable revision at Begin, resolves its exact retained inputs,
//! and supplies verified committed savepoints. Layout is presentation only.
//!
//! Storage: `session-team.v1.json` is a small head (identity and a
//! [`SegmentsMarker`]); every revision is one record of a segmented log beside
//! it, so a Session can apply any number of Team changes. A store that still
//! holds the older single-file layout, with every revision inline in that
//! file, is converted on open.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution_content::{
    ActivationEvidenceContent, ExecutionContentError, ExecutionContentStore,
};
use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_store::{
    DurableSessionIdentity, ExecutionStoreError, ExecutionStoreOwner, SessionExecutionStore,
};
use crate::segment_log::{
    KeyFilter, SegmentCache, SegmentError, SegmentLog, SegmentSpec, SegmentsMarker,
};
use crate::turn_contract::{
    CommandId, CompletionCondition, ConditionKind, ConversationSavepoint, DefinitionSnapshotRef,
    DependencyEdge, EvidenceRef, GraphNode, GraphSnapshotId, LogicalTurnId, NodeConversationId,
    SessionTeamSlotId, TurnContractError, TurnContractEvent, TurnGraphSnapshot, TurnNodeId,
    MAX_CONTRACT_ENVELOPE_BYTES, MAX_CONTRACT_NODES, MAX_RETAINED_CONTRACT_BYTES,
};

const FILE: &str = "session-team.v1.json";
pub const SESSION_TEAM_SCHEMA_VERSION: u32 = 1;

/// The revision log. A segment is sealed at 512 KiB or 256 revisions, so the
/// active segment, which stays in memory, is bounded; one revision is bounded
/// by the per-request maximum.
const SPEC: SegmentSpec = SegmentSpec {
    name: "session-team",
    kind: "session-team",
    segment_bytes: 512 * 1024,
    segment_records: 256,
    record_bytes: MAX_CONTRACT_ENVELOPE_BYTES + RECORD_FRAME_BYTES,
};
/// A record line wraps one revision as `{"record":<revision>}\n`.
const RECORD_FRAME_BYTES: usize = 16;
/// Decoded sealed segments kept for repeated lookups of older revisions.
const CACHED_SEGMENTS: usize = 4;
/// The older single-file layout never exceeded this size.
const LEGACY_FILE_BYTES: usize = MAX_RETAINED_CONTRACT_BYTES;
/// The log's directory of sealed segments (see `segment_log`'s layout).
const SEALED_DIR: &str = "segments";
/// Conversations the canonical prefix declares are held exactly until this
/// many accumulate; older ones are summarized by one filter per chunk.
const FRONTIER_CHUNK_KEYS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamSlot {
    pub slot_id: SessionTeamSlotId,
    pub node_id: TurnNodeId,
    pub definition: DefinitionSnapshotRef,
    pub conversation_id: NodeConversationId,
    pub required: bool,
    /// Exact retained Budget body, not a current turn's authority grant.
    pub budget: EvidenceRef,
    /// Explicit human-approved exact policy. Absence preserves older team
    /// records and provides no authority to execute a future turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamGraph {
    pub slots: Vec<SessionTeamSlot>,
    pub dependencies: Vec<DependencyEdge>,
    pub conditions: Vec<CompletionCondition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamPosition {
    pub slot_id: SessionTeamSlotId,
    pub x: f64,
    pub y: f64,
}

/// Every proposed slot has an explicit continuity decision in the Apply diff.
/// No reference alone establishes provider-native transcript compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionTeamContinuity {
    /// Same current slot, exact definition snapshot, and conversation identity.
    PreserveUnchanged,
    /// New/replaced/reset work starts a conversation never used by this team.
    Reset,
    /// Requires the host's real accepted-history projection proof. This store
    /// provides no default validator that turns a JSON assertion into proof.
    PreserveWithProjection { evidence: EvidenceRef },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotContinuityDecision {
    pub slot_id: SessionTeamSlotId,
    pub decision: SessionTeamContinuity,
}

/// Explicit initial import from an actually retained graph. Selected slots in
/// the Apply payload are the membership; no dynamic node is silently imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamSource {
    pub turn_id: LogicalTurnId,
    pub snapshot_id: GraphSnapshotId,
    pub graph_revision: u64,
}

#[derive(Debug, Clone)]
pub struct SessionTeamConversationSource {
    pub slot_id: SessionTeamSlotId,
    pub definition: DefinitionSnapshotRef,
    pub conversation_id: NodeConversationId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamCommit {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub expected_configuration_revision: u64,
    pub graph: SessionTeamGraph,
    pub initial_source: Option<SessionTeamSource>,
    pub continuity: Vec<SlotContinuityDecision>,
    /// Presentation coordinates do not become dependencies or execution state.
    pub layout: Vec<SessionTeamPosition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamRevision {
    pub schema_version: u32,
    pub configuration_revision: u64,
    /// Canonical prefix observed at Apply; later turns may legitimately use the
    /// new conversations and must not invalidate their original fresh allocation.
    pub canonical_record_count: u64,
    pub command_id: CommandId,
    pub expected_configuration_revision: u64,
    pub graph: SessionTeamGraph,
    pub initial_source: Option<SessionTeamSource>,
    pub continuity: Vec<SlotContinuityDecision>,
    pub layout: Vec<SessionTeamPosition>,
}
impl SessionTeamRevision {
    fn from_commit(revision: u64, canonical_record_count: u64, request: SessionTeamCommit) -> Self {
        Self {
            schema_version: request.schema_version,
            configuration_revision: revision,
            canonical_record_count,
            command_id: request.command_id,
            expected_configuration_revision: request.expected_configuration_revision,
            graph: request.graph,
            initial_source: request.initial_source,
            continuity: request.continuity,
            layout: request.layout,
        }
    }
    fn request(&self) -> SessionTeamCommit {
        SessionTeamCommit {
            schema_version: self.schema_version,
            command_id: self.command_id.clone(),
            expected_configuration_revision: self.expected_configuration_revision,
            graph: self.graph.clone(),
            initial_source: self.initial_source.clone(),
            continuity: self.continuity.clone(),
            layout: self.layout.clone(),
        }
    }

    /// Structural construction only. The host must verify actual committed
    /// checkpoint bytes and select this immutable revision under Begin's lock.
    /// Every slot requires an explicit savepoint, including explicit Empty.
    pub fn initial_graph(
        &self,
        owner: &ExecutionStoreOwner,
        snapshot_id: GraphSnapshotId,
        savepoints: &[(SessionTeamSlotId, ConversationSavepoint)],
    ) -> Result<TurnGraphSnapshot, SessionTeamError> {
        let points: HashMap<_, _> = savepoints
            .iter()
            .map(|(slot, point)| (slot, point))
            .collect();
        if points.len() != savepoints.len()
            || points.len() != self.graph.slots.len()
            || self
                .graph
                .slots
                .iter()
                .any(|slot| !points.contains_key(&slot.slot_id))
        {
            return Err(SessionTeamError::Invalid(
                "initial graph requires exactly one explicit savepoint per slot",
            ));
        }
        let graph = TurnGraphSnapshot {
            snapshot_id,
            revision: 1,
            nodes: self
                .graph
                .slots
                .iter()
                .map(|slot| GraphNode {
                    node_id: slot.node_id.clone(),
                    slot_id: slot.slot_id.clone(),
                    definition: slot.definition.clone(),
                    conversation_id: slot.conversation_id.clone(),
                    required: slot.required,
                    starting_savepoint: (*points[&slot.slot_id]).clone(),
                })
                .collect(),
            dependencies: self.graph.dependencies.clone(),
            conditions: self.graph.conditions.clone(),
        };
        graph.validate(&owner.session_id)?;
        Ok(graph)
    }
}

/// Implemented only by a host that resolves actual accepted history and proves
/// its selected projection for both exact definitions. Validation is repeated on
/// reopen. A missing validator refuses preservation across changed definitions;
/// storing a reference, a user choice, or matching model names cannot approve it.
pub trait SessionTeamHistoryValidator {
    fn validate_preservation(
        &self,
        identity: &DurableSessionIdentity,
        previous: &SessionTeamConversationSource,
        proposed: &SessionTeamSlot,
        evidence: &EvidenceRef,
        content: &ExecutionContentStore,
    ) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum SessionTeamError {
    #[error("Session team storage: {0}")]
    Io(#[from] io::Error),
    #[error("Session team encoding: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Session team evidence: {0}")]
    Content(#[from] ExecutionContentError),
    #[error("Session team canonical history: {0}")]
    Canonical(#[from] ExecutionStoreError),
    #[error("Session team graph: {0}")]
    Graph(#[from] TurnContractError),
    #[error("invalid Session team: {0}")]
    Invalid(&'static str),
    #[error("Session team belongs to another canonical owner")]
    OwnerConflict,
    #[error("Session team command identity already has a different payload")]
    CommandConflict,
    #[error("Session configuration changed; expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("Session team revision exceeds its size bound; no revision was applied")]
    Capacity,
    #[error("Session team write is uncertain; reopen before acknowledging configuration")]
    RecoveryRequired,
    #[error("accepted-history preservation has not been verified: {0}")]
    UnverifiedPreservation(String),
}

impl From<SegmentError> for SessionTeamError {
    fn from(error: SegmentError) -> Self {
        match error {
            SegmentError::Io(error) => Self::Io(error),
            SegmentError::Json(error) => Self::Json(error),
            SegmentError::Invalid(reason) => Self::Invalid(reason),
            SegmentError::RecordTooLarge => Self::Capacity,
            SegmentError::RecoveryRequired => Self::RecoveryRequired,
            SegmentError::Changed => {
                Self::Invalid("the Session team log changed while it was read")
            }
        }
    }
}

/// The primary file of a segmented store. It never grows: revisions live in
/// the log, and the marker makes an older daemon, which knows only the inline
/// layout, refuse the store instead of reading it as a team without history.
#[derive(Serialize)]
struct TeamHead<'a> {
    schema_version: u32,
    canonical_journal_id: &'a str,
    owner: &'a ExecutionStoreOwner,
    segments: SegmentsMarker,
}

/// The primary file as read: a head, or the older layout that held every
/// revision inline in this one file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTeam {
    schema_version: u32,
    canonical_journal_id: String,
    owner: ExecutionStoreOwner,
    #[serde(default)]
    segments: Option<SegmentsMarker>,
    #[serde(default)]
    revisions: Option<Vec<SessionTeamRevision>>,
}

/// The identity every segment header of the log repeats.
#[derive(Serialize)]
struct LogMeta<'a> {
    canonical_journal_id: &'a str,
    owner: &'a ExecutionStoreOwner,
}

enum Layout {
    New,
    Segmented,
    Legacy(Vec<SessionTeamRevision>),
}

/// One writer, bound to the canonical Session incarnation. Every acknowledged
/// revision is retained, with no limit on how many a Session applies.
///
/// Memory does not grow with the number of revisions except for one small key
/// filter per sealed segment (built on the first lookup that needs them) and
/// the canonical frontier's filters. It holds the active segment's revisions,
/// the latest revision, and a few decoded sealed segments; any other revision
/// is read back from its sealed segment on demand.
pub struct SessionTeamStore {
    namespace: OwnedExecutionNamespace,
    log: SegmentLog,
    /// Revisions of the active segment, in order.
    active: Vec<Arc<SessionTeamRevision>>,
    latest: Option<Arc<SessionTeamRevision>>,
    /// Command identities and slot conversations of each sealed segment,
    /// aligned with `log.sealed()`.
    filters: Mutex<Option<Vec<KeyFilter>>>,
    cache: SegmentCache<SessionTeamRevision>,
    /// The canonical prefix the next revision is validated against.
    frontier: Mutex<CanonicalFrontier>,
    poisoned: bool,
}
impl SessionTeamStore {
    pub fn open_owned(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<Self, SessionTeamError> {
        Self::open_with(namespace, canonical, content, history, SPEC)
    }

    /// Open with the given log shape; tests use small segments.
    fn open_with(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
        spec: SegmentSpec,
    ) -> Result<Self, SessionTeamError> {
        namespace.require_root(&ExecutionComponent::SessionTeam)?;
        let identity = namespace.identity().clone();
        if canonical.identity()? != identity {
            return Err(SessionTeamError::OwnerConflict);
        }
        content.require_owned_identity(&identity)?;
        let dir = namespace.secure_dir()?;
        let meta = serde_json::to_value(LogMeta {
            canonical_journal_id: identity.journal_id(),
            owner: identity.owner(),
        })?;
        let layout = match namespace.read_limited(FILE, LEGACY_FILE_BYTES) {
            Ok(bytes) => {
                let stored = serde_json::from_slice::<StoredTeam>(&bytes)?;
                if stored.owner != *identity.owner()
                    || stored.canonical_journal_id != identity.journal_id()
                {
                    return Err(SessionTeamError::OwnerConflict);
                }
                if stored.schema_version != SESSION_TEAM_SCHEMA_VERSION {
                    return Err(SessionTeamError::Invalid("unsupported team store schema"));
                }
                match (stored.segments, stored.revisions) {
                    (Some(marker), None) if marker.matches(&spec) => Layout::Segmented,
                    (None, Some(revisions)) => Layout::Legacy(revisions),
                    _ => return Err(SessionTeamError::Invalid("unsupported team store layout")),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                namespace.check_journal_creation(FILE)?;
                Layout::New
            }
            Err(error) => return Err(error.into()),
        };
        let create = matches!(layout, Layout::New);
        let publish_head = !matches!(layout, Layout::Segmented);
        match layout {
            // Marker first: an interrupted creation stays fail-closed instead
            // of ever looking like a new empty team.
            Layout::New => namespace.mark_journal_initialized(FILE)?,
            Layout::Legacy(revisions) => convert_legacy(&dir, spec, &meta, &revisions)?,
            Layout::Segmented => {}
        }
        let inputs = Inputs {
            identity: &identity,
            canonical,
            content,
            history,
        };
        let loaded = load(&dir, spec, &meta, create, &inputs)?;
        if !create {
            namespace.mark_journal_initialized(FILE)?;
        }
        if publish_head {
            // Until this write, a crash leaves a new store fail-closed and a
            // legacy store converted again from its untouched inline file.
            let head = TeamHead {
                schema_version: SESSION_TEAM_SCHEMA_VERSION,
                canonical_journal_id: identity.journal_id(),
                owner: identity.owner(),
                segments: SegmentsMarker::of(&spec),
            };
            namespace.atomic_write(FILE, &serde_json::to_vec(&head)?)?;
        }
        Ok(Self {
            namespace,
            log: loaded.log,
            active: loaded.active,
            latest: loaded.latest,
            filters: Mutex::new(None),
            cache: SegmentCache::new(CACHED_SEGMENTS),
            frontier: Mutex::new(loaded.frontier),
            poisoned: false,
        })
    }

    pub fn configuration_revision(&self) -> Result<u64, SessionTeamError> {
        self.healthy()?;
        Ok(self.count())
    }
    pub fn current(&self) -> Result<Option<&SessionTeamRevision>, SessionTeamError> {
        self.healthy()?;
        Ok(self.latest.as_deref())
    }
    /// One applied revision; an older one is read back from its segment.
    pub fn get(&self, revision: u64) -> Result<Option<Arc<SessionTeamRevision>>, SessionTeamError> {
        self.healthy()?;
        if revision == 0 || revision > self.count() {
            return Ok(None);
        }
        self.revision(revision).map(Some)
    }
    /// The revision an exact command identity applied, if any.
    pub fn find_command(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<Arc<SessionTeamRevision>>, SessionTeamError> {
        self.healthy()?;
        self.find(&command_key(command_id), |revision| {
            &revision.command_id == command_id
        })
    }
    pub fn identity(&self) -> &DurableSessionIdentity {
        self.namespace.identity()
    }

    /// Validate the entire candidate and all retained inputs without changing
    /// this configuration journal. The returned projection is not an Apply receipt.
    pub fn preview(
        &self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<SessionTeamRevision, SessionTeamError> {
        self.prepare_commit(request, canonical, content, history)
            .map(|(record, _)| record.as_ref().clone())
    }

    pub fn commit(
        &mut self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<SessionTeamRevision, SessionTeamError> {
        self.commit_with(request, canonical, content, history, |log, line| {
            log.append_line(line)
        })
    }
    fn commit_with(
        &mut self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
        append: impl FnOnce(&mut SegmentLog, &[u8]) -> Result<(), SegmentError>,
    ) -> Result<SessionTeamRevision, SessionTeamError> {
        let (record, line) = self.prepare_commit(request, canonical, content, history)?;
        let Some(line) = line else {
            return Ok(record.as_ref().clone());
        };
        if let Err(error) = append(&mut self.log, &line) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.active.push(record.clone());
        self.latest = Some(record.clone());
        // The revision is durable once appended. A seal that fails leaves this
        // handle refusing further use; reopening completes the seal.
        if self.log.should_seal() && self.seal_active().is_err() {
            self.poisoned = true;
        }
        Ok(record.as_ref().clone())
    }
    fn prepare_commit(
        &self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<(Arc<SessionTeamRevision>, Option<Vec<u8>>), SessionTeamError> {
        self.healthy()?;
        if canonical.identity()? != *self.identity() {
            return Err(SessionTeamError::OwnerConflict);
        }
        content.require_owned_identity(self.identity())?;
        encoded(&request, MAX_CONTRACT_ENVELOPE_BYTES)?;
        // An exact retry is its original configuration receipt, even after later
        // edits or a different provider projection becomes unavailable.
        if let Some(record) = self.find(&command_key(&request.command_id), |record| {
            record.command_id == request.command_id
        })? {
            return if record.request() == request {
                Ok((record, None))
            } else {
                Err(SessionTeamError::CommandConflict)
            };
        }
        let actual = self.count();
        if request.expected_configuration_revision != actual {
            return Err(SessionTeamError::RevisionConflict {
                expected: request.expected_configuration_revision,
                actual,
            });
        }
        let mut frontier = self
            .frontier
            .lock()
            .map_err(|_| SessionTeamError::Invalid("Session team frontier lock poisoned"))?;
        let canonical_count = canonical_len(canonical)?;
        frontier.advance(canonical, canonical_count)?;
        let record = SessionTeamRevision::from_commit(actual + 1, canonical_count, request);
        let prior = StoreLookups {
            store: self,
            canonical,
            frontier: &frontier,
        };
        validate_revision(
            &record,
            actual,
            self.latest.as_deref(),
            self.identity(),
            &prior,
            content,
            history,
        )?;
        let line = self.log.encode_record(&record)?;
        Ok((Arc::new(record), Some(line)))
    }
    fn healthy(&self) -> Result<(), SessionTeamError> {
        if self.poisoned {
            return Err(SessionTeamError::RecoveryRequired);
        }
        self.namespace.verify_ambient_identity()?;
        Ok(())
    }

    fn count(&self) -> u64 {
        self.log.next_sequence() - 1
    }

    fn revision(&self, sequence: u64) -> Result<Arc<SessionTeamRevision>, SessionTeamError> {
        let first = self.log.active_first_sequence();
        let found = if sequence >= first {
            usize::try_from(sequence - first)
                .ok()
                .and_then(|at| self.active.get(at).cloned())
        } else if let Some(segment) = self.log.segment_of(sequence) {
            let revisions = self.cache.get(&self.log, segment)?;
            usize::try_from(sequence - segment.first_sequence)
                .ok()
                .and_then(|at| revisions.get(at).cloned())
        } else {
            None
        };
        found
            .filter(|revision| revision.configuration_revision == sequence)
            .ok_or(SessionTeamError::Invalid(
                "an applied team revision is missing from its log",
            ))
    }

    /// A revision holding `key` that `matches` confirms: the active segment
    /// exactly, then every sealed segment whose filter does not rule it out.
    fn find(
        &self,
        key: &str,
        matches: impl Fn(&SessionTeamRevision) -> bool,
    ) -> Result<Option<Arc<SessionTeamRevision>>, SessionTeamError> {
        if let Some(revision) = self.active.iter().find(|revision| matches(revision)) {
            return Ok(Some(revision.clone()));
        }
        let sealed = self.log.sealed();
        let mut filters = self
            .filters
            .lock()
            .map_err(|_| SessionTeamError::Invalid("Session team filter lock poisoned"))?;
        if filters
            .as_ref()
            .is_none_or(|filters| filters.len() != sealed.len())
        {
            let mut built = Vec::with_capacity(sealed.len());
            for segment in sealed {
                built.push(segment_filter(
                    &self.log.read_sealed::<SessionTeamRevision>(segment)?,
                ));
            }
            *filters = Some(built);
        }
        let Some(filters) = filters.as_ref() else {
            return Err(SessionTeamError::Invalid(
                "Session team filters are missing",
            ));
        };
        for (segment, filter) in sealed.iter().zip(filters) {
            if !filter.may_contain(key) {
                continue;
            }
            let revisions = self.cache.get(&self.log, segment)?;
            if let Some(revision) = revisions.iter().find(|revision| matches(revision)) {
                return Ok(Some(revision.clone()));
            }
        }
        Ok(None)
    }

    fn seal_active(&mut self) -> Result<(), SessionTeamError> {
        let filter = segment_filter(self.active.iter().map(Arc::as_ref));
        self.log.seal()?;
        self.active.clear();
        match self.filters.get_mut() {
            Ok(Some(filters)) => filters.push(filter),
            Ok(None) => {}
            Err(_) => {
                return Err(SessionTeamError::Invalid(
                    "Session team filter lock poisoned",
                ))
            }
        }
        Ok(())
    }
}

struct Inputs<'a> {
    identity: &'a DurableSessionIdentity,
    canonical: &'a SessionExecutionStore,
    content: &'a ExecutionContentStore,
    history: Option<&'a dyn SessionTeamHistoryValidator>,
}

struct Loaded {
    log: SegmentLog,
    active: Vec<Arc<SessionTeamRevision>>,
    latest: Option<Arc<SessionTeamRevision>>,
    frontier: CanonicalFrontier,
}

/// Open the log and validate every revision in order, exactly as it was
/// validated when it was applied. Besides the bounded working set, this holds
/// a 16-byte fingerprint per revision and per team conversation, dropped on
/// return, so each revision is checked against all earlier ones in one pass.
fn load(
    dir: &SecureDir,
    spec: SegmentSpec,
    meta: &serde_json::Value,
    create: bool,
    inputs: &Inputs<'_>,
) -> Result<Loaded, SessionTeamError> {
    // A sealed segment that is visible must be durable before opening may
    // complete an interrupted seal by replacing the active segment.
    if dir.has_exact_directory(SEALED_DIR)? {
        dir.existing_child(SEALED_DIR)?.sync_all()?;
    }
    let mut frontier = CanonicalFrontier::default();
    let mut commands = HashSet::new();
    let mut conversations = HashSet::new();
    let mut recent = RecentRevisions::new(&spec);
    let mut latest: Option<Arc<SessionTeamRevision>> = None;
    let mut log = SegmentLog::open(
        dir.clone(),
        spec,
        meta.clone(),
        create,
        |sequence, record: SessionTeamRevision| {
            if !commands.insert(fingerprint(record.command_id.as_str())) {
                return Err(SessionTeamError::CommandConflict);
            }
            frontier.advance(inputs.canonical, record.canonical_record_count)?;
            let prior = OpenLookups {
                canonical: inputs.canonical,
                frontier: &frontier,
                conversations: &conversations,
            };
            validate_revision(
                &record,
                sequence - 1,
                latest.as_deref(),
                inputs.identity,
                &prior,
                inputs.content,
                inputs.history,
            )?;
            conversations.extend(
                record
                    .graph
                    .slots
                    .iter()
                    .map(|slot| fingerprint(slot.conversation_id.as_str())),
            );
            let record = Arc::new(record);
            recent.push(sequence, record.clone())?;
            latest = Some(record);
            Ok(())
        },
    )?;
    let first = log.active_first_sequence();
    let mut active = match recent.since(first, log.active_records()) {
        Some(active) => active,
        // Only an active segment larger than this store's segments gets here.
        None => read_active(dir, spec, meta, first)?,
    };
    if active.len() as u64 != log.active_records() {
        return Err(SessionTeamError::Invalid(
            "the active Session team segment changed while it was read",
        ));
    }
    // Finish a seal that an earlier writer could not complete.
    if log.should_seal() {
        log.seal()?;
        active.clear();
    }
    // Re-acknowledge durability of everything just read: an append whose sync
    // failed may still be visible, and it is now treated as applied.
    dir.open_append(spec.active_name())?.sync_all()?;
    dir.sync_all()?;
    Ok(Loaded {
        log,
        active,
        latest,
        frontier,
    })
}

/// Read the active segment's revisions again, for an active segment larger
/// than [`RecentRevisions`] keeps.
fn read_active(
    dir: &SecureDir,
    spec: SegmentSpec,
    meta: &serde_json::Value,
    first: u64,
) -> Result<Vec<Arc<SessionTeamRevision>>, SessionTeamError> {
    let active = RefCell::new(Vec::new());
    SegmentLog::read(
        dir.clone(),
        spec,
        meta.clone(),
        || active.borrow_mut().clear(),
        |sequence, record: SessionTeamRevision| {
            if sequence >= first {
                active.borrow_mut().push(Arc::new(record));
            }
            Ok::<(), SessionTeamError>(())
        },
    )?;
    Ok(active.into_inner())
}

/// Copy the inline layout's revisions, in order, into a new log, sealing as
/// segments fill. The head replaces the inline file only after this log
/// reopens and every revision validates, so a crash at any point before that
/// converts again from the untouched inline file.
fn convert_legacy(
    dir: &SecureDir,
    spec: SegmentSpec,
    meta: &serde_json::Value,
    revisions: &[SessionTeamRevision],
) -> Result<(), SessionTeamError> {
    SegmentLog::remove(dir, &spec)?;
    let mut log = SegmentLog::open(
        dir.clone(),
        spec,
        meta.clone(),
        true,
        |_, _: SessionTeamRevision| {
            Err(SessionTeamError::Invalid(
                "a removed Session team log still had revisions",
            ))
        },
    )?;
    for revision in revisions {
        let line = log.encode_record(revision)?;
        log.append_line(&line)?;
        if log.should_seal() {
            log.seal()?;
        }
    }
    Ok(())
}

/// The last revisions read while opening, bounded like an active segment, so
/// the active segment's revisions are at hand without reading it again.
struct RecentRevisions {
    revisions: VecDeque<(u64, Arc<SessionTeamRevision>, usize)>,
    bytes: usize,
    max_revisions: usize,
    max_bytes: usize,
}
impl RecentRevisions {
    fn new(spec: &SegmentSpec) -> Self {
        Self {
            revisions: VecDeque::new(),
            bytes: 0,
            max_revisions: usize::try_from(spec.segment_records).unwrap_or(usize::MAX),
            max_bytes: spec.segment_bytes.saturating_add(spec.record_bytes),
        }
    }
    fn push(
        &mut self,
        sequence: u64,
        revision: Arc<SessionTeamRevision>,
    ) -> Result<(), SessionTeamError> {
        let bytes = encoded_len(revision.as_ref())?;
        self.revisions.push_back((sequence, revision, bytes));
        self.bytes += bytes;
        while self.revisions.len() > self.max_revisions || self.bytes > self.max_bytes {
            let Some((_, _, bytes)) = self.revisions.pop_front() else {
                break;
            };
            self.bytes -= bytes;
        }
        Ok(())
    }
    /// The revisions from sequence `first` on, if all `count` are still held.
    fn since(self, first: u64, count: u64) -> Option<Vec<Arc<SessionTeamRevision>>> {
        let since = self
            .revisions
            .into_iter()
            .filter(|(sequence, ..)| *sequence >= first)
            .map(|(_, revision, _)| revision)
            .collect::<Vec<_>>();
        (since.len() as u64 == count).then_some(since)
    }
}

fn encoded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, SessionTeamError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.limit {
                return Err(io::Error::other("Session team encoding limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| SessionTeamError::Capacity)?;
    Ok(writer.bytes)
}

fn encoded_len(value: &impl Serialize) -> Result<usize, SessionTeamError> {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

/// A collision-resistant stand-in for a key, so opening can compare each
/// revision with every earlier one without holding their contents.
type Fingerprint = [u8; 16];

fn fingerprint(key: &str) -> Fingerprint {
    let digest = Sha256::digest(key.as_bytes());
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&digest[..16]);
    fingerprint
}

fn command_key(id: &CommandId) -> String {
    format!("command:{}", id.as_str())
}

fn conversation_key(id: &NodeConversationId) -> String {
    format!("conversation:{}", id.as_str())
}

/// One sealed segment's filter: its command identities and the conversations
/// of all its slots.
fn segment_filter<'a>(revisions: impl IntoIterator<Item = &'a SessionTeamRevision>) -> KeyFilter {
    let mut keys = HashSet::new();
    for revision in revisions {
        keys.insert(command_key(&revision.command_id));
        keys.extend(
            revision
                .graph
                .slots
                .iter()
                .map(|slot| conversation_key(&slot.conversation_id)),
        );
    }
    KeyFilter::new(keys.iter().map(String::as_str))
}

/// The number of records in the canonical journal. With [`canonical_graphs`]
/// it is the only way this store reads the canonical journal.
fn canonical_len(canonical: &SessionExecutionStore) -> Result<u64, SessionTeamError> {
    // Reading the turn index proves the store still holds its journal.
    canonical.turn_ids()?;
    Ok(canonical.record_count())
}

/// Canonical records read into memory at once by [`canonical_graphs`].
const CANONICAL_CHUNK: u64 = 1024;

/// Hand `visit` each graph a Begin or ReviseGraph declares in canonical
/// records `[from, to)` (zero-based positions), in order, with its position.
/// The canonical store has already validated every envelope; only these
/// immutable graph declarations introduce conversation identity.
fn canonical_graphs(
    canonical: &SessionExecutionStore,
    from: u64,
    to: u64,
    mut visit: impl FnMut(u64, &LogicalTurnId, &TurnGraphSnapshot) -> Result<(), SessionTeamError>,
) -> Result<(), SessionTeamError> {
    if from > to || to > canonical_len(canonical)? {
        return Err(SessionTeamError::Invalid(
            "configuration canonical prefix is absent or moves backward",
        ));
    }
    // Position p is canonical sequence p + 1. Read a chunk at a time so that
    // memory stays bounded however long the journal is.
    let mut start = from;
    while start < to {
        let end = start.saturating_add(CANONICAL_CHUNK).min(to);
        for (sequence, event) in canonical.records_in(start + 1, end)? {
            if let TurnContractEvent::Begin { graph, .. }
            | TurnContractEvent::ReviseGraph { graph, .. } = &event.event
            {
                visit(sequence - 1, &event.turn_id, graph)?;
            }
        }
        start = end;
    }
    Ok(())
}

/// The conversations the graphs of a canonical prefix declare. It only moves
/// forward, as the canonical journal only grows. Declarations since the last
/// chunk are held exactly; each older chunk is kept as one filter and
/// confirmed by reading its range of the canonical journal again.
#[derive(Default)]
struct CanonicalFrontier {
    count: u64,
    open_start: u64,
    open: HashSet<NodeConversationId>,
    chunks: Vec<DeclaredChunk>,
}

struct DeclaredChunk {
    start: u64,
    end: u64,
    filter: KeyFilter,
}

impl CanonicalFrontier {
    fn advance(
        &mut self,
        canonical: &SessionExecutionStore,
        count: u64,
    ) -> Result<(), SessionTeamError> {
        if count < self.count {
            return Err(SessionTeamError::Invalid(
                "configuration canonical prefix is absent or moves backward",
            ));
        }
        let from = self.count;
        let open = &mut self.open;
        let open_start = &mut self.open_start;
        let chunks = &mut self.chunks;
        let result = canonical_graphs(canonical, from, count, |position, _, graph| {
            open.extend(graph.nodes.iter().map(|node| node.conversation_id.clone()));
            if open.len() >= FRONTIER_CHUNK_KEYS {
                let end = position + 1;
                chunks.push(DeclaredChunk {
                    start: *open_start,
                    end,
                    filter: KeyFilter::new(open.iter().map(NodeConversationId::as_str)),
                });
                open.clear();
                *open_start = end;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                self.count = count;
                Ok(())
            }
            Err(error) => {
                // A partial advance is discarded; the next one starts over.
                *self = Self::default();
                Err(error)
            }
        }
    }

    fn declares(
        &self,
        canonical: &SessionExecutionStore,
        conversation: &NodeConversationId,
    ) -> Result<bool, SessionTeamError> {
        if self.open.contains(conversation) {
            return Ok(true);
        }
        for chunk in &self.chunks {
            if !chunk.filter.may_contain(conversation.as_str()) {
                continue;
            }
            let mut found = false;
            canonical_graphs(canonical, chunk.start, chunk.end, |_, _, graph| {
                found |= graph
                    .nodes
                    .iter()
                    .any(|node| &node.conversation_id == conversation);
                Ok(())
            })?;
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The graph `turn` has at the end of this prefix.
    fn graph(
        &self,
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
    ) -> Result<Option<TurnGraphSnapshot>, SessionTeamError> {
        let mut found = None;
        canonical_graphs(canonical, 0, self.count, |_, id, graph| {
            if id == turn {
                found = Some(graph.clone());
            }
            Ok(())
        })?;
        Ok(found)
    }
}

/// What validating one revision needs to know about what came before it.
trait PriorLookups {
    /// Whether the revision's canonical prefix or an earlier revision of this
    /// team already used `conversation`.
    fn conversation_used(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<bool, SessionTeamError>;
    /// The graph `turn` has at the end of the revision's canonical prefix.
    fn canonical_graph(
        &self,
        turn: &LogicalTurnId,
    ) -> Result<Option<TurnGraphSnapshot>, SessionTeamError>;
}

/// While opening, earlier revisions are known by fingerprint.
struct OpenLookups<'a> {
    canonical: &'a SessionExecutionStore,
    frontier: &'a CanonicalFrontier,
    conversations: &'a HashSet<Fingerprint>,
}
impl PriorLookups for OpenLookups<'_> {
    fn conversation_used(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<bool, SessionTeamError> {
        Ok(self.frontier.declares(self.canonical, conversation)?
            || self
                .conversations
                .contains(&fingerprint(conversation.as_str())))
    }
    fn canonical_graph(
        &self,
        turn: &LogicalTurnId,
    ) -> Result<Option<TurnGraphSnapshot>, SessionTeamError> {
        self.frontier.graph(self.canonical, turn)
    }
}

/// For a new revision, earlier revisions are found through the log.
struct StoreLookups<'a> {
    store: &'a SessionTeamStore,
    canonical: &'a SessionExecutionStore,
    frontier: &'a CanonicalFrontier,
}
impl PriorLookups for StoreLookups<'_> {
    fn conversation_used(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<bool, SessionTeamError> {
        Ok(self.frontier.declares(self.canonical, conversation)?
            || self
                .store
                .find(&conversation_key(conversation), |revision| {
                    revision
                        .graph
                        .slots
                        .iter()
                        .any(|slot| &slot.conversation_id == conversation)
                })?
                .is_some())
    }
    fn canonical_graph(
        &self,
        turn: &LogicalTurnId,
    ) -> Result<Option<TurnGraphSnapshot>, SessionTeamError> {
        self.frontier.graph(self.canonical, turn)
    }
}

/// Validate one revision against the `count` revisions before it, the last
/// of which is `last`.
fn validate_revision(
    record: &SessionTeamRevision,
    count: u64,
    last: Option<&SessionTeamRevision>,
    identity: &DurableSessionIdentity,
    prior: &dyn PriorLookups,
    content: &ExecutionContentStore,
    history: Option<&dyn SessionTeamHistoryValidator>,
) -> Result<(), SessionTeamError> {
    if record.schema_version != SESSION_TEAM_SCHEMA_VERSION
        || record.configuration_revision != count + 1
        || record.expected_configuration_revision != count
    {
        return Err(SessionTeamError::Invalid(
            "team configuration revision is not its exact predecessor",
        ));
    }
    encoded(record, MAX_CONTRACT_ENVELOPE_BYTES)?;
    let imported = match &record.initial_source {
        Some(source) => {
            if count != 0 {
                return Err(SessionTeamError::Invalid(
                    "initial team source is permitted only before its first revision",
                ));
            }
            let graph =
                prior
                    .canonical_graph(&source.turn_id)?
                    .ok_or(SessionTeamError::Invalid(
                        "initial team source has no graph in the acknowledged canonical prefix",
                    ))?;
            if graph.snapshot_id != source.snapshot_id || graph.revision != source.graph_revision {
                return Err(SessionTeamError::Invalid(
                    "initial team source differs from its exact retained graph",
                ));
            }
            Some(graph)
        }
        None => None,
    };

    let empty = record
        .graph
        .slots
        .iter()
        .map(|slot| (slot.slot_id.clone(), ConversationSavepoint::Empty))
        .collect::<Vec<_>>();
    record.initial_graph(
        identity.owner(),
        GraphSnapshotId::new("team-configuration-validation")?,
        &empty,
    )?;
    let slots: HashMap<_, _> = record
        .graph
        .slots
        .iter()
        .map(|slot| (&slot.slot_id, slot))
        .collect();
    let decisions: HashMap<_, _> = record
        .continuity
        .iter()
        .map(|item| (&item.slot_id, &item.decision))
        .collect();
    if decisions.len() != record.continuity.len()
        || decisions.len() != slots.len()
        || decisions.keys().any(|slot| !slots.contains_key(slot))
    {
        return Err(SessionTeamError::Invalid(
            "every proposed slot requires one explicit continuity decision",
        ));
    }
    if record.layout.len() > MAX_CONTRACT_NODES {
        return Err(SessionTeamError::Capacity);
    }
    let mut positions = HashSet::new();
    for position in &record.layout {
        if !position.x.is_finite()
            || !position.y.is_finite()
            || !slots.contains_key(&position.slot_id)
            || !positions.insert(&position.slot_id)
        {
            return Err(SessionTeamError::Invalid(
                "layout requires unique declared slots and finite coordinates",
            ));
        }
    }
    for slot in &record.graph.slots {
        match &content.resolve_activation_evidence(&slot.definition.snapshot)? {
            ActivationEvidenceContent::Definition {
                definition_id,
                revision,
                profile,
                ..
            } if definition_id == &slot.definition.definition_id
                && *revision > 0
                && profile.definition == definition_id.as_str() => {}
            _ => {
                return Err(SessionTeamError::Invalid(
                    "slot definition differs from exact retained definition evidence",
                ))
            }
        }
        if !matches!(
            content.resolve_activation_evidence(&slot.budget)?,
            ActivationEvidenceContent::Budget { .. }
        ) {
            return Err(SessionTeamError::Invalid(
                "slot budget is missing or has the wrong evidence role",
            ));
        }
        if let Some(reference) = &slot.grant {
            let ActivationEvidenceContent::Grant { policy } =
                content.resolve_activation_evidence(reference)?
            else {
                return Err(SessionTeamError::Invalid(
                    "slot grant has the wrong retained evidence role",
                ));
            };
            let ActivationEvidenceContent::Definition { profile, .. } =
                &content.resolve_activation_evidence(&slot.definition.snapshot)?
            else {
                return Err(SessionTeamError::Invalid(
                    "slot grant has no retained definition profile",
                ));
            };
            let ActivationEvidenceContent::Budget { limits } =
                &content.resolve_activation_evidence(&slot.budget)?
            else {
                return Err(SessionTeamError::Invalid(
                    "slot grant has no retained budget",
                ));
            };
            if policy.holder != slot.node_id
                || !policy.profiles.contains(profile)
                || &policy.limits != limits
            {
                return Err(SessionTeamError::Invalid(
                    "slot grant differs from its exact holder, definition profile or budget",
                ));
            }
            content.resolve_activation_evidence(&policy.issuer_evidence)?;
        }
        let previous = last
            .and_then(|record| {
                record
                    .graph
                    .slots
                    .iter()
                    .find(|old| old.slot_id == slot.slot_id)
            })
            .map(|old| SessionTeamConversationSource {
                slot_id: old.slot_id.clone(),
                definition: old.definition.clone(),
                conversation_id: old.conversation_id.clone(),
            })
            .or_else(|| {
                imported
                    .as_ref()
                    .and_then(|graph| graph.nodes.iter().find(|node| node.slot_id == slot.slot_id))
                    .map(|node| SessionTeamConversationSource {
                        slot_id: node.slot_id.clone(),
                        definition: node.definition.clone(),
                        conversation_id: node.conversation_id.clone(),
                    })
            });
        match decisions[&slot.slot_id] {
            SessionTeamContinuity::Reset => {
                if prior.conversation_used(&slot.conversation_id)? {
                    return Err(SessionTeamError::Invalid(
                        "reset requires a conversation never used by this Session team",
                    ));
                }
            }
            SessionTeamContinuity::PreserveUnchanged => {
                let old = previous.ok_or(SessionTeamError::Invalid(
                    "only a current slot can preserve its conversation",
                ))?;
                if old.definition != slot.definition || old.conversation_id != slot.conversation_id
                {
                    return Err(SessionTeamError::Invalid(
                        "changed definition or conversation requires reset or verified projection",
                    ));
                }
            }
            SessionTeamContinuity::PreserveWithProjection { evidence } => {
                let old = previous.ok_or(SessionTeamError::Invalid(
                    "projection requires an exact current predecessor slot",
                ))?;
                if old.conversation_id != slot.conversation_id {
                    return Err(SessionTeamError::Invalid(
                        "preservation must retain the exact predecessor conversation",
                    ));
                }
                history
                    .ok_or_else(|| {
                        SessionTeamError::UnverifiedPreservation(
                            "no accepted-history projection validator is installed".into(),
                        )
                    })?
                    .validate_preservation(identity, &old, slot, evidence, content)
                    .map_err(SessionTeamError::UnverifiedPreservation)?;
            }
        }
    }
    for condition in &record.graph.conditions {
        match &condition.kind {
            ConditionKind::RepositoryCheck { definition } => {
                content.resolve_repository_check_definition(definition)?;
            }
            ConditionKind::Review { criterion } => {
                if !matches!(
                    content.resolve_activation_evidence(criterion)?,
                    ActivationEvidenceContent::Guidance { .. }
                ) {
                    return Err(SessionTeamError::Invalid(
                        "review criterion lacks retained instruction evidence",
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "session_team_tests.rs"]
mod tests;
