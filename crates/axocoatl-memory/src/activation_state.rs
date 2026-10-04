//! Immutable activation artifacts and selective conversation promotion.
//!
//! This isolated store does not change the existing Agent checkpoint store. Its
//! live host must provide an owned canonical namespace under the upgraded format
//! and Session writer. The isolated path opener remains for compatibility fixtures.
//! The daemon's native Session controller opens it through an owned namespace;
//! this module wires no actor, Tier 2–4 memory, or other writer itself.
//!
//! Inputs require opaque snapshots from successfully persisted canonical Session
//! history. An owned namespace binds journal and workspace at open. The isolated
//! compatibility opener binds them at its first durable snapshot.
//! Promotion selects only current accepted generations of a closed snapshot; byte
//! identity comes from this store, never versions or directory ordering. A live
//! snapshot may be stale: staging retains evidence, and does not permit dispatch.
//! Only immutable closed snapshots can authorize conversation promotion.
//! A durable promotion decision precedes per-conversation pointer updates. While
//! a write is uncertain no restore is permitted; reopen completes the recorded
//! decision before exposing any conversation. This is conversation state only,
//! not rollback or settlement of filesystem, tool, provider, or remote effects.
//!
//! Records live in an append-only segmented journal (see the `journal`
//! module), so a Session may record any number of activations, turns and
//! rewinds; only single records, objects and migrations are bounded.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use axocoatl_core::{MessageRole, SecureDir, SecureDirEntry, TokenUsageStats};
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use axocoatl_session::execution_store::{
    DurableLegacySeal, DurableSessionIdentity, DurableTurnSnapshot, SessionExecutionStore,
};
use axocoatl_session::turn_contract::{
    ActivationInputManifest, ActivationRef, ActivationState, CheckpointId, CheckpointRef,
    CheckpointSource, ClosedTurnRef, ConversationSavepoint, EpochState, EvidenceRef, LogicalTurnId,
    LogicalTurnState, NodeConversationId, SessionId, SessionTeamSlotId, TurnClosure,
    TurnGraphSnapshot, TurnNodeId,
};
use axocoatl_session::turn_ledger::{SessionTurn, SessionTurnLifecycle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkpoint::{decode_current, encode_current, AgentCheckpoint, MAX_CHECKPOINT_BYTES};
use crate::legacy_conversation::{
    bounded_history_checkpoint, ToolReplayPolicy, LEGACY_CONVERSATION_PROJECTION_VERSION,
};
use crate::StoredMessage;

#[path = "activation_state_legacy_roles.rs"]
mod legacy_roles;
pub use legacy_roles::{
    LegacyActorProjectionPolicy, LegacyCheckpointProjectionDetails, LegacyRoleAssignment,
};

#[path = "activation_state_rewind.rs"]
mod rewind;
pub use rewind::SessionRewind;

#[path = "activation_state_journal.rs"]
mod journal;
use journal::{Event, Projection, SealedSummary, StoreHead, LOG_SPEC, MAX_RECORD_BYTES};

const SCHEMA: u32 = 1;
const STATE_FILE: &str = "activation-state.json";
// Per-migration and per-record bounds; nothing bounds a Session's lifetime.
const MAX_BASELINES: usize = 128;
const MAX_BASELINE_TURNS: usize = 4096;
const LEGACY_PROJECTION_POLICY: &str = "plain-completed-single-agent-text-v1";
const ORDINARY_LEGACY_PROJECTION_POLICY: &str = "ordinary-autonomous-canonical-history-v2";

#[derive(Debug, thiserror::Error)]
pub enum ActivationStateError {
    #[error("activation state I/O: {0}")]
    Io(#[from] io::Error),
    #[error("activation state JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("legacy baseline canonical history: {0}")]
    Execution(#[from] axocoatl_session::execution_store::ExecutionStoreError),
    #[error("legacy baseline content: {0}")]
    Content(#[from] axocoatl_session::execution_content::ExecutionContentError),
    #[error("unsupported legacy baseline projection: {0}")]
    UnsupportedLegacy(&'static str),
    #[error("invalid activation state: {0}")]
    Invalid(String),
    /// A single record, checkpoint object or migration exceeds its bound.
    /// Nothing bounds how much a Session records over its life.
    #[error("activation state capacity exceeded")]
    Capacity,
    #[error("activation state write is uncertain; reopen before restoring or writing")]
    RecoveryRequired,
}

type Result<T> = std::result::Result<T, ActivationStateError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRecord {
    slot_id: SessionTeamSlotId,
    input: ActivationInputManifest,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    reference: CheckpointRef,
    input_sha256: String,
    payload_sha256: String,
    payload_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateReservation {
    activation: ActivationRef,
    input_sha256: String,
    max_checkpoint_bytes: usize,
}

/// Evidence that the exact activation has durable room for one candidate.
/// This does not authorize provider execution or acceptance. In particular,
/// recovery may recover this receipt after the turn has closed. No deserializer
/// or public constructor can invent a reservation. The store retains ownership;
/// using a receipt still requires an open, verified matching owned store.
pub struct ReservedActivationCheckpoint {
    identity: DurableSessionIdentity,
    conversation_id: NodeConversationId,
    record: CandidateReservation,
}

impl ReservedActivationCheckpoint {
    pub fn activation(&self) -> &ActivationRef {
        &self.record.activation
    }

    pub fn conversation_id(&self) -> &NodeConversationId {
        &self.conversation_id
    }

    pub fn max_checkpoint_bytes(&self) -> usize {
        self.record.max_checkpoint_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineRecord {
    slot_id: SessionTeamSlotId,
    reference: CheckpointRef,
    frontier: EvidenceRef,
    policy: String,
    original_agent_id: String,
    visible_turns: Vec<String>,
    payload_sha256: String,
    payload_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection: Option<LegacyProjectionDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint_projection: Option<LegacyCheckpointProjectionDetails>,
}

/// Explicit model-cache omissions. Every source row remains in sealed History.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum LegacyTurnOmission {
    Superseded { turn_id: String },
    Cancelled { turn_id: String },
    BoundedTail { turn_id: String },
}
impl LegacyTurnOmission {
    pub fn turn_id(&self) -> &str {
        match self {
            Self::Superseded { turn_id }
            | Self::Cancelled { turn_id }
            | Self::BoundedTail { turn_id } => turn_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyProjectionDetails {
    pub shared_policy_version: String,
    pub tool_replay_policy: ToolReplayPolicy,
    pub omitted_turns: Vec<LegacyTurnOmission>,
    /// Starts in visible, non-Cancelled source rows, before cache bounding.
    pub source_tool_starts: usize,
    /// Calls actually preserved in complete native groups in the cache.
    /// The difference remains Route-only because of replay policy, incomplete
    /// or malformed/bounded group evidence, or the bounded whole-turn tail.
    pub retained_tool_calls: usize,
    pub history_truncated: bool,
}

/// A bounded model-visible projection derived from actual sealed legacy history.
/// No deserializer or caller-provided checkpoint can manufacture this value.
/// It does not assert that unrecorded private actor state was recoverable.
pub struct LegacyBaselineProjection {
    identity: DurableSessionIdentity,
    record: BaselineRecord,
    payload: Vec<u8>,
    source_archive: Option<std::sync::Arc<crate::checkpoint::LegacySessionCheckpointSnapshot>>,
}

impl LegacyBaselineProjection {
    pub fn from_sealed_history(
        content: &ExecutionContentStore,
        seal: &DurableLegacySeal,
        slot_id: SessionTeamSlotId,
        conversation_id: NodeConversationId,
    ) -> Result<Self> {
        let frontier = content.read_legacy_history(seal)?;
        let (checkpoint, original_agent_id, visible_turns) =
            project_plain_legacy(&frontier.turns, &conversation_id)?;
        Self::encode_projection(
            seal,
            slot_id,
            checkpoint,
            original_agent_id,
            visible_turns,
            None,
        )
    }

    /// Preserves the established ordinary autonomous v1 restart-cache policy,
    /// including Failed/Interrupted text and complete provider-native groups.
    /// The host supplies the same token counter/replay policy as the old route.
    /// This cannot infer an unrecorded historical role or private actor state.
    pub fn from_ordinary_sealed_history(
        content: &ExecutionContentStore,
        seal: &DurableLegacySeal,
        slot_id: SessionTeamSlotId,
        conversation_id: NodeConversationId,
        policy: ToolReplayPolicy,
        count_text: &dyn Fn(&str) -> usize,
    ) -> Result<Self> {
        let frontier = content.read_legacy_history(seal)?;
        let (checkpoint, original_agent_id, visible_turns, projection) =
            project_ordinary_legacy(&frontier.turns, &conversation_id, policy, count_text)?;
        Self::encode_projection(
            seal,
            slot_id,
            checkpoint,
            original_agent_id,
            visible_turns,
            Some(projection),
        )
    }

    fn encode_projection(
        seal: &DurableLegacySeal,
        slot_id: SessionTeamSlotId,
        checkpoint: AgentCheckpoint,
        original_agent_id: String,
        visible_turns: Vec<String>,
        projection: Option<LegacyProjectionDetails>,
    ) -> Result<Self> {
        let conversation_id = NodeConversationId::new(checkpoint.agent_id.clone())
            .map_err(|error| invalid_error(error.to_string()))?;
        let payload =
            encode_current(&checkpoint).map_err(|error| invalid_error(error.to_string()))?;
        if payload.len() > MAX_CHECKPOINT_BYTES {
            return Err(ActivationStateError::Capacity);
        }
        let payload_sha256 = digest_bytes(&payload);
        let mut record = BaselineRecord {
            slot_id,
            reference: CheckpointRef {
                checkpoint_id: CheckpointId::new("unassigned")
                    .map_err(|error| invalid_error(error.to_string()))?,
                session_id: seal.identity().owner().session_id.clone(),
                conversation_id,
                source: CheckpointSource::Committed {
                    evidence: seal.reference().clone(),
                },
            },
            frontier: seal.reference().clone(),
            policy: if projection.is_some() {
                ORDINARY_LEGACY_PROJECTION_POLICY
            } else {
                LEGACY_PROJECTION_POLICY
            }
            .into(),
            original_agent_id,
            visible_turns,
            payload_sha256,
            payload_bytes: payload.len(),
            projection,
            checkpoint_projection: None,
        };
        record.reference = baseline_reference(&canonical_journal(seal.identity()), &record)?;
        Ok(Self {
            identity: seal.identity().clone(),
            record,
            payload,
            source_archive: None,
        })
    }

    pub fn reference(&self) -> &CheckpointRef {
        &self.record.reference
    }
    pub fn policy(&self) -> &str {
        &self.record.policy
    }
    pub fn payload_sha256(&self) -> &str {
        &self.record.payload_sha256
    }
    pub fn payload_bytes(&self) -> usize {
        self.record.payload_bytes
    }
    pub fn frontier(&self) -> &EvidenceRef {
        &self.record.frontier
    }
    pub fn checkpoint_projection_details(&self) -> Option<&LegacyCheckpointProjectionDetails> {
        self.record.checkpoint_projection.as_ref()
    }
    pub fn projection_details(&self) -> Option<&LegacyProjectionDetails> {
        self.record.projection.as_ref()
    }
}

/// One exact current accepted generation selected for final conversation state.
/// This record is evidence; deserializing it does not authorize promotion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotedConversation {
    pub slot_id: SessionTeamSlotId,
    pub node_id: TurnNodeId,
    pub accepted: CheckpointRef,
    pub committed: CheckpointRef,
    pub previous_committed: Option<CheckpointRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionManifest {
    pub journal_id: String,
    pub workspace_id: String,
    pub promotion_id: String,
    pub closure: ClosedTurnRef,
    pub contract_sha256: String,
    pub selected: Vec<PromotedConversation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalJournal {
    journal_id: String,
    workspace_id: String,
}

enum StoreDirectory {
    Isolated(SecureDir),
    Owned(OwnedExecutionNamespace),
}

impl StoreDirectory {
    fn identity(&self) -> Option<&DurableSessionIdentity> {
        match self {
            Self::Isolated(_) => None,
            Self::Owned(dir) => Some(dir.identity()),
        }
    }
    fn check_journal_creation(&self, primary: impl AsRef<Path>) -> io::Result<()> {
        match self {
            Self::Isolated(_) => Ok(()),
            Self::Owned(dir) => dir.check_journal_creation(primary),
        }
    }
    fn mark_journal_initialized(&self, primary: impl AsRef<Path>) -> io::Result<()> {
        match self {
            Self::Isolated(_) => Ok(()),
            Self::Owned(dir) => dir.mark_journal_initialized(primary),
        }
    }
    fn child(&self, name: impl AsRef<Path>) -> io::Result<Self> {
        match self {
            Self::Isolated(dir) => dir.child(name).map(Self::Isolated),
            Self::Owned(dir) => dir.child(name).map(Self::Owned),
        }
    }
    fn read_limited(&self, name: impl AsRef<Path>, max: usize) -> io::Result<Vec<u8>> {
        match self {
            Self::Isolated(dir) => dir.read_limited(name, max),
            Self::Owned(dir) => dir.read_limited(name, max),
        }
    }
    fn atomic_write(&self, name: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Isolated(dir) => dir.atomic_write(name, bytes),
            Self::Owned(dir) => dir.atomic_write(name, bytes),
        }
    }
    fn entries_limited(&self, max: usize) -> io::Result<Vec<SecureDirEntry>> {
        match self {
            Self::Isolated(dir) => dir.entries_limited(max),
            Self::Owned(dir) => dir.entries_limited(max),
        }
    }
    fn is_file(&self, name: impl AsRef<Path>) -> io::Result<bool> {
        match self {
            Self::Isolated(dir) => dir.is_file(name),
            Self::Owned(dir) => dir.is_file(name),
        }
    }
    fn sync_all(&self) -> io::Result<()> {
        match self {
            Self::Isolated(dir) => dir.sync_all(),
            Self::Owned(dir) => dir.sync_all(),
        }
    }
    fn verify_ambient_identity(&self) -> io::Result<()> {
        match self {
            Self::Isolated(dir) => dir.verify_ambient_identity(),
            Self::Owned(dir) => dir.verify_ambient_identity(),
        }
    }
}

/// One exclusively owned activation-artifact namespace for a Session.
/// No mutable conversation cache is keyed by an Agent definition/template.
pub struct ActivationStateStore {
    root: StoreDirectory,
    objects: StoreDirectory,
    heads: StoreDirectory,
    head: StoreHead,
    log: axocoatl_session::segment_log::SegmentLog,
    /// Records of the active segment, the only ones held in memory.
    active: Vec<std::sync::Arc<Event>>,
    /// One key filter per sealed segment, in segment order.
    sealed: Vec<SealedSummary>,
    cache: axocoatl_session::segment_log::SegmentCache<Event>,
    projection: Projection,
    uncertain: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    recovery: axocoatl_session::segment_log::SegmentRecovery,
}

impl ActivationStateStore {
    /// Isolated compatibility/fixture opener. Live host code must use open_owned
    /// so the format and Session writer leases remain held with every artifact.
    /// The caller must durably provision this private root outside every checkout.
    pub fn open(path: impl AsRef<Path>, session_id: SessionId) -> Result<Self> {
        Self::open_isolated(path, session_id, LOG_SPEC)
    }

    fn open_isolated(
        path: impl AsRef<Path>,
        session_id: SessionId,
        spec: axocoatl_session::segment_log::SegmentSpec,
    ) -> Result<Self> {
        let root = SecureDir::open_existing_all(path)?;
        #[cfg(unix)]
        {
            root.require_owner_and_private_writes(effective_uid())?;
            root.lock_exclusive_waiting(axocoatl_core::LOCK_INHERITANCE_GRACE)?;
        }
        #[cfg(not(unix))]
        return Err(io::Error::new(io::ErrorKind::Unsupported, "Unix ownership required").into());
        Self::open_directory(StoreDirectory::Isolated(root), session_id, None, spec)
    }

    pub fn open_owned(namespace: OwnedExecutionNamespace) -> Result<Self> {
        Self::open_owned_with(namespace, LOG_SPEC)
    }

    fn open_owned_with(
        namespace: OwnedExecutionNamespace,
        spec: axocoatl_session::segment_log::SegmentSpec,
    ) -> Result<Self> {
        namespace.require_root(&ExecutionComponent::ActivationState)?;
        let identity = namespace.identity().clone();
        let journal = CanonicalJournal {
            journal_id: identity.journal_id().to_owned(),
            workspace_id: identity.owner().workspace_id.clone(),
        };
        Self::open_directory(
            StoreDirectory::Owned(namespace),
            identity.owner().session_id.clone(),
            Some(journal),
            spec,
        )
    }

    fn session_id(&self) -> &SessionId {
        &self.head.session_id
    }

    /// The bound canonical journal, which every populated store has.
    fn bound_journal(&self) -> Result<CanonicalJournal> {
        self.head.journal.clone().ok_or_else(|| {
            invalid_error("populated artifact namespace has no canonical journal binding")
        })
    }

    /// Install an exact immutable legacy baseline before any canonical v2 turn.
    /// The projection is minted only from retained, canonically sealed history.
    /// It creates a committed baseline directly, never an accepted v2 activation.
    pub fn import_legacy_baseline(
        &mut self,
        canonical: &SessionExecutionStore,
        projection: &LegacyBaselineProjection,
    ) -> Result<CheckpointRef> {
        self.ready()?;
        if self.root.identity() != Some(&projection.identity)
            || canonical.identity()? != projection.identity
            || canonical
                .legacy_seal()?
                .as_ref()
                .map(|seal| seal.reference())
                != Some(&projection.record.frontier)
        {
            return invalid("legacy baseline belongs to another owned canonical namespace");
        }
        if !canonical.records()?.is_empty() || self.projection.v2 {
            return invalid("legacy baseline must precede every v2 input and promotion");
        }
        let record = &projection.record;
        if projection.payload.len() != record.payload_bytes
            || digest_bytes(&projection.payload) != record.payload_sha256
        {
            return invalid("legacy projection payload changed");
        }
        if let Some(existing) = self.projection.baselines.iter().find(|existing| {
            existing.slot_id == record.slot_id
                || existing.reference.conversation_id == record.reference.conversation_id
        }) {
            if existing != record {
                return invalid("legacy baseline is immutable");
            }
            self.load_baseline(existing)?;
            return Ok(existing.reference.clone());
        }
        let event = Event::Baseline(record.clone());
        let admitted = self.admit(&event, &self.bound_journal()?)?;
        let name = object_name(&record.reference);
        if self.objects.is_file(&name)?
            && self.objects.read_limited(&name, MAX_CHECKPOINT_BYTES)? != projection.payload
        {
            return invalid("immutable baseline artifact already contains different bytes");
        }
        self.uncertain = true;
        self.retain_legacy_checkpoint_archive(projection)?;
        self.objects.atomic_write(&name, &projection.payload)?;
        self.append(event, admitted)?;
        Ok(record.reference.clone())
    }

    /// Persist the exact immutable starting inputs of a materialized activation.
    /// Repeating identical input is inert; identity/content collisions fail.
    pub fn record_input(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<()> {
        self.ready()?;
        let journal = self.check_snapshot(snapshot)?;
        let contract = snapshot.contract();
        let item = contract
            .activations()
            .iter()
            .find(|item| item.activation == *activation)
            .ok_or_else(|| invalid_error("activation is absent from validated contract"))?;
        if item.activation.session_id != *self.session_id() {
            return invalid("input belongs to another Session");
        }
        let slot_id = contract
            .graph()
            .and_then(|graph| {
                graph
                    .nodes
                    .iter()
                    .find(|node| node.node_id == activation.node_id)
            })
            .ok_or_else(|| invalid_error("activation has no canonical graph slot"))?
            .slot_id
            .clone();
        let record = InputRecord {
            sha256: digest(&(&slot_id, &item.input))?,
            slot_id,
            input: item.input.clone(),
        };
        if let Some(existing) = self.input(activation)? {
            return if existing == record {
                Ok(())
            } else {
                invalid("immutable input changed")
            };
        }
        // Resolve exact starting/parent artifacts before acknowledging execution
        // inputs. Accepted status is established by the validated contract.
        if let ConversationSavepoint::Checkpoint { checkpoint } = &record.input.starting_savepoint {
            self.load_reference(checkpoint)?;
        }
        for parent in &record.input.parents {
            self.load_reference(&parent.checkpoint)?;
        }
        let event = Event::Input(record);
        let admitted = self.admit(&event, &journal)?;
        self.bind_journal(journal)?;
        self.append(event, admitted)
    }

    /// Reserve one bounded candidate before provider work. The current canonical
    /// store, rather than a previously captured snapshot, proves that this exact
    /// activation and epoch are still running. Controller dispatch authority and
    /// physical input/profile checks remain separate mandatory host checks.
    /// This reserves the candidate's single settlement, not physical
    /// filesystem free space.
    pub fn reserve_candidate(
        &mut self,
        canonical: &SessionExecutionStore,
        activation: &ActivationRef,
    ) -> Result<ReservedActivationCheckpoint> {
        self.ready()?;
        let identity = self.require_owned_canonical(canonical)?;
        let snapshot = canonical.snapshot(&activation.turn_id)?;
        self.check_snapshot(&snapshot)?;
        let contract = snapshot.contract();
        let item = contract
            .activations()
            .iter()
            .find(|item| item.activation == *activation)
            .filter(|item| item.state == ActivationState::Running)
            .ok_or_else(|| {
                invalid_error("candidate reservation requires a current running activation")
            })?;
        if contract.state() != Some(LogicalTurnState::Running)
            || contract.epochs().last().is_none_or(|epoch| {
                epoch.id != activation.execution_epoch_id || epoch.state != EpochState::Running
            })
        {
            return invalid("candidate reservation requires the current running epoch");
        }
        let input = self
            .input(activation)?
            .filter(|input| input.input == item.input)
            .ok_or_else(|| invalid_error("candidate reservation requires exact durable input"))?;
        let record = CandidateReservation {
            activation: activation.clone(),
            input_sha256: input.sha256.clone(),
            max_checkpoint_bytes: MAX_CHECKPOINT_BYTES,
        };
        let conversation_id = input.input.conversation_id.clone();
        if let Some(existing) = self.reservation(activation)? {
            if existing != record {
                return invalid("candidate reservation differs from durable input");
            }
        } else {
            if !self.candidates_for(activation)?.is_empty() {
                return invalid("cannot reserve an activation after its candidate was retained");
            }
            let event = Event::CandidateReserved(record.clone());
            let admitted = self.admit(&event, &canonical_journal(&identity))?;
            self.append(event, admitted)?;
        }
        Ok(ReservedActivationCheckpoint {
            identity,
            conversation_id,
            record,
        })
    }

    /// Recover an existing reservation for evidence-only settlement. A closed or
    /// cancelled activation can retain its observed diagnostic state; this receipt
    /// cannot reopen execution or authorize acceptance/promotion.
    pub fn candidate_reservation(
        &self,
        canonical: &SessionExecutionStore,
        activation: &ActivationRef,
    ) -> Result<ReservedActivationCheckpoint> {
        self.ready()?;
        let identity = self.require_owned_canonical(canonical)?;
        let snapshot = canonical.snapshot(&activation.turn_id)?;
        self.check_snapshot(&snapshot)?;
        let item = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == *activation)
            .ok_or_else(|| invalid_error("candidate reservation has no canonical activation"))?;
        let input = self
            .input(activation)?
            .filter(|input| input.input == item.input)
            .ok_or_else(|| {
                invalid_error("candidate reservation input differs from canonical history")
            })?;
        let record = self
            .reservation(activation)?
            .filter(|record| record.input_sha256 == input.sha256)
            .ok_or_else(|| invalid_error("activation has no durable candidate reservation"))?;
        Ok(ReservedActivationCheckpoint {
            identity,
            conversation_id: input.input.conversation_id.clone(),
            record,
        })
    }

    /// Resolve the exact immutable starting conversation through its reservation.
    /// Its cumulative usage is historical checkpoint evidence only. Before actor
    /// restore, the controller must replace accounting with its independently
    /// validated incurred-usage aggregate, including failed/superseded calls.
    pub fn starting_checkpoint_for(
        &self,
        reservation: &ReservedActivationCheckpoint,
    ) -> Result<Option<AgentCheckpoint>> {
        self.require_reservation(reservation)?;
        self.starting_checkpoint(reservation.activation())
    }

    /// Retain the single reserved candidate, even after Stop/closure. Identical
    /// retries verify existing bytes; conflicting output cannot reuse the slot.
    /// The reference alone confers no accepted-conversation authority.
    pub fn stage_reserved_candidate(
        &mut self,
        reservation: &ReservedActivationCheckpoint,
        checkpoint: &AgentCheckpoint,
    ) -> Result<CheckpointRef> {
        self.require_reservation(reservation)?;
        let input = self
            .input(reservation.activation())?
            .ok_or_else(|| invalid_error("reserved activation has no input"))?;
        if checkpoint.agent_id != reservation.conversation_id.as_str() {
            return invalid("checkpoint must own the exact reserved conversation");
        }
        self.stage_checkpoint(
            &canonical_journal(&reservation.identity),
            &input,
            checkpoint,
        )
    }

    /// Isolated compatibility staging. Owned stores require the explicit
    /// reservation API, which fixes each activation's single settlement
    /// before any provider work.
    pub fn stage_candidate(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
        checkpoint: &AgentCheckpoint,
    ) -> Result<CheckpointRef> {
        self.ready()?;
        if self.root.identity().is_some() {
            return invalid("owned candidate staging requires an opaque reservation");
        }
        let journal = self.check_snapshot(snapshot)?;
        let item = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == *activation)
            .filter(|item| item.state == ActivationState::Running)
            .ok_or_else(|| invalid_error("candidate producer is not a running activation"))?;
        let input = self
            .input(activation)?
            .ok_or_else(|| invalid_error("starting input was not persisted"))?;
        if input.input != item.input || checkpoint.agent_id != input.input.conversation_id.as_str()
        {
            return invalid("checkpoint must own the exact node conversation and input");
        }
        self.stage_checkpoint(&journal, &input, checkpoint)
    }

    fn stage_checkpoint(
        &mut self,
        journal: &CanonicalJournal,
        input: &InputRecord,
        checkpoint: &AgentCheckpoint,
    ) -> Result<CheckpointRef> {
        let activation = &input.input.activation;
        if crate::encoded_checkpoint_size(checkpoint)
            .map_err(|error| invalid_error(error.to_string()))?
            > MAX_CHECKPOINT_BYTES
        {
            return Err(ActivationStateError::Capacity);
        }
        let payload =
            encode_current(checkpoint).map_err(|error| invalid_error(error.to_string()))?;
        if payload.len() > MAX_CHECKPOINT_BYTES {
            return Err(ActivationStateError::Capacity);
        }
        let payload_sha256 = digest_bytes(&payload);
        let key = digest(&(
            &journal,
            activation,
            &input.input.conversation_id,
            &input.sha256,
            &payload_sha256,
            payload.len(),
        ))?;
        let reference = CheckpointRef {
            checkpoint_id: CheckpointId::new(format!("checkpoint:{key}"))
                .map_err(|e| invalid_error(e.to_string()))?,
            session_id: self.session_id().clone(),
            conversation_id: input.input.conversation_id.clone(),
            source: CheckpointSource::Accepted {
                activation: activation.clone(),
            },
        };
        let candidate = Candidate {
            reference: reference.clone(),
            input_sha256: input.sha256.clone(),
            payload_sha256,
            payload_bytes: payload.len(),
        };
        if let Some(existing) = self.candidate(&reference)? {
            if existing != candidate {
                return invalid("checkpoint identity collision");
            }
            self.load_candidate(&existing)?;
            return Ok(reference);
        }
        if self.reservation(activation)?.is_some() && !self.candidates_for(activation)?.is_empty() {
            return invalid("reserved activation already retained a different candidate");
        }
        let event = Event::Candidate(candidate);
        let admitted = self.admit(&event, journal)?;
        let name = object_name(&reference);
        if self.objects.is_file(&name)?
            && self.objects.read_limited(&name, MAX_CHECKPOINT_BYTES)? != payload
        {
            return invalid("immutable checkpoint file already contains different bytes");
        }
        self.uncertain = true;
        self.objects.atomic_write(&name, &payload)?;
        self.append(event, admitted)?;
        Ok(reference)
    }

    /// Validate the exact retained canonical owner without reacquiring a lock.
    pub fn verify_canonical_owner(&self, canonical: &SessionExecutionStore) -> Result<()> {
        self.ready()?;
        self.require_owned_canonical(canonical).map(|_| ())
    }

    fn require_owned_canonical(
        &self,
        canonical: &SessionExecutionStore,
    ) -> Result<DurableSessionIdentity> {
        let identity = canonical.identity()?;
        if self.root.identity() != Some(&identity) {
            return invalid("candidate reservation requires the exact owned canonical namespace");
        }
        Ok(identity)
    }

    fn require_reservation(&self, reservation: &ReservedActivationCheckpoint) -> Result<()> {
        self.ready()?;
        if self.root.identity() != Some(&reservation.identity)
            || self.reservation(reservation.activation())?.as_ref() != Some(&reservation.record)
            || self.input(reservation.activation())?.is_none_or(|input| {
                input.sha256 != reservation.record.input_sha256
                    || input.input.conversation_id != reservation.conversation_id
            })
        {
            return invalid("candidate reservation belongs to another namespace or input");
        }
        Ok(())
    }

    /// Restore the recorded starting point, never an activation's newest file.
    pub fn starting_checkpoint(
        &self,
        activation: &ActivationRef,
    ) -> Result<Option<AgentCheckpoint>> {
        self.ready()?;
        let input = self
            .input(activation)?
            .ok_or_else(|| invalid_error("unknown activation input"))?;
        match &input.input.starting_savepoint {
            ConversationSavepoint::Empty => Ok(None),
            ConversationSavepoint::Checkpoint { checkpoint } => {
                self.load_reference(checkpoint).map(Some)
            }
        }
    }

    /// Read an exact retained artifact, checking ownership and byte digest.
    /// This is not proof that the artifact is current or accepted for dispatch.
    pub fn checkpoint(&self, reference: &CheckpointRef) -> Result<AgentCheckpoint> {
        self.ready()?;
        self.load_reference(reference)
    }

    pub fn committed_reference(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<Option<CheckpointRef>> {
        self.ready()?;
        self.verify_heads()?;
        Ok(self.projection.effective(conversation))
    }

    pub fn committed_checkpoint(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<Option<AgentCheckpoint>> {
        self.committed_reference(conversation)?
            .as_ref()
            .map(|reference| self.load_reference(reference))
            .transpose()
    }

    /// Validate a future/recovered graph against exact current conversation
    /// state. This is preflight, not graph admission or permission to execute.
    pub fn validate_starting_savepoints(&self, graph: &TurnGraphSnapshot) -> Result<()> {
        self.ready()?;
        for node in &graph.nodes {
            // A reviewed future Team Reset creates a fresh conversation
            // for this slot. Historical conversation ownership never moves
            // to another slot; selection authority belongs to Team admission.
            if self
                .projection
                .conversations
                .get(&node.conversation_id)
                .and_then(|conversation| conversation.slot_id.as_ref())
                .is_some_and(|slot| slot != &node.slot_id)
            {
                return invalid("graph assigns a retained conversation to another team slot");
            }
            let current = self.committed_reference(&node.conversation_id)?;
            let expected = current
                .clone()
                .map_or(ConversationSavepoint::Empty, |checkpoint| {
                    ConversationSavepoint::Checkpoint {
                        checkpoint: Box::new(checkpoint),
                    }
                });
            if node.starting_savepoint != expected {
                return invalid("graph savepoint differs from its committed conversation baseline");
            }
            if let Some(reference) = current {
                self.load_reference(&reference)?;
            }
        }
        Ok(())
    }

    /// Read only the immutable imported legacy baseline. Later promoted heads
    /// cannot replace this once-only accounting/conversation migration source.
    pub fn legacy_baseline_checkpoint(
        &self,
        conversation: &NodeConversationId,
    ) -> Result<Option<AgentCheckpoint>> {
        self.ready()?;
        self.projection
            .baseline(conversation)
            .map(|baseline| self.load_baseline(baseline))
            .transpose()
    }

    /// Read a matching acknowledged promotion without moving current heads.
    pub fn promotion(&self, snapshot: &DurableTurnSnapshot) -> Result<Option<PromotionManifest>> {
        self.ready()?;
        self.check_snapshot(snapshot)?;
        let closure = snapshot
            .contract()
            .closed_reference()
            .map_err(|e| invalid_error(e.to_string()))?;
        let contract_sha256 = digest(snapshot.contract())?;
        match self.promotion_of_turn(closure.turn_id())? {
            Some(existing)
                if existing.closure == closure && existing.contract_sha256 == contract_sha256 =>
            {
                Ok(Some(existing))
            }
            Some(_) => invalid("closed turn already has a different promotion decision"),
            None => Ok(None),
        }
    }

    /// Reserve even an empty turn's final promotion before canonical closure.
    /// Also verify every selected candidate and committed base before closing.
    /// The receipt is still a canonical snapshot, not caller-authored acceptance.
    pub fn prepare_close(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        closure: TurnClosure,
    ) -> Result<()> {
        self.ready()?;
        let journal = self.check_snapshot(snapshot)?;
        if snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
        {
            let actual = snapshot
                .contract()
                .closed_reference()
                .map_err(|e| invalid_error(e.to_string()))?;
            if actual.closure() != closure {
                return invalid("requested closure differs from canonical closure");
            }
            if self.promotion(snapshot)?.is_some() {
                return Ok(());
            }
        }
        self.selected_for_promotion(snapshot)?;
        if self.turn_reserved(snapshot.turn_id())? {
            // Input admission already reserves this turn's final promotion.
            return Ok(());
        }
        let event = Event::PromotionReserved {
            turn_id: snapshot.turn_id().clone(),
        };
        let admitted = self.admit(&event, &journal)?;
        self.bind_journal(journal)?;
        self.append(event, admitted)
    }

    /// Inspect the exact would-be promotion without changing conversation state.
    pub fn preview_promotion(&self, snapshot: &DurableTurnSnapshot) -> Result<PromotionManifest> {
        self.ready()?;
        if let Some(existing) = self.promotion(snapshot)? {
            return Ok(existing);
        }
        let journal = self.check_snapshot(snapshot)?;
        let contract = snapshot.contract();
        let closure = contract
            .closed_reference()
            .map_err(|e| invalid_error(e.to_string()))?;
        let contract_sha256 = digest(contract)?;
        let mut selected = self.selected_for_promotion(snapshot)?;
        let promotion_id = promotion_id(&journal, &closure, &contract_sha256, &selected)?;
        for entry in &mut selected {
            entry.committed = committed_ref(&promotion_id, &entry.accepted)?;
        }
        Ok(PromotionManifest {
            journal_id: journal.journal_id,
            workspace_id: journal.workspace_id,
            promotion_id,
            closure,
            contract_sha256,
            selected,
        })
    }

    fn selected_for_promotion(
        &self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<Vec<PromotedConversation>> {
        self.verify_heads()?;
        let contract = snapshot.contract();
        let mut selected = vec![];
        for item in contract.current_accepted_activations() {
            if !contract.selected_for_finalization(&item.activation) {
                continue;
            }
            let accepted = item
                .checkpoint
                .as_ref()
                .ok_or_else(|| invalid_error("accepted activation lacks checkpoint"))?;
            let input = self
                .input(&item.activation)?
                .ok_or_else(|| invalid_error("accepted input is not persisted"))?;
            if input.input != item.input {
                return invalid("accepted input differs from persisted input");
            }
            let candidate = self
                .candidate(accepted)?
                .ok_or_else(|| invalid_error("accepted checkpoint is not retained"))?;
            self.load_candidate(&candidate)?;
            let previous = self.committed_reference(&item.conversation_id)?;
            if let Some(reference) = &previous {
                self.load_reference(reference)?;
            }
            if self.committed_base(&item.activation)? != previous {
                return invalid("conversation advanced since activation's starting savepoint");
            }
            selected.push(PromotedConversation {
                slot_id: input.slot_id.clone(),
                node_id: item.activation.node_id.clone(),
                accepted: accepted.clone(),
                committed: accepted.clone(),
                previous_committed: previous,
            });
        }
        selected.sort_by(|a, b| {
            a.accepted
                .conversation_id
                .as_str()
                .cmp(b.accepted.conversation_id.as_str())
        });
        Ok(selected)
    }

    /// Select only exact current accepted generations of an immutable closed turn.
    /// Whole-turn cancellation does not invalidate previously accepted generations.
    /// Failed, interrupted and superseded generations remain unselected; their
    /// prior committed conversation heads and diagnostic candidates are preserved.
    pub fn promote(&mut self, snapshot: &DurableTurnSnapshot) -> Result<PromotionManifest> {
        self.ready()?;
        if let Some(existing) = self.promotion(snapshot)? {
            return Ok(existing);
        }
        let journal = self.check_snapshot(snapshot)?;
        let manifest = self.preview_promotion(snapshot)?;
        let event = Event::PromotionPrepared(manifest.clone());
        let admitted = self.admit(&event, &journal)?;
        self.bind_journal(journal)?;
        self.append(event, admitted)?;
        self.finish_pending()?;
        Ok(manifest)
    }

    fn check_snapshot(&self, snapshot: &DurableTurnSnapshot) -> Result<CanonicalJournal> {
        let journal = CanonicalJournal {
            journal_id: snapshot.journal_id().to_owned(),
            workspace_id: snapshot.owner().workspace_id.clone(),
        };
        if snapshot.owner().session_id != *self.session_id()
            || self
                .head
                .journal
                .as_ref()
                .is_some_and(|owner| owner != &journal)
        {
            return invalid("snapshot belongs to another canonical journal or workspace");
        }
        Ok(journal)
    }

    fn committed_base(&self, activation: &ActivationRef) -> Result<Option<CheckpointRef>> {
        let mut at = activation.clone();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(at.activation_id.clone()) {
                return invalid("cyclic starting savepoint");
            }
            let record = self
                .input(&at)?
                .ok_or_else(|| invalid_error("starting input is missing"))?;
            match record.input.starting_savepoint {
                ConversationSavepoint::Empty => return Ok(None),
                ConversationSavepoint::Checkpoint { checkpoint } => match checkpoint.source {
                    CheckpointSource::Committed { .. } => return Ok(Some(*checkpoint)),
                    CheckpointSource::Accepted {
                        activation: previous,
                    } => {
                        at = previous;
                    }
                },
            }
        }
    }

    fn finish_pending(&mut self) -> Result<()> {
        let Some(manifest) = self.projection.pending.clone() else {
            return Ok(());
        };
        journal::check_promotion(&self.bound_journal()?, self.session_id(), &manifest)?;
        for entry in &manifest.selected {
            self.load_reference(&entry.accepted)?;
        }
        self.uncertain = true;
        for entry in &manifest.selected {
            self.heads.atomic_write(
                head_name(&entry.committed.conversation_id),
                &serde_json::to_vec(entry)?,
            )?;
        }
        let event = Event::PromotionFinished {
            promotion_id: manifest.promotion_id,
        };
        let admitted = self.admit(&event, &self.bound_journal()?)?;
        self.append(event, admitted)
    }

    fn load_reference(&self, reference: &CheckpointRef) -> Result<AgentCheckpoint> {
        if reference.session_id != *self.session_id() {
            return invalid("checkpoint belongs to another Session");
        }
        match &reference.source {
            CheckpointSource::Accepted { .. } => {
                let candidate = self
                    .candidate(reference)?
                    .ok_or_else(|| invalid_error("unknown checkpoint or changed ownership"))?;
                self.load_candidate(&candidate)
            }
            CheckpointSource::Committed { .. } => {
                if let Some(promotion) = journal::committed_promotion(reference) {
                    let accepted = self
                        .promotion_by_id(promotion)?
                        .and_then(|manifest| {
                            manifest
                                .selected
                                .into_iter()
                                .find(|entry| entry.committed == *reference)
                        })
                        .ok_or_else(|| {
                            invalid_error("unknown committed checkpoint or changed ownership")
                        })?;
                    return self.load_reference(&accepted.accepted);
                }
                if let Some(baseline) = self
                    .projection
                    .baselines
                    .iter()
                    .find(|baseline| baseline.reference == *reference)
                {
                    return self.load_baseline(baseline);
                }
                let projection = self
                    .rewinds()?
                    .into_iter()
                    .flat_map(|rewind| rewind.conversations)
                    .find(|entry| entry.checkpoint.as_ref() == Some(reference))
                    .and_then(|entry| entry.projection)
                    .ok_or_else(|| {
                        invalid_error("unknown committed checkpoint or changed ownership")
                    })?;
                self.load_payload(
                    reference,
                    &projection.payload_sha256,
                    projection.payload_bytes,
                )
            }
        }
    }

    fn load_baseline(&self, baseline: &BaselineRecord) -> Result<AgentCheckpoint> {
        self.verify_legacy_checkpoint_archive(baseline)?;
        self.load_payload(
            &baseline.reference,
            &baseline.payload_sha256,
            baseline.payload_bytes,
        )
    }

    fn load_candidate(&self, candidate: &Candidate) -> Result<AgentCheckpoint> {
        self.load_payload(
            &candidate.reference,
            &candidate.payload_sha256,
            candidate.payload_bytes,
        )
    }

    fn load_payload(
        &self,
        reference: &CheckpointRef,
        payload_sha256: &str,
        payload_bytes: usize,
    ) -> Result<AgentCheckpoint> {
        let bytes = self
            .objects
            .read_limited(object_name(reference), MAX_CHECKPOINT_BYTES)?;
        if bytes.len() != payload_bytes || digest_bytes(&bytes) != payload_sha256 {
            return invalid("checkpoint artifact digest mismatch");
        }
        // The reused decoder expects its caller to check the current envelope
        // magic. Activation artifacts accept no markerless/legacy fallback.
        if !bytes.starts_with(b"AXOCKPT\0") {
            return invalid("checkpoint envelope magic mismatch");
        }
        let checkpoint = decode_current(&bytes).map_err(invalid_error)?;
        if checkpoint.agent_id != reference.conversation_id.as_str() {
            return invalid("checkpoint payload belongs to another node conversation");
        }
        Ok(checkpoint)
    }

    fn ready(&self) -> Result<()> {
        if self.uncertain || self.projection.pending.is_some() {
            return Err(ActivationStateError::RecoveryRequired);
        }
        self.root.verify_ambient_identity()?;
        Ok(())
    }

    fn verify_heads(&self) -> Result<()> {
        for baseline in self.projection.baselines.iter() {
            self.verify_legacy_checkpoint_archive(baseline)?;
            if self
                .projection
                .conversations
                .get(&baseline.reference.conversation_id)
                .is_none_or(|conversation| conversation.head.is_none())
            {
                self.load_baseline(baseline)?;
            }
        }
        for head in self
            .projection
            .conversations
            .values()
            .filter_map(|conversation| conversation.head.as_ref())
        {
            let bytes = self
                .heads
                .read_limited(head_name(&head.committed.conversation_id), MAX_RECORD_BYTES)?;
            if serde_json::from_slice::<PromotedConversation>(&bytes)? != *head {
                return invalid(
                    "materialized conversation pointer differs from canonical promotion",
                );
            }
        }
        Ok(())
    }
}

fn promotion_id(
    journal: &CanonicalJournal,
    closure: &ClosedTurnRef,
    contract_sha256: &str,
    selected: &[PromotedConversation],
) -> Result<String> {
    digest(&(
        journal,
        closure,
        contract_sha256,
        selected
            .iter()
            .map(|item| {
                (
                    &item.slot_id,
                    &item.node_id,
                    &item.accepted,
                    &item.previous_committed,
                )
            })
            .collect::<Vec<_>>(),
    ))
}

fn committed_ref(promotion_id: &str, accepted: &CheckpointRef) -> Result<CheckpointRef> {
    let key = digest(&(promotion_id, accepted))?;
    Ok(CheckpointRef {
        checkpoint_id: CheckpointId::new(format!("committed:{key}"))
            .map_err(|e| invalid_error(e.to_string()))?,
        session_id: accepted.session_id.clone(),
        conversation_id: accepted.conversation_id.clone(),
        source: CheckpointSource::Committed {
            evidence: EvidenceRef::new(format!("promotion:{promotion_id}"))
                .map_err(|e| invalid_error(e.to_string()))?,
        },
    })
}

fn canonical_journal(identity: &DurableSessionIdentity) -> CanonicalJournal {
    CanonicalJournal {
        journal_id: identity.journal_id().to_owned(),
        workspace_id: identity.owner().workspace_id.clone(),
    }
}

fn baseline_reference(
    journal: &CanonicalJournal,
    baseline: &BaselineRecord,
) -> Result<CheckpointRef> {
    let content_key = digest(&(
        journal,
        &baseline.reference.session_id,
        &baseline.slot_id,
        &baseline.reference.conversation_id,
        &baseline.frontier,
        &baseline.policy,
        &baseline.original_agent_id,
        &baseline.visible_turns,
        &baseline.payload_sha256,
        baseline.payload_bytes,
    ))?;
    // Preserve identities of the original strict policy. The richer policy
    // additionally binds its explicit bounded-cache/omission report.
    let key = match &baseline.projection {
        Some(projection) => digest(&(content_key, projection))?,
        None => content_key,
    };
    let key = match &baseline.checkpoint_projection {
        Some(projection) => digest(&(key, projection))?,
        None => key,
    };
    Ok(CheckpointRef {
        checkpoint_id: CheckpointId::new(format!("baseline:{key}"))
            .map_err(|error| invalid_error(error.to_string()))?,
        session_id: baseline.reference.session_id.clone(),
        conversation_id: baseline.reference.conversation_id.clone(),
        source: CheckpointSource::Committed {
            evidence: EvidenceRef::new(format!("baseline:{key}"))
                .map_err(|error| invalid_error(error.to_string()))?,
        },
    })
}

fn valid_baseline_policy(record: &BaselineRecord) -> bool {
    if record.checkpoint_projection.is_some() {
        return legacy_roles::valid_checkpoint_baseline_policy(record);
    }
    match (record.policy.as_str(), &record.projection) {
        (LEGACY_PROJECTION_POLICY, None) => !record.visible_turns.is_empty(),
        (ORDINARY_LEGACY_PROJECTION_POLICY, Some(details)) => {
            let mut ids: HashSet<&str> = record.visible_turns.iter().map(String::as_str).collect();
            details.shared_policy_version == LEGACY_CONVERSATION_PROJECTION_VERSION
                && details.omitted_turns.len().saturating_add(ids.len()) <= MAX_BASELINE_TURNS
                && details.retained_tool_calls <= details.source_tool_starts
                && (details.tool_replay_policy != ToolReplayPolicy::OmitNativeGroups
                    || details.retained_tool_calls == 0)
                && details.history_truncated
                    == details
                        .omitted_turns
                        .iter()
                        .any(|item| matches!(item, LegacyTurnOmission::BoundedTail { .. }))
                && details
                    .omitted_turns
                    .iter()
                    .all(|item| !item.turn_id().is_empty() && ids.insert(item.turn_id()))
                && !ids.is_empty()
        }
        _ => false,
    }
}

fn project_ordinary_legacy(
    turns: &[SessionTurn],
    conversation: &NodeConversationId,
    policy: ToolReplayPolicy,
    count_text: &dyn Fn(&str) -> usize,
) -> Result<(
    AgentCheckpoint,
    String,
    Vec<String>,
    LegacyProjectionDetails,
)> {
    if turns.len() > MAX_BASELINE_TURNS {
        return Err(ActivationStateError::Capacity);
    }
    let mut agent: Option<String> = None;
    let mut usage = TokenUsageStats::default();
    let mut known = true;
    for turn in turns {
        validate_ordinary_legacy_turn(turn, &mut agent)?;
        let (incurred, complete) = legacy_usage(turn)?;
        usage.input_tokens = usage
            .input_tokens
            .checked_add(incurred.input_tokens)
            .ok_or(ActivationStateError::Capacity)?;
        usage.output_tokens = usage
            .output_tokens
            .checked_add(incurred.output_tokens)
            .ok_or(ActivationStateError::Capacity)?;
        if let Some(reasoning) = incurred.reasoning_tokens {
            usage.reasoning_tokens = Some(
                usage
                    .reasoning_tokens
                    .unwrap_or(0)
                    .checked_add(reasoning)
                    .ok_or(ActivationStateError::Capacity)?,
            );
        }
        known &= complete;
    }
    let agent = agent.ok_or(ActivationStateError::UnsupportedLegacy(
        "no recorded ordinary agent identity",
    ))?;
    let visible: Vec<_> = turns
        .iter()
        .filter(|turn| !turn.superseded)
        .cloned()
        .collect();
    let bounded = bounded_history_checkpoint(
        count_text,
        &visible,
        1,
        conversation.as_str().to_owned(),
        turns
            .iter()
            .map(|turn| turn.updated_at / 1_000)
            .max()
            .unwrap_or(0),
        usage,
        known,
        None,
        policy,
    )
    .map_err(|error| invalid_error(error.to_string()))?;
    let retained: HashSet<_> = bounded
        .retained_turn_ids
        .iter()
        .map(String::as_str)
        .collect();
    let omitted_turns = turns
        .iter()
        .filter_map(|turn| {
            if turn.superseded {
                Some(LegacyTurnOmission::Superseded {
                    turn_id: turn.id.clone(),
                })
            } else if turn.status == SessionTurnLifecycle::Cancelled {
                Some(LegacyTurnOmission::Cancelled {
                    turn_id: turn.id.clone(),
                })
            } else if !retained.contains(turn.id.as_str()) {
                Some(LegacyTurnOmission::BoundedTail {
                    turn_id: turn.id.clone(),
                })
            } else {
                None
            }
        })
        .collect();
    let details = LegacyProjectionDetails {
        shared_policy_version: LEGACY_CONVERSATION_PROJECTION_VERSION.into(),
        tool_replay_policy: policy,
        omitted_turns,
        source_tool_starts: visible
            .iter()
            .filter(|turn| turn.status != SessionTurnLifecycle::Cancelled)
            .flat_map(|turn| &turn.execution_events)
            .filter(|record| record.event.kind == "tool_started")
            .count(),
        retained_tool_calls: bounded
            .checkpoint
            .session_messages
            .iter()
            .map(|message| message.tool_calls.len())
            .sum(),
        history_truncated: bounded.history_truncated,
    };
    Ok((
        bounded.checkpoint,
        agent,
        bounded.retained_turn_ids,
        details,
    ))
}

fn validate_ordinary_legacy_turn(turn: &SessionTurn, agent: &mut Option<String>) -> Result<()> {
    let unsupported = |message| ActivationStateError::UnsupportedLegacy(message);
    if !turn.status.is_terminal() {
        return Err(unsupported("ordinary history still contains running work"));
    }
    let recorded = turn
        .agent_id
        .as_ref()
        .filter(|agent| !agent.is_empty() && agent.len() <= 256)
        .ok_or_else(|| unsupported("recorded ordinary agent identity is unavailable"))?;
    if agent.as_ref().is_some_and(|agent| agent != recorded) {
        return Err(unsupported(
            "mixed or coordinated agent history requires an exact per-identity checkpoint bridge",
        ));
    }
    *agent = Some(recorded.clone());
    let checkpoint_import = turn
        .metadata
        .get("source")
        .and_then(serde_json::Value::as_str)
        == Some("actor_checkpoint")
        && turn
            .metadata
            .get("checkpoint_version")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && matches!(
            turn.metadata
                .get("checkpoint_encoding")
                .and_then(serde_json::Value::as_str),
            Some("bincode_v0.1.0" | "bincode_v0.1.1-v0.1.4" | "postcard_unframed_launch_candidate")
        );
    for (key, value) in &turn.metadata {
        let allowed = match key.as_str() {
            "mode" => value.as_str() == Some("single_agent"),
            "target_agent" => value.as_str() == Some(recorded),
            "model" => value.as_str().is_some() && value.as_str() == turn.model.as_deref(),
            "input_tokens" | "output_tokens" | "reasoning_tokens" | "total_tokens" => {
                value.as_u64().is_some()
            }
            "token_usage_known" => value.as_bool().is_some(),
            // The existing v1 importer commits this exact provenance triplet
            // before replacing a historical checkpoint cache. It is retained
            // canonical evidence, not private actor orchestration state.
            "source" | "checkpoint_version" | "checkpoint_encoding" => checkpoint_import,
            _ => false,
        };
        if !allowed {
            return Err(unsupported(
                "unsupported mode or private/unknown turn metadata",
            ));
        }
    }
    for context in &turn.context {
        let (required, allowed): (&str, &[&str]) =
            match context.kind.as_str() {
                "code_selection" => (
                    "content",
                    &["path", "start_line", "end_line", "language", "content"],
                ),
                "browser_selection" => ("html", &["url", "selector", "html"]),
                _ => return Err(unsupported(
                    "context requires an exact attachment/blob or specialized projection policy",
                )),
            };
        if !context
            .metadata
            .get(required)
            .is_some_and(serde_json::Value::is_string)
            || context
                .metadata
                .keys()
                .any(|key| !allowed.contains(&key.as_str()))
            || context.metadata.iter().any(|(key, value)| {
                if matches!(key.as_str(), "start_line" | "end_line") {
                    value.as_u64().is_none()
                } else {
                    !value.is_string()
                }
            })
        {
            return Err(unsupported(
                "inline context is incomplete or has unknown/private metadata",
            ));
        }
    }
    for record in &turn.execution_events {
        let event = &record.event;
        if event.attempt_id.is_some() {
            return Err(unsupported(
                "attempt or coordinated execution requires a per-identity bridge",
            ));
        }
        match event.kind.as_str() {
            "run_started"
                if event.execution_id.as_deref() == Some(turn.id.as_str())
                    && event.metadata.is_empty() => {}
            "tool_started" | "tool_result" => {
                const KEYS: &[&str] = &[
                    "agent_id",
                    "tool_name",
                    "tool_name_truncated",
                    "call_id",
                    "call_id_truncated",
                    "call_id_sha256",
                    "occurrence",
                    "arguments",
                    "arguments_truncated",
                    "provider_arguments",
                    "provider_arguments_truncated",
                    "provider_metadata",
                    "provider_metadata_truncated",
                    "provider_response_group",
                    "provider_call_index",
                    "provider_call_count",
                    "assistant_content",
                    "assistant_content_truncated",
                    "result",
                    "result_truncated",
                    "is_error",
                ];
                if event
                    .metadata
                    .get("agent_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(recorded)
                    || event
                        .metadata
                        .keys()
                        .any(|key| !KEYS.contains(&key.as_str()))
                {
                    return Err(unsupported(
                        "tool evidence has a foreign agent or unknown/private attribution",
                    ));
                }
            }
            _ => {
                return Err(unsupported(
                    "coordinated or unknown execution records require another projection policy",
                ))
            }
        }
    }
    for output in &turn.agent_outputs {
        if output.agent_id != *recorded
            || output.attempt_id.is_some()
            || output.activation_generation.is_some()
            || output.superseded
        {
            return Err(unsupported(
                "output has coordinated or foreign activation attribution",
            ));
        }
    }
    Ok(())
}

fn project_plain_legacy(
    turns: &[SessionTurn],
    conversation: &NodeConversationId,
) -> Result<(AgentCheckpoint, String, Vec<String>)> {
    if turns.len() > MAX_BASELINE_TURNS {
        return Err(ActivationStateError::Capacity);
    }
    let mut messages = vec![];
    let mut visible_turns = vec![];
    let mut agent: Option<String> = None;
    let mut usage = TokenUsageStats::default();
    let mut usage_known = true;
    let mut checkpoint_time = 0;
    let mut projected_bytes = 0usize;
    for turn in turns {
        let (turn_usage, known) = legacy_usage(turn)?;
        usage.input_tokens = usage
            .input_tokens
            .checked_add(turn_usage.input_tokens)
            .ok_or(ActivationStateError::Capacity)?;
        usage.output_tokens = usage
            .output_tokens
            .checked_add(turn_usage.output_tokens)
            .ok_or(ActivationStateError::Capacity)?;
        if let Some(reasoning) = turn_usage.reasoning_tokens {
            usage.reasoning_tokens = Some(
                usage
                    .reasoning_tokens
                    .unwrap_or(0)
                    .checked_add(reasoning)
                    .ok_or(ActivationStateError::Capacity)?,
            );
        }
        usage_known &= known;
        // Rewind removes these rows from future conversation, but their exact
        // history and all available usage remain retained by the sealed frontier.
        if turn.superseded {
            continue;
        }
        if turn.status != SessionTurnLifecycle::Completed
            || turn.error.is_some()
            || turn.completed_at.is_none()
            || turn.final_output.is_none()
        {
            return Err(ActivationStateError::UnsupportedLegacy(
                "visible history is not completed text",
            ));
        }
        if !turn.context.is_empty() {
            return Err(ActivationStateError::UnsupportedLegacy(
                "attached or inline context requires a richer exact projection policy",
            ));
        }
        let recorded_agent = turn
            .agent_id
            .as_ref()
            .filter(|agent| !agent.is_empty() && agent.len() <= 256)
            .ok_or(ActivationStateError::UnsupportedLegacy(
                "recorded single-agent identity is unavailable",
            ))?;
        if agent.as_ref().is_some_and(|agent| agent != recorded_agent) {
            return Err(ActivationStateError::UnsupportedLegacy(
                "history contains multiple agent identities",
            ));
        }
        agent = Some(recorded_agent.clone());
        for (key, value) in &turn.metadata {
            let allowed = match key.as_str() {
                "mode" => value.as_str() == Some("single_agent"),
                "target_agent" => value.as_str() == Some(recorded_agent.as_str()),
                "model" => value.as_str().is_some() && value.as_str() == turn.model.as_deref(),
                "input_tokens" | "output_tokens" | "reasoning_tokens" | "total_tokens" => {
                    value.as_u64().is_some()
                }
                "token_usage_known" => value.as_bool().is_some(),
                _ => false,
            };
            if !allowed {
                return Err(ActivationStateError::UnsupportedLegacy(
                    "unsupported mode or private/unknown turn metadata",
                ));
            }
        }
        if turn.execution_events.len() > 1
            || turn.execution_events.iter().any(|record| {
                record.event.kind != "run_started"
                    || record.event.execution_id.as_deref() != Some(turn.id.as_str())
                    || record.event.attempt_id.is_some()
                    || !record.event.metadata.is_empty()
            })
        {
            return Err(ActivationStateError::UnsupportedLegacy(
                "tool, coordinated, or unknown execution events require another projection policy",
            ));
        }
        let answer = turn
            .final_output
            .as_ref()
            .ok_or(ActivationStateError::UnsupportedLegacy(
                "completed history has no final text",
            ))?;
        let answered_at = if turn.agent_outputs.is_empty() {
            turn.updated_at
        } else {
            if turn.agent_outputs.len() != 1 {
                return Err(ActivationStateError::UnsupportedLegacy(
                    "history has multiple attributed outputs",
                ));
            }
            let output = &turn.agent_outputs[0];
            if output.agent_id != *recorded_agent
                || output.output != *answer
                || output.attempt_id.is_some()
                || output.activation_generation.is_some()
                || output.superseded
            {
                return Err(ActivationStateError::UnsupportedLegacy(
                    "attributed output is not an exact ordinary single-agent answer",
                ));
            }
            output.recorded_at
        };
        projected_bytes = projected_bytes
            .checked_add(turn.user_input.len())
            .and_then(|bytes| bytes.checked_add(answer.len()))
            .ok_or(ActivationStateError::Capacity)?;
        if projected_bytes > MAX_CHECKPOINT_BYTES {
            return Err(ActivationStateError::Capacity);
        }
        for (role, text, timestamp) in [
            (MessageRole::User, &turn.user_input, turn.created_at),
            (MessageRole::Assistant, answer, answered_at),
        ] {
            messages.push(StoredMessage {
                content_parts: None,
                role,
                content: text.clone(),
                timestamp: timestamp / 1_000,
                // Conservative byte-based context budget, not provider usage.
                token_count: text.len(),
                name: None,
                tool_calls: vec![],
                tool_call_id: None,
            });
        }
        visible_turns.push(turn.id.clone());
        checkpoint_time = checkpoint_time.max(turn.updated_at / 1_000);
    }
    let agent = agent.ok_or(ActivationStateError::UnsupportedLegacy(
        "no visible completed conversation; no empty baseline is fabricated",
    ))?;
    Ok((
        AgentCheckpoint {
            version: 1,
            agent_id: conversation.as_str().to_owned(),
            checkpoint_time,
            session_messages: messages,
            cumulative_token_usage: usage,
            cumulative_token_usage_known: usage_known,
            behavior_state: None,
        },
        agent,
        visible_turns,
    ))
}

fn legacy_usage(turn: &SessionTurn) -> Result<(TokenUsageStats, bool)> {
    let number = |name: &str| -> Result<Option<usize>> {
        turn.metadata
            .get(name)
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(ActivationStateError::UnsupportedLegacy(
                        "invalid or overflowing legacy usage",
                    ))
            })
            .transpose()
    };
    let input = number("input_tokens")?;
    let output = number("output_tokens")?;
    let reasoning = number("reasoning_tokens")?;
    if let Some(total) = number("total_tokens")? {
        let subtotal = input
            .unwrap_or(0)
            .checked_add(output.unwrap_or(0))
            .and_then(|n| n.checked_add(reasoning.unwrap_or(0)))
            .ok_or(ActivationStateError::Capacity)?;
        if total != subtotal {
            return Err(ActivationStateError::UnsupportedLegacy(
                "legacy usage subtotal differs from its recorded total",
            ));
        }
    }
    let known = match turn.metadata.get("token_usage_known") {
        Some(value) => value
            .as_bool()
            .ok_or(ActivationStateError::UnsupportedLegacy(
                "invalid legacy usage completeness",
            ))?,
        None => false,
    };
    Ok((
        TokenUsageStats {
            input_tokens: input.unwrap_or(0),
            output_tokens: output.unwrap_or(0),
            reasoning_tokens: reasoning,
        },
        known && input.is_some() && output.is_some(),
    ))
}

fn digest(value: &impl Serialize) -> Result<String> {
    Ok(digest_bytes(&serde_json::to_vec(value)?))
}
fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn object_name(reference: &CheckpointRef) -> String {
    format!(
        "{}.checkpoint",
        digest_bytes(reference.checkpoint_id.as_str().as_bytes())
    )
}
fn head_name(conversation: &NodeConversationId) -> String {
    format!("{}.json", digest_bytes(conversation.as_str().as_bytes()))
}
fn invalid_error(message: impl Into<String>) -> ActivationStateError {
    ActivationStateError::Invalid(message.into())
}
fn invalid<T>(message: &str) -> Result<T> {
    Err(invalid_error(message))
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no arguments and has no failure sentinel.
    unsafe { geteuid() }
}

/// The bytes of a store's head and every journal segment, in order.
#[cfg(all(test, unix))]
fn journal_bytes(root: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(root.join(STATE_FILE)).unwrap();
    bytes.extend(std::fs::read(root.join(LOG_SPEC.active_name())).unwrap());
    let mut sealed: Vec<_> = std::fs::read_dir(root.join("segments"))
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().path())
        .collect();
    sealed.sort();
    for path in sealed {
        bytes.extend(std::fs::read(path).unwrap());
    }
    bytes
}

/// Make the next journal append fail by putting a directory where the
/// active segment is; returns the moved segment.
#[cfg(all(test, unix))]
fn block_active_segment(root: &Path) -> std::path::PathBuf {
    let active = root.join(LOG_SPEC.active_name());
    let saved = root.join("saved-active-segment");
    std::fs::rename(&active, &saved).unwrap();
    std::fs::create_dir(&active).unwrap();
    saved
}

#[cfg(all(test, unix))]
fn unblock_active_segment(root: &Path, saved: std::path::PathBuf) {
    let active = root.join(LOG_SPEC.active_name());
    std::fs::remove_dir(&active).unwrap();
    std::fs::rename(saved, active).unwrap();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use axocoatl_session::turn_contract::{CommandId, TurnContractEnvelope, TurnContractEvent};
    use std::fs;
    use std::sync::Arc;

    #[test]
    fn isolated_store_binds_its_journal_with_the_first_input_and_refuses_oversized_checkpoints() {
        let history_root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(history_root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let envelopes: Vec<TurnContractEnvelope> = serde_json::from_str(include_str!(
            "../tests/fixtures/activation_state/running_history.json"
        ))
        .unwrap();
        let mut history = SessionExecutionStore::open(
            ownership,
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: envelopes[0].session_id.clone(),
            },
        )
        .unwrap();
        for envelope in &envelopes {
            history.append(envelope.clone()).unwrap();
        }
        let turn_id = envelopes[0].turn_id.clone();
        let snapshot = history.snapshot(&turn_id).unwrap();
        let activation = snapshot.contract().activations()[0].activation.clone();
        let root = tempfile::tempdir().unwrap();
        let mut store =
            ActivationStateStore::open(root.path(), activation.session_id.clone()).unwrap();
        let head = |root: &Path| -> serde_json::Value {
            serde_json::from_slice(&fs::read(root.join(STATE_FILE)).unwrap()).unwrap()
        };
        assert!(head(root.path())["journal"].is_null());
        assert_eq!(head(root.path())["segments"]["kind"], "activation-state");
        store.record_input(&snapshot, &activation).unwrap();
        assert_eq!(
            head(root.path())["journal"]["journal_id"],
            snapshot.journal_id()
        );
        let mut checkpoint = AgentCheckpoint {
            version: 1,
            agent_id: "conversation-a".into(),
            checkpoint_time: 1,
            session_messages: vec![],
            cumulative_token_usage: axocoatl_core::TokenUsageStats::new(0, 0),
            cumulative_token_usage_known: true,
            behavior_state: None,
        };
        let accepted = store
            .stage_candidate(&snapshot, &activation, &checkpoint)
            .unwrap();
        let before = journal_bytes(root.path());
        checkpoint.version = 900;
        checkpoint.behavior_state = Some("x".repeat(MAX_CHECKPOINT_BYTES));
        assert!(matches!(
            store.stage_candidate(&snapshot, &activation, &checkpoint),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(journal_bytes(root.path()), before);
        assert_eq!(
            fs::read_dir(root.path().join("objects")).unwrap().count(),
            1
        );
        for event in [
            TurnContractEvent::AcceptActivation {
                activation: activation.clone(),
                checkpoint: Box::new(accepted),
                output: EvidenceRef::new("output").unwrap(),
            },
            TurnContractEvent::Close {
                closure: TurnClosure::Completed,
            },
        ] {
            let revision = history.turn(&turn_id).unwrap().unwrap().revision();
            history
                .append(TurnContractEnvelope {
                    schema_version: axocoatl_session::turn_contract::TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new(format!("command-{revision}")).unwrap(),
                    expected_revision: revision,
                    session_id: activation.session_id.clone(),
                    turn_id: turn_id.clone(),
                    event,
                })
                .unwrap();
        }
        store.promote(&history.snapshot(&turn_id).unwrap()).unwrap();
        assert!(store.projection.pending.is_none());
        drop(store);
        let reopened = ActivationStateStore::open(root.path(), activation.session_id).unwrap();
        assert_eq!(
            reopened
                .committed_checkpoint(&NodeConversationId::new("conversation-a").unwrap())
                .unwrap()
                .unwrap()
                .version,
            1
        );
    }
}

#[cfg(all(test, unix))]
mod reservation_tests {
    use super::*;
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::execution_store::ExecutionStoreOwner;
    use axocoatl_session::turn_contract::{
        ActivationId, CommandId, InputManifestId, TurnContractEnvelope, TurnContractEvent,
    };
    use std::fs;
    use std::sync::Arc;

    fn fixture() -> (
        tempfile::TempDir,
        SessionExecutionStore,
        ActivationStateStore,
        Vec<ActivationRef>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let mut envelopes: Vec<TurnContractEnvelope> = serde_json::from_str(include_str!(
            "../tests/fixtures/activation_state/running_history.json"
        ))
        .unwrap();
        if let TurnContractEvent::Begin { graph, .. } = &mut envelopes[0].event {
            let mut node = graph.nodes[0].clone();
            node.node_id = TurnNodeId::new("node-b").unwrap();
            node.slot_id = SessionTeamSlotId::new("slot-b").unwrap();
            node.conversation_id = NodeConversationId::new("conversation-b").unwrap();
            graph.nodes.push(node);
        }
        let mut additional = envelopes[1].clone();
        additional.command_id = CommandId::new("command-2").unwrap();
        additional.expected_revision = 2;
        if let TurnContractEvent::StartActivation { input } = &mut additional.event {
            input.manifest_id = InputManifestId::new("input-b").unwrap();
            input.activation.node_id = TurnNodeId::new("node-b").unwrap();
            input.activation.activation_id = ActivationId::new("activation-b").unwrap();
            input.conversation_id = NodeConversationId::new("conversation-b").unwrap();
        }
        envelopes.push(additional);
        let mut canonical = SessionExecutionStore::open(
            ownership,
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: envelopes[0].session_id.clone(),
            },
        )
        .unwrap();
        for envelope in &envelopes {
            canonical.append(envelope.clone()).unwrap();
        }
        let snapshot = canonical.snapshot(&envelopes[0].turn_id).unwrap();
        let activations: Vec<_> = snapshot
            .contract()
            .activations()
            .iter()
            .map(|item| item.activation.clone())
            .collect();
        let mut memory = ActivationStateStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .unwrap(),
        )
        .unwrap();
        memory.record_input(&snapshot, &activations[0]).unwrap();
        (root, canonical, memory, activations)
    }
    fn empty_checkpoint(conversation: &str) -> AgentCheckpoint {
        AgentCheckpoint {
            version: 1,
            agent_id: conversation.into(),
            checkpoint_time: 1,
            session_messages: vec![],
            cumulative_token_usage: TokenUsageStats::new(5, 3),
            cumulative_token_usage_known: true,
            behavior_state: None,
        }
    }
    fn memory_root(canonical: &SessionExecutionStore) -> std::path::PathBuf {
        canonical.path().parent().unwrap().join("activation-state")
    }

    #[test]
    fn every_activation_reserves_and_settles_exactly_once() {
        let (_root, canonical, mut memory, activations) = fixture();
        let first = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        memory
            .record_input(
                &canonical.snapshot(&activations[1].turn_id).unwrap(),
                &activations[1],
            )
            .unwrap();
        let second = memory
            .reserve_candidate(&canonical, &activations[1])
            .unwrap();
        let a = memory
            .stage_reserved_candidate(&first, &empty_checkpoint("conversation-a"))
            .unwrap();
        let b = memory
            .stage_reserved_candidate(&second, &empty_checkpoint("conversation-b"))
            .unwrap();
        assert_ne!(a, b);
        let mut different = empty_checkpoint("conversation-a");
        different.version = 2;
        assert!(memory.stage_reserved_candidate(&first, &different).is_err());
        assert_eq!(memory.candidates_for(&activations[0]).unwrap().len(), 1);
        assert_eq!(
            memory.checkpoint(&a).unwrap().cumulative_token_usage,
            TokenUsageStats::new(5, 3)
        );
    }

    #[test]
    fn reopen_keeps_an_unsettled_reservation_and_its_exact_settlement() {
        let (_root, canonical, mut memory, activations) = fixture();
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        memory
            .record_input(
                &canonical.snapshot(&activations[1].turn_id).unwrap(),
                &activations[1],
            )
            .unwrap();
        let candidate = memory
            .stage_reserved_candidate(&reservation, &empty_checkpoint("conversation-a"))
            .unwrap();
        drop(memory);
        let mut memory = ActivationStateStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .unwrap(),
        )
        .unwrap();
        let recovered = memory
            .candidate_reservation(&canonical, &activations[0])
            .unwrap();
        assert_eq!(
            memory
                .stage_reserved_candidate(&recovered, &empty_checkpoint("conversation-a"))
                .unwrap(),
            candidate
        );
        assert!(memory
            .candidate_reservation(&canonical, &activations[1])
            .is_err());
    }

    #[test]
    fn reserved_checkpoint_byte_cap_is_checked_before_any_object_publication() {
        let (_root, canonical, mut memory, activations) = fixture();
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        let before = journal_bytes(&memory_root(&canonical));
        let mut checkpoint = empty_checkpoint("conversation-a");
        checkpoint.behavior_state = Some("x".repeat(reservation.max_checkpoint_bytes()));
        assert!(matches!(
            memory.stage_reserved_candidate(&reservation, &checkpoint),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(journal_bytes(&memory_root(&canonical)), before);
        assert!(memory.objects.entries_limited(1).unwrap().is_empty());
        assert!(memory.candidates_for(&activations[0]).unwrap().is_empty());
        assert!(memory.reservation(&activations[0]).unwrap().is_some());
    }

    #[test]
    fn interrupted_candidate_publication_keeps_reservation_and_reuses_exact_orphan_after_reopen() {
        let (_root, canonical, mut memory, activations) = fixture();
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        let root = memory_root(&canonical);
        let saved = block_active_segment(&root);
        assert!(memory
            .stage_reserved_candidate(&reservation, &empty_checkpoint("conversation-a"))
            .is_err());
        assert!(matches!(
            memory.starting_checkpoint_for(&reservation),
            Err(ActivationStateError::RecoveryRequired)
        ));
        let files: Vec<_> = fs::read_dir(root.join("objects")).unwrap().collect();
        assert_eq!(files.len(), 1);
        let orphan = fs::read(files[0].as_ref().unwrap().path()).unwrap();
        drop(memory);
        unblock_active_segment(&root, saved);
        let mut memory = ActivationStateStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .unwrap(),
        )
        .unwrap();
        let recovered = memory
            .candidate_reservation(&canonical, &activations[0])
            .unwrap();
        let retained = memory
            .stage_reserved_candidate(&recovered, &empty_checkpoint("conversation-a"))
            .unwrap();
        assert_eq!(
            memory
                .objects
                .read_limited(object_name(&retained), MAX_CHECKPOINT_BYTES)
                .unwrap(),
            orphan
        );
        assert_eq!(memory.checkpoint(&retained).unwrap().version, 1);
    }
}

#[cfg(all(test, unix))]
mod empty_promotion_tests {
    use super::*;
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::execution_store::ExecutionStoreOwner;
    use axocoatl_session::turn_contract::{CommandId, TurnContractEnvelope, TurnContractEvent};
    use std::sync::Arc;

    #[test]
    fn empty_turn_reserves_its_promotion_before_close_and_promotes_an_empty_selection() {
        let history_root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(history_root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let envelopes: Vec<TurnContractEnvelope> = serde_json::from_str(include_str!(
            "../tests/fixtures/activation_state/running_history.json"
        ))
        .unwrap();
        let begin = envelopes[0].clone();
        let mut canonical = SessionExecutionStore::open(
            ownership,
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: begin.session_id.clone(),
            },
        )
        .unwrap();
        canonical.append(begin.clone()).unwrap();
        let snapshot = canonical.snapshot(&begin.turn_id).unwrap();
        assert!(snapshot.contract().activations().is_empty());
        let root = tempfile::tempdir().unwrap();
        let mut memory = ActivationStateStore::open(root.path(), begin.session_id.clone()).unwrap();
        assert!(!memory.turn_reserved(&begin.turn_id).unwrap());
        let saved = block_active_segment(root.path());
        assert!(memory
            .prepare_close(&snapshot, TurnClosure::Cancelled)
            .is_err());
        assert!(matches!(
            memory.prepare_close(&snapshot, TurnClosure::Cancelled),
            Err(ActivationStateError::RecoveryRequired)
        ));
        drop(memory);
        unblock_active_segment(root.path(), saved);
        let mut memory = ActivationStateStore::open(root.path(), begin.session_id.clone()).unwrap();
        assert!(!memory.turn_reserved(&begin.turn_id).unwrap());
        memory
            .prepare_close(&snapshot, TurnClosure::Cancelled)
            .unwrap();
        assert!(memory.turn_reserved(&begin.turn_id).unwrap());
        let before = journal_bytes(root.path());
        memory
            .prepare_close(&snapshot, TurnClosure::Cancelled)
            .unwrap();
        assert_eq!(journal_bytes(root.path()), before, "a repeat is inert");
        drop(memory);
        let mut memory = ActivationStateStore::open(root.path(), begin.session_id.clone()).unwrap();
        canonical
            .append(TurnContractEnvelope {
                schema_version: begin.schema_version,
                command_id: CommandId::new("empty-close").unwrap(),
                expected_revision: 1,
                session_id: begin.session_id.clone(),
                turn_id: begin.turn_id.clone(),
                event: TurnContractEvent::Close {
                    closure: TurnClosure::Cancelled,
                },
            })
            .unwrap();
        let closed = canonical.snapshot(&begin.turn_id).unwrap();
        let promoted = memory.promote(&closed).unwrap();
        assert!(promoted.selected.is_empty());
        assert_eq!(memory.promotion(&closed).unwrap(), Some(promoted.clone()));
        drop(memory);
        let reopened = ActivationStateStore::open(root.path(), begin.session_id).unwrap();
        assert_eq!(reopened.promotion(&closed).unwrap(), Some(promoted));
    }
}
