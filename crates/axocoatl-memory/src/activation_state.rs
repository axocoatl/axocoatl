//! Immutable activation artifacts and selective conversation promotion.
//!
//! This isolated store does not change the existing Agent checkpoint store. Its
//! live host must provide an owned canonical namespace under the upgraded format
//! and Session writer. The isolated path opener remains for compatibility fixtures.
//! No daemon, actor, Tier 2–4 memory, or live writer is wired here.
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

const SCHEMA: u32 = 1;
const STATE_FILE: &str = "activation-state.json";
const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUTS: usize = 4096;
const MAX_CANDIDATES: usize = 8192;
const MAX_PROMOTIONS: usize = 4096;
const MAX_BASELINES: usize = 128;
const MAX_BASELINE_TURNS: usize = 4096;
const LEGACY_PROJECTION_POLICY: &str = "plain-completed-single-agent-text-v1";
const ORDINARY_LEGACY_PROJECTION_POLICY: &str = "ordinary-autonomous-canonical-history-v2";
// Schema-1 identities are at most 128 ASCII bytes. A selected entry has fewer
// than 32 such fields across accepted, committed, previous, node, and slot refs.
// 16 KiB covers both its manifest and materialized-head representations plus
// JSON framing. 4 KiB covers closure, journal/workspace identity, and digests.
// This deliberately reserves for every unpromoted turn/conversation, including
// failed generations, until an explicit empty/partial promotion releases it.
const PROMOTION_CONVERSATION_RESERVE: usize = 16 * 1024;
const PROMOTION_TURN_RESERVE: usize = 4 * 1024;
// A candidate contains a bounded activation/ref plus three digests and lengths.
// This covers its complete serialized record with maximal schema-1 identifiers.
const CANDIDATE_METADATA_RESERVE: usize = 4 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    state_bytes: usize,
    promotions: usize,
    candidates: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            state_bytes: MAX_STATE_BYTES,
            promotions: MAX_PROMOTIONS,
            candidates: MAX_CANDIDATES,
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreState {
    schema_version: u32,
    session_id: SessionId,
    journal: Option<CanonicalJournal>,
    #[serde(default)]
    baselines: Vec<BaselineRecord>,
    inputs: Vec<InputRecord>,
    candidates: Vec<Candidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    candidate_reservations: Vec<CandidateReservation>,
    heads: Vec<PromotedConversation>,
    promotions: Vec<PromotionManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rewinds: Vec<SessionRewind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    promotion_reservations: Vec<LogicalTurnId>,
    pending: Option<PromotionManifest>,
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

/// One exclusively owned, bounded activation-artifact namespace for a Session.
/// No mutable conversation cache is keyed by an Agent definition/template.
pub struct ActivationStateStore {
    root: StoreDirectory,
    objects: StoreDirectory,
    heads: StoreDirectory,
    state: StoreState,
    uncertain: bool,
    limits: Limits,
}

impl ActivationStateStore {
    /// Isolated compatibility/fixture opener. Live host code must use open_owned
    /// so the format and Session writer leases remain held with every artifact.
    /// The caller must durably provision this private root outside every checkout.
    pub fn open(path: impl AsRef<Path>, session_id: SessionId) -> Result<Self> {
        let root = SecureDir::open_existing_all(path)?;
        #[cfg(unix)]
        {
            root.require_owner_and_private_writes(effective_uid())?;
            root.try_lock_exclusive()?;
        }
        #[cfg(not(unix))]
        return Err(io::Error::new(io::ErrorKind::Unsupported, "Unix ownership required").into());
        Self::open_directory(StoreDirectory::Isolated(root), session_id, None)
    }

    pub fn open_owned(namespace: OwnedExecutionNamespace) -> Result<Self> {
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
        )
    }

    fn open_directory(
        root: StoreDirectory,
        session_id: SessionId,
        journal: Option<CanonicalJournal>,
    ) -> Result<Self> {
        root.verify_ambient_identity()?;
        let loaded = match root.read_limited(STATE_FILE, MAX_STATE_BYTES) {
            Ok(bytes) => Some(serde_json::from_slice::<StoreState>(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let state = if let Some(state) = loaded {
            if state.session_id != session_id
                || journal
                    .as_ref()
                    .is_some_and(|expected| state.journal.as_ref() != Some(expected))
            {
                return invalid("store belongs to another Session or canonical journal");
            }
            state
        } else {
            root.check_journal_creation(STATE_FILE)?;
            if !root.entries_limited(1)?.is_empty() {
                return invalid("missing store identity in a nonempty directory");
            }
            StoreState {
                schema_version: SCHEMA,
                session_id,
                journal,
                baselines: vec![],
                inputs: vec![],
                candidates: vec![],
                candidate_reservations: vec![],
                heads: vec![],
                promotions: vec![],
                rewinds: vec![],
                promotion_reservations: vec![],
                pending: None,
            }
        };
        admit_state(&state, Limits::default())?;
        if let Some(pending) = &state.pending {
            admit_state(&apply_promotion(&state, pending), Limits::default())?;
        }
        // Persist identity before creating subordinate namespaces. A crash can
        // resume missing empty namespaces, but never reassign a populated store.
        root.mark_journal_initialized(STATE_FILE)?;
        root.atomic_write(STATE_FILE, &state_bytes(&state)?)?;
        let objects = root.child("objects")?;
        let heads = root.child("heads")?;
        objects.sync_all()?;
        heads.sync_all()?;
        root.sync_all()?;
        let mut store = Self {
            root,
            objects,
            heads,
            state,
            uncertain: false,
            limits: Limits::default(),
        };
        if store.state.pending.is_some() {
            store.finish_pending()?;
        }
        store.verify_heads()?;
        Ok(store)
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
        if !canonical.records()?.is_empty()
            || !self.state.inputs.is_empty()
            || !self.state.candidates.is_empty()
            || !self.state.promotions.is_empty()
            || !self.state.promotion_reservations.is_empty()
            || self.state.pending.is_some()
        {
            return invalid("legacy baseline must precede every v2 input and promotion");
        }
        let record = &projection.record;
        if projection.payload.len() != record.payload_bytes
            || digest_bytes(&projection.payload) != record.payload_sha256
        {
            return invalid("legacy projection payload changed");
        }
        if let Some(existing) = self.state.baselines.iter().find(|existing| {
            existing.slot_id == record.slot_id
                || existing.reference.conversation_id == record.reference.conversation_id
        }) {
            if existing != record {
                return invalid("legacy baseline is immutable");
            }
            self.load_baseline(existing)?;
            return Ok(existing.reference.clone());
        }
        let mut next = self.state.clone();
        next.baselines.push(record.clone());
        next.baselines.sort_by(|a, b| {
            a.reference
                .conversation_id
                .as_str()
                .cmp(b.reference.conversation_id.as_str())
        });
        self.admit(&next)?;
        let name = object_name(&record.reference);
        if self.objects.is_file(&name)?
            && self.objects.read_limited(&name, MAX_CHECKPOINT_BYTES)? != projection.payload
        {
            return invalid("immutable baseline artifact already contains different bytes");
        }
        self.uncertain = true;
        self.retain_legacy_checkpoint_archive(projection)?;
        self.objects.atomic_write(&name, &projection.payload)?;
        self.persist(next)?;
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
        if item.activation.session_id != self.state.session_id {
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
        if let Some(existing) = self.input(activation) {
            return if existing == &record {
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
        let mut next = self.state.clone();
        next.journal = Some(journal);
        next.inputs.push(record);
        self.admit(&next)?;
        self.persist(next)
    }

    /// Reserve one bounded candidate before provider work. The current canonical
    /// store, rather than a previously captured snapshot, proves that this exact
    /// activation and epoch are still running. Controller dispatch authority and
    /// physical input/profile checks remain separate mandatory host checks.
    /// This reserves journal capacity, not physical filesystem free space.
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
            .input(activation)
            .filter(|input| input.input == item.input)
            .ok_or_else(|| invalid_error("candidate reservation requires exact durable input"))?;
        let record = CandidateReservation {
            activation: activation.clone(),
            input_sha256: input.sha256.clone(),
            max_checkpoint_bytes: MAX_CHECKPOINT_BYTES,
        };
        let conversation_id = input.input.conversation_id.clone();
        if let Some(existing) = self
            .state
            .candidate_reservations
            .iter()
            .find(|existing| existing.activation == *activation)
        {
            if existing != &record {
                return invalid("candidate reservation differs from durable input");
            }
        } else {
            if self.state.candidates.iter().any(|candidate| {
                matches!(&candidate.reference.source, CheckpointSource::Accepted { activation: producer } if producer == activation)
            }) {
                return invalid("cannot reserve an activation after its candidate was retained");
            }
            let mut next = self.state.clone();
            next.candidate_reservations.push(record.clone());
            self.admit(&next)?;
            self.persist(next)?;
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
            .input(activation)
            .filter(|input| input.input == item.input)
            .ok_or_else(|| {
                invalid_error("candidate reservation input differs from canonical history")
            })?;
        let record = self
            .state
            .candidate_reservations
            .iter()
            .find(|record| record.activation == *activation && record.input_sha256 == input.sha256)
            .ok_or_else(|| invalid_error("activation has no durable candidate reservation"))?
            .clone();
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
            .input(reservation.activation())
            .ok_or_else(|| invalid_error("reserved activation has no input"))?
            .clone();
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
    /// reservation API so provider admission cannot consume settlement capacity.
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
            .input(activation)
            .ok_or_else(|| invalid_error("starting input was not persisted"))?
            .clone();
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
            session_id: self.state.session_id.clone(),
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
        if let Some(existing) = self.candidate(&reference) {
            if existing != &candidate {
                return invalid("checkpoint identity collision");
            }
            self.load_candidate(existing)?;
            return Ok(reference);
        }
        if self.state.candidate_reservations.iter().any(|record| record.activation == *activation)
            && self.state.candidates.iter().any(|candidate| {
                matches!(&candidate.reference.source, CheckpointSource::Accepted { activation: producer } if producer == activation)
            })
        {
            return invalid("reserved activation already retained a different candidate");
        }
        let mut next = self.state.clone();
        next.candidates.push(candidate.clone());
        self.admit(&next)?;
        let name = object_name(&reference);
        if self.objects.is_file(&name)?
            && self.objects.read_limited(&name, MAX_CHECKPOINT_BYTES)? != payload
        {
            return invalid("immutable checkpoint file already contains different bytes");
        }
        self.uncertain = true;
        self.objects.atomic_write(&name, &payload)?;
        self.persist(next)?;
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
            || !self
                .state
                .candidate_reservations
                .contains(&reservation.record)
            || self.input(reservation.activation()).is_none_or(|input| {
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
            .input(activation)
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
        Ok(
            rewind::effective_reference(&self.state, conversation, self.state.promotions.len())
                .cloned(),
        )
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
            for (slot, conversation) in
                self.state
                    .inputs
                    .iter()
                    .map(|input| (&input.slot_id, &input.input.conversation_id))
                    .chain(
                        self.state.baselines.iter().map(|baseline| {
                            (&baseline.slot_id, &baseline.reference.conversation_id)
                        }),
                    )
            {
                // A reviewed future Team Reset creates a fresh conversation
                // for this slot. Historical conversation ownership never moves
                // to another slot; selection authority belongs to Team admission.
                if conversation == &node.conversation_id && slot != &node.slot_id {
                    return invalid("graph assigns a retained conversation to another team slot");
                }
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
        self.state
            .baselines
            .iter()
            .find(|baseline| baseline.reference.conversation_id == *conversation)
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
        match self
            .state
            .promotions
            .iter()
            .find(|item| item.closure.turn_id() == closure.turn_id())
        {
            Some(existing)
                if existing.closure == closure && existing.contract_sha256 == contract_sha256 =>
            {
                Ok(Some(existing.clone()))
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
        if self
            .state
            .promotion_reservations
            .contains(snapshot.turn_id())
            || self
                .state
                .inputs
                .iter()
                .any(|input| &input.input.activation.turn_id == snapshot.turn_id())
        {
            // Input admission already reserves this turn and its selected heads.
            return Ok(());
        }
        let mut next = self.state.clone();
        next.journal = Some(journal);
        next.promotion_reservations.push(snapshot.turn_id().clone());
        self.admit(&next)?;
        self.persist(next)
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
                .input(&item.activation)
                .ok_or_else(|| invalid_error("accepted input is not persisted"))?;
            if input.input != item.input {
                return invalid("accepted input differs from persisted input");
            }
            let candidate = self
                .candidate(accepted)
                .ok_or_else(|| invalid_error("accepted checkpoint is not retained"))?;
            self.load_candidate(candidate)?;
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
        // Reserve the final manifest/head representation before any intent is
        // acknowledged. Capacity must never strand a recoverable transaction.
        let mut next = self.state.clone();
        next.journal = Some(journal);
        let completed = apply_promotion(&next, &manifest);
        self.admit(&completed)?;
        next.pending = Some(manifest.clone());
        self.admit(&next)?;
        self.persist(next)?;
        self.finish_pending()?;
        Ok(manifest)
    }

    fn check_snapshot(&self, snapshot: &DurableTurnSnapshot) -> Result<CanonicalJournal> {
        let journal = CanonicalJournal {
            journal_id: snapshot.journal_id().to_owned(),
            workspace_id: snapshot.owner().workspace_id.clone(),
        };
        if snapshot.owner().session_id != self.state.session_id
            || self
                .state
                .journal
                .as_ref()
                .is_some_and(|owner| owner != &journal)
        {
            return invalid("snapshot belongs to another canonical journal or workspace");
        }
        Ok(journal)
    }

    fn committed_base(&self, activation: &ActivationRef) -> Result<Option<CheckpointRef>> {
        let mut at = activation;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(at.activation_id.as_str()) {
                return invalid("cyclic starting savepoint");
            }
            let record = self
                .input(at)
                .ok_or_else(|| invalid_error("starting input is missing"))?;
            match &record.input.starting_savepoint {
                ConversationSavepoint::Empty => return Ok(None),
                ConversationSavepoint::Checkpoint { checkpoint } => match &checkpoint.source {
                    CheckpointSource::Committed { .. } => return Ok(Some((**checkpoint).clone())),
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
        let Some(manifest) = self.state.pending.clone() else {
            return Ok(());
        };
        validate_promotion(&self.state, &manifest)?;
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
        self.persist(apply_promotion(&self.state, &manifest))
    }

    fn load_reference(&self, reference: &CheckpointRef) -> Result<AgentCheckpoint> {
        if reference.session_id != self.state.session_id {
            return invalid("checkpoint belongs to another Session");
        }
        match &reference.source {
            CheckpointSource::Accepted { .. } => {
                let candidate = self
                    .candidate(reference)
                    .ok_or_else(|| invalid_error("unknown checkpoint or changed ownership"))?;
                self.load_candidate(candidate)
            }
            CheckpointSource::Committed { .. } => {
                if let Some(projection) = self
                    .state
                    .rewinds
                    .iter()
                    .flat_map(|rewind| &rewind.conversations)
                    .find(|entry| entry.checkpoint.as_ref() == Some(reference))
                    .and_then(|entry| entry.projection.as_ref())
                {
                    return self.load_payload(
                        reference,
                        &projection.payload_sha256,
                        projection.payload_bytes,
                    );
                }
                if let Some(baseline) = self
                    .state
                    .baselines
                    .iter()
                    .find(|baseline| baseline.reference == *reference)
                {
                    return self.load_baseline(baseline);
                }
                let accepted = self
                    .state
                    .promotions
                    .iter()
                    .flat_map(|p| &p.selected)
                    .find(|entry| entry.committed == *reference)
                    .ok_or_else(|| {
                        invalid_error("unknown committed checkpoint or changed ownership")
                    })?;
                self.load_reference(&accepted.accepted)
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

    fn input(&self, activation: &ActivationRef) -> Option<&InputRecord> {
        self.state
            .inputs
            .iter()
            .find(|item| item.input.activation == *activation)
    }

    fn candidate(&self, reference: &CheckpointRef) -> Option<&Candidate> {
        self.state
            .candidates
            .iter()
            .find(|item| item.reference == *reference)
    }

    fn ready(&self) -> Result<()> {
        if self.uncertain || self.state.pending.is_some() {
            return Err(ActivationStateError::RecoveryRequired);
        }
        self.root.verify_ambient_identity()?;
        Ok(())
    }

    fn admit(&self, next: &StoreState) -> Result<()> {
        admit_state(next, self.limits)
    }

    fn persist(&mut self, next: StoreState) -> Result<()> {
        validate_state(&next)?;
        let bytes = state_bytes(&next)?;
        if bytes.len() > self.limits.state_bytes
            || next.promotions.len() > self.limits.promotions
            || next.candidates.len() > self.limits.candidates
        {
            return Err(ActivationStateError::Capacity);
        }
        self.root.verify_ambient_identity()?;
        self.uncertain = true;
        self.root.atomic_write(STATE_FILE, &bytes)?;
        self.state = next;
        self.uncertain = false;
        Ok(())
    }

    fn verify_heads(&self) -> Result<()> {
        for baseline in &self.state.baselines {
            self.verify_legacy_checkpoint_archive(baseline)?;
            if !self
                .state
                .heads
                .iter()
                .any(|head| head.committed.conversation_id == baseline.reference.conversation_id)
            {
                self.load_baseline(baseline)?;
            }
        }
        for head in &self.state.heads {
            let bytes = self
                .heads
                .read_limited(head_name(&head.committed.conversation_id), MAX_STATE_BYTES)?;
            if serde_json::from_slice::<PromotedConversation>(&bytes)? != *head {
                return invalid(
                    "materialized conversation pointer differs from canonical promotion",
                );
            }
        }
        Ok(())
    }
}

fn admit_state(state: &StoreState, limits: Limits) -> Result<()> {
    validate_state(state)?;
    let (bytes, turns) = promotion_reservation(state);
    let candidates = unfilled_candidate_reservations(state);
    if state_bytes(state)?
        .len()
        .saturating_add(bytes)
        .saturating_add(candidates.saturating_mul(CANDIDATE_METADATA_RESERVE))
        > limits.state_bytes
        || state.candidates.len().saturating_add(candidates) > limits.candidates
        || state
            .promotions
            .len()
            .saturating_add(usize::from(state.pending.is_some()))
            .saturating_add(turns)
            > limits.promotions
    {
        return Err(ActivationStateError::Capacity);
    }
    Ok(())
}

fn unfilled_candidate_reservations(state: &StoreState) -> usize {
    state.candidate_reservations.iter().filter(|reservation| {
        !state.candidates.iter().any(|candidate| {
            matches!(&candidate.reference.source, CheckpointSource::Accepted { activation } if activation == &reservation.activation)
        })
    }).count()
}

fn promotion_reservation(state: &StoreState) -> (usize, usize) {
    let completed: HashSet<_> = state
        .promotions
        .iter()
        .chain(state.pending.iter())
        .map(|manifest| manifest.closure.turn_id())
        .collect();
    let mut turns = state
        .promotion_reservations
        .iter()
        .filter(|turn| !completed.contains(turn))
        .collect::<HashSet<_>>();
    let mut conversations = HashSet::new();
    for input in &state.inputs {
        let turn = &input.input.activation.turn_id;
        if !completed.contains(turn) {
            turns.insert(turn);
            conversations.insert((turn, &input.input.conversation_id));
        }
    }
    (
        conversations
            .len()
            .saturating_mul(PROMOTION_CONVERSATION_RESERVE)
            .saturating_add(turns.len().saturating_mul(PROMOTION_TURN_RESERVE)),
        turns.len(),
    )
}

fn validate_state(state: &StoreState) -> Result<()> {
    if state.schema_version != SCHEMA {
        return invalid("unsupported activation-state schema");
    }
    if state.baselines.len() > MAX_BASELINES
        || state.inputs.len() > MAX_INPUTS
        || state.candidates.len() > MAX_CANDIDATES
        || state.candidate_reservations.len() > MAX_INPUTS
        || state.promotions.len() > MAX_PROMOTIONS
        || state.promotion_reservations.len() > MAX_PROMOTIONS
    {
        return Err(ActivationStateError::Capacity);
    }
    if let Some(journal) = &state.journal {
        if journal.workspace_id.is_empty()
            || journal.workspace_id.len() > 128
            || journal.workspace_id.chars().any(char::is_control)
            || uuid::Uuid::parse_str(&journal.journal_id)
                .ok()
                .is_none_or(|id| id.is_nil() || id.to_string() != journal.journal_id)
        {
            return invalid("invalid canonical journal binding");
        }
    } else if !state.baselines.is_empty()
        || !state.inputs.is_empty()
        || !state.candidates.is_empty()
        || !state.candidate_reservations.is_empty()
        || !state.promotions.is_empty()
        || !state.promotion_reservations.is_empty()
        || state.pending.is_some()
        || !state.heads.is_empty()
        || !state.rewinds.is_empty()
    {
        return invalid("populated artifact namespace has no canonical journal binding");
    }
    if state
        .promotion_reservations
        .iter()
        .collect::<HashSet<_>>()
        .len()
        != state.promotion_reservations.len()
        || state
            .promotion_reservations
            .iter()
            .any(|turn| state.promotions.iter().any(|p| p.closure.turn_id() == turn))
    {
        return invalid("invalid or already completed promotion reservation");
    }
    let mut input_ids = HashSet::new();
    let mut activation_ids = HashSet::new();
    let mut bindings = vec![];
    let mut baseline_conversations = HashSet::new();
    let mut baseline_slots = HashSet::new();
    for baseline in &state.baselines {
        let journal = state
            .journal
            .as_ref()
            .ok_or_else(|| invalid_error("baseline has no journal"))?;
        if baseline.reference.session_id != state.session_id
            || baseline.reference != baseline_reference(journal, baseline)?
            || !valid_baseline_policy(baseline)
            || baseline.original_agent_id.is_empty()
            || baseline.original_agent_id.len() > 256
            || baseline.visible_turns.len() > MAX_BASELINE_TURNS
            || baseline.visible_turns.iter().collect::<HashSet<_>>().len()
                != baseline.visible_turns.len()
            || !is_digest(&baseline.payload_sha256)
            || baseline.payload_bytes > MAX_CHECKPOINT_BYTES
            || !baseline_conversations.insert(&baseline.reference.conversation_id)
            || !baseline_slots.insert(&baseline.slot_id)
        {
            return invalid("legacy baseline identity, ownership, or projection mismatch");
        }
        bindings.push((
            baseline.slot_id.clone(),
            baseline.reference.conversation_id.clone(),
        ));
    }
    for record in &state.inputs {
        let input = &record.input;
        if input.activation.session_id != state.session_id
            || record.sha256 != digest(&(&record.slot_id, input))?
            || !input_ids.insert(&input.manifest_id)
            || !activation_ids.insert(&input.activation.activation_id)
        {
            return invalid("input identity, ownership, or digest mismatch");
        }
        if bindings.iter().any(|(slot, conversation)| {
            conversation == &input.conversation_id && slot != &record.slot_id
        }) {
            return invalid("Session conversation identity is shared by different slots");
        }
        bindings.push((record.slot_id.clone(), input.conversation_id.clone()));
    }
    let mut ids = HashSet::new();
    for candidate in &state.candidates {
        let CheckpointSource::Accepted { activation } = &candidate.reference.source else {
            return invalid("candidate has non-activation source");
        };
        let input = state
            .inputs
            .iter()
            .find(|item| item.input.activation == *activation)
            .ok_or_else(|| invalid_error("candidate has no immutable input"))?;
        let key = digest(&(
            state
                .journal
                .as_ref()
                .ok_or_else(|| invalid_error("missing journal binding"))?,
            activation,
            &input.input.conversation_id,
            &input.sha256,
            &candidate.payload_sha256,
            candidate.payload_bytes,
        ))?;
        if candidate.reference.session_id != state.session_id
            || candidate.reference.conversation_id != input.input.conversation_id
            || candidate.input_sha256 != input.sha256
            || candidate.reference.checkpoint_id.as_str() != format!("checkpoint:{key}")
            || candidate.payload_bytes > MAX_CHECKPOINT_BYTES
            || !is_digest(&candidate.payload_sha256)
            || !ids.insert(&candidate.reference.checkpoint_id)
        {
            return invalid("candidate identity, ownership, or digest mismatch");
        }
    }
    let mut reserved = HashSet::new();
    for reservation in &state.candidate_reservations {
        let input = state
            .inputs
            .iter()
            .find(|input| input.input.activation == reservation.activation)
            .ok_or_else(|| invalid_error("candidate reservation has no immutable input"))?;
        if reservation.input_sha256 != input.sha256
            || reservation.max_checkpoint_bytes != MAX_CHECKPOINT_BYTES
            || !reserved.insert(&reservation.activation.activation_id)
            || state.candidates.iter().filter(|candidate| {
                matches!(&candidate.reference.source, CheckpointSource::Accepted { activation } if activation == &reservation.activation)
            }).count() > 1
        {
            return invalid("candidate reservation identity, capacity, or single settlement mismatch");
        }
    }
    let mut turns = HashSet::new();
    let mut expected_heads: Vec<PromotedConversation> = vec![];
    rewind::validate_rewinds(state)?;
    for (index, promotion) in state
        .promotions
        .iter()
        .chain(state.pending.iter())
        .enumerate()
    {
        if !turns.insert(promotion.closure.turn_id()) {
            return invalid("duplicate promotion for closed turn");
        }
        validate_promotion(state, promotion)?;
        for selected in &promotion.selected {
            let previous =
                rewind::effective_reference(state, &selected.committed.conversation_id, index);
            if previous != selected.previous_committed.as_ref() {
                return invalid("promotion does not follow the prior committed conversation");
            }
        }
        if state.pending.as_ref() != Some(promotion) {
            for selected in &promotion.selected {
                expected_heads.retain(|head| {
                    head.committed.conversation_id != selected.committed.conversation_id
                });
                expected_heads.push(selected.clone());
            }
        }
    }
    expected_heads.sort_by(|a, b| {
        a.committed
            .conversation_id
            .as_str()
            .cmp(b.committed.conversation_id.as_str())
    });
    if state.heads != expected_heads {
        return invalid("conversation heads do not match exact promotion history");
    }
    let mut conversations = HashSet::new();
    for head in &state.heads {
        if !conversations.insert(&head.committed.conversation_id)
            || !state.promotions.iter().any(|p| p.selected.contains(head))
        {
            return invalid("conversation head is not a uniquely committed selection");
        }
    }
    Ok(())
}

fn validate_promotion(state: &StoreState, manifest: &PromotionManifest) -> Result<()> {
    let journal = state
        .journal
        .as_ref()
        .ok_or_else(|| invalid_error("missing journal binding"))?;
    if manifest.journal_id != journal.journal_id
        || manifest.workspace_id != journal.workspace_id
        || manifest.closure.session_id() != &state.session_id
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
        let CheckpointSource::Accepted { activation } = &entry.accepted.source else {
            return invalid("promotion lacks exact activation source");
        };
        if activation.turn_id != *manifest.closure.turn_id()
            || activation.node_id != entry.node_id
            || !nodes.insert(&entry.node_id)
            || !conversations.insert(&entry.accepted.conversation_id)
            || !state.inputs.iter().any(|record| {
                record.input.activation == *activation && record.slot_id == entry.slot_id
            })
            || entry.committed != committed_ref(&manifest.promotion_id, &entry.accepted)?
            || !state
                .candidates
                .iter()
                .any(|candidate| candidate.reference == entry.accepted)
        {
            return invalid("promotion selects a foreign or missing checkpoint");
        }
    }
    Ok(())
}

fn apply_promotion(state: &StoreState, manifest: &PromotionManifest) -> StoreState {
    let mut next = state.clone();
    for entry in &manifest.selected {
        next.heads
            .retain(|head| head.committed.conversation_id != entry.committed.conversation_id);
        next.heads.push(entry.clone());
    }
    next.heads.sort_by(|a, b| {
        a.committed
            .conversation_id
            .as_str()
            .cmp(b.committed.conversation_id.as_str())
    });
    next.promotions.push(manifest.clone());
    next.promotion_reservations
        .retain(|turn| turn != manifest.closure.turn_id());
    next.pending = None;
    next
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
            || output.disposition.is_some()
            || output.causal_signal_id.is_some()
            || output.superseded
            || output.superseded_by_generation.is_some()
            || output.superseded_by_signal_id.is_some()
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
                || output.disposition.is_some()
                || output.causal_signal_id.is_some()
                || output.superseded
                || output.superseded_by_generation.is_some()
                || output.superseded_by_signal_id.is_some()
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

fn state_bytes(state: &StoreState) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(state)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(ActivationStateError::Capacity);
    }
    Ok(bytes)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use axocoatl_session::turn_contract::{CommandId, TurnContractEnvelope, TurnContractEvent};
    use std::fs;
    use std::sync::Arc;

    #[test]
    fn disk_admission_reserves_promotion_space_before_acceptance_and_survives_reopen() {
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
        let before = fs::read(root.path().join(STATE_FILE)).unwrap();
        store.limits.promotions = 0;
        assert!(matches!(
            store.record_input(&snapshot, &activation),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(fs::read(root.path().join(STATE_FILE)).unwrap(), before);
        store.limits = Limits::default();
        store.record_input(&snapshot, &activation).unwrap();
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
        let (reserved, turns) = promotion_reservation(&store.state);
        assert_eq!(turns, 1);
        store.limits.state_bytes = state_bytes(&store.state).unwrap().len() + reserved;
        let bytes = fs::read(root.path().join(STATE_FILE)).unwrap();
        checkpoint.version = 900;
        assert!(matches!(
            store.stage_candidate(&snapshot, &activation, &checkpoint),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(fs::read(root.path().join(STATE_FILE)).unwrap(), bytes);
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
        assert_eq!(promotion_reservation(&store.state), (0, 0));
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
    fn empty_checkpoint() -> AgentCheckpoint {
        AgentCheckpoint {
            version: 1,
            agent_id: "conversation-a".into(),
            checkpoint_time: 1,
            session_messages: vec![],
            cumulative_token_usage: TokenUsageStats::new(5, 3),
            cumulative_token_usage_known: true,
            behavior_state: None,
        }
    }

    #[test]
    fn owned_admission_reserves_last_candidate_slot_before_any_provider_work() {
        let (_root, canonical, mut memory, activations) = fixture();
        memory.limits.candidates = 1;
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        memory
            .record_input(
                &canonical.snapshot(&activations[1].turn_id).unwrap(),
                &activations[1],
            )
            .unwrap();
        let before = memory
            .root
            .read_limited(STATE_FILE, MAX_STATE_BYTES)
            .unwrap();
        assert!(matches!(
            memory.reserve_candidate(&canonical, &activations[1]),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(
            memory
                .root
                .read_limited(STATE_FILE, MAX_STATE_BYTES)
                .unwrap(),
            before
        );
        let candidate = memory
            .stage_reserved_candidate(&reservation, &empty_checkpoint())
            .unwrap();
        assert_eq!(memory.state.candidates.len(), 1);
        assert_eq!(unfilled_candidate_reservations(&memory.state), 0);
        assert_eq!(
            memory
                .checkpoint(&candidate)
                .unwrap()
                .cumulative_token_usage,
            TokenUsageStats::new(5, 3)
        );
    }

    #[test]
    fn unrelated_input_cannot_spend_reserved_candidate_bytes_and_reopen_keeps_reservation() {
        let (_root, canonical, mut memory, activations) = fixture();
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        let (promotion_bytes, _) = promotion_reservation(&memory.state);
        memory.limits.state_bytes = state_bytes(&memory.state).unwrap().len()
            + promotion_bytes
            + CANDIDATE_METADATA_RESERVE;
        let before = memory
            .root
            .read_limited(STATE_FILE, MAX_STATE_BYTES)
            .unwrap();
        assert!(matches!(
            memory.record_input(
                &canonical.snapshot(&activations[1].turn_id).unwrap(),
                &activations[1]
            ),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(
            memory
                .root
                .read_limited(STATE_FILE, MAX_STATE_BYTES)
                .unwrap(),
            before
        );
        let candidate = memory
            .stage_reserved_candidate(&reservation, &empty_checkpoint())
            .unwrap();
        assert!(
            serde_json::to_vec(&memory.state.candidates[0])
                .unwrap()
                .len()
                < CANDIDATE_METADATA_RESERVE
        );
        let limits = memory.limits;
        drop(memory);
        let mut memory = ActivationStateStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .unwrap(),
        )
        .unwrap();
        memory.limits = limits;
        let recovered = memory
            .candidate_reservation(&canonical, &activations[0])
            .unwrap();
        assert_eq!(
            memory
                .stage_reserved_candidate(&recovered, &empty_checkpoint())
                .unwrap(),
            candidate
        );
        admit_state(&memory.state, limits).unwrap();
    }

    #[test]
    fn reserved_checkpoint_byte_cap_is_checked_before_any_object_publication() {
        let (_root, canonical, mut memory, activations) = fixture();
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        let before = memory
            .root
            .read_limited(STATE_FILE, MAX_STATE_BYTES)
            .unwrap();
        let mut checkpoint = empty_checkpoint();
        checkpoint.behavior_state = Some("x".repeat(reservation.max_checkpoint_bytes()));
        assert!(matches!(
            memory.stage_reserved_candidate(&reservation, &checkpoint),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(
            memory
                .root
                .read_limited(STATE_FILE, MAX_STATE_BYTES)
                .unwrap(),
            before
        );
        assert!(memory.objects.entries_limited(1).unwrap().is_empty());
        assert_eq!(unfilled_candidate_reservations(&memory.state), 1);
    }

    #[test]
    fn interrupted_candidate_publication_keeps_reservation_and_reuses_exact_orphan_after_reopen() {
        let (_root, canonical, mut memory, activations) = fixture();
        let reservation = memory
            .reserve_candidate(&canonical, &activations[0])
            .unwrap();
        let root = canonical.path().parent().unwrap().join("activation-state");
        let primary = root.join(STATE_FILE);
        let saved = root.join("saved-state");
        fs::rename(&primary, &saved).unwrap();
        fs::create_dir(&primary).unwrap();
        assert!(memory
            .stage_reserved_candidate(&reservation, &empty_checkpoint())
            .is_err());
        assert!(matches!(
            memory.starting_checkpoint_for(&reservation),
            Err(ActivationStateError::RecoveryRequired)
        ));
        let files: Vec<_> = fs::read_dir(root.join("objects")).unwrap().collect();
        assert_eq!(files.len(), 1);
        let orphan = fs::read(files[0].as_ref().unwrap().path()).unwrap();
        drop(memory);
        fs::remove_dir(&primary).unwrap();
        fs::rename(saved, primary).unwrap();
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
            .stage_reserved_candidate(&recovered, &empty_checkpoint())
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
    fn empty_turn_reserves_durable_promotion_before_close_at_capacity() {
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
        let before = std::fs::read(root.path().join(STATE_FILE)).unwrap();
        memory.limits.state_bytes = before.len() + 8;
        assert!(matches!(
            memory.prepare_close(&snapshot, TurnClosure::Cancelled),
            Err(ActivationStateError::Capacity)
        ));
        assert_eq!(std::fs::read(root.path().join(STATE_FILE)).unwrap(), before);
        assert_eq!(
            canonical
                .snapshot(&begin.turn_id)
                .unwrap()
                .contract()
                .state(),
            Some(LogicalTurnState::Running)
        );
        memory.limits = Limits::default();
        memory
            .prepare_close(&snapshot, TurnClosure::Cancelled)
            .unwrap();
        assert_eq!(
            promotion_reservation(&memory.state),
            (PROMOTION_TURN_RESERVE, 1)
        );
        drop(memory);
        let mut memory = ActivationStateStore::open(root.path(), begin.session_id.clone()).unwrap();
        let (reserved, _) = promotion_reservation(&memory.state);
        memory.limits.state_bytes = state_bytes(&memory.state).unwrap().len() + reserved;
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
        assert!(memory.state.promotion_reservations.is_empty());
        assert_eq!(memory.promotion(&closed).unwrap(), Some(promoted.clone()));
        drop(memory);
        let reopened = ActivationStateStore::open(root.path(), begin.session_id).unwrap();
        assert_eq!(reopened.promotion(&closed).unwrap(), Some(promoted));
    }
}
