//! Typed requests and durable, bounded control-command receipts.
//!
//! Public JSON contains requested parameters only. Source attribution enters
//! through an opaque host value; neither a serialized human tag nor an evidence
//! identifier authenticates a caller. Agent attribution is minted by the live
//! ControlAuthority after validating its generation lease. Attribution is not
//! authorization to execute the requested operation.
//!
//! The controller must validate current canonical revisions, graph legality,
//! grants, evidence, resource ownership and effect safety, then durably join the
//! semantic transition before recording Applied. This journal alone cannot make
//! those stores atomic. Recovered Accepted/Applied receipts remain pending; they
//! never imply that an actor or an external effect resumed or settled.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::turn_contract::{
    ActivationInputManifest, ActivationRef, CheckpointSource, CommandId, ConditionId,
    ContinuationPlan, ContinuationSelection, ConversationSavepoint, DelegatedOperation,
    EvidenceRef, ExecutionEpochId, GrantId, InvocationId, LogicalTurnId, SessionId, TurnNodeId,
    MAX_COMPLETION_CONDITIONS,
};

pub const CONTROL_COMMAND_SCHEMA_VERSION: u32 = 1;
pub const MAX_CONTROL_REQUEST_BYTES: usize = 128 * 1024;
const FILE: &str = "control-command.v1.json";
const MAX_COMMANDS: usize = 256;
const MAX_RECORDS: usize = MAX_COMMANDS * 4;
const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 8 * 1024;
const MAX_UPDATE_RECORD_BYTES: usize = 9 * 1024;
const MAX_REQUEST_RECORD_BYTES: usize = MAX_CONTROL_REQUEST_BYTES + 4 * 1024;
const MAX_REFERENCES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlCommandOwner {
    pub workspace_id: String,
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
}

/// References must be resolved against the exact invocation's authoritative
/// audit and current approval policy. Naming a decision does not grant replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationDecisionRef {
    pub invocation_id: InvocationId,
    pub activation: ActivationRef,
    pub decision: EvidenceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerMode {
    NextSafeBoundary,
    InterruptAndRevise,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlockerResponse {
    Evidence { evidence: EvidenceRef },
    Approval { approval: EvidenceRef },
    Decline { reason: EvidenceRef },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FinishMode {
    Normal,
    ForcePartial {
        approval: EvidenceRef,
        missing_conditions: Vec<EvidenceRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        missing_condition_ids: Vec<ConditionId>,
        stop_activations: Vec<ActivationRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        selected_activations: Vec<ActivationRef>,
    },
}

/// The target is part of each variant, so a free-form kind/target/parameters
/// combination cannot silently select a different interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlParameters {
    StopActivation {
        activation: ActivationRef,
    },
    RetryActivation {
        activation: ActivationRef,
        input: Box<ActivationInputManifest>,
        replay_decisions: Vec<InvocationDecisionRef>,
    },
    SteerActivation {
        activation: ActivationRef,
        instruction: EvidenceRef,
        mode: SteerMode,
    },
    ReviseActivation {
        activation: ActivationRef,
        input: Box<ActivationInputManifest>,
        instruction: EvidenceRef,
        invalidate: Vec<ActivationRef>,
    },
    ResumeBlocked {
        activation: ActivationRef,
        blocker_id: EvidenceRef,
        response: BlockerResponse,
    },
    ContinueTurn {
        plan: ContinuationPlan,
        replay_decisions: Vec<InvocationDecisionRef>,
    },
    AddAgent {
        input: Box<ActivationInputManifest>,
        dependencies: Vec<TurnNodeId>,
    },
    ReplaceFutureAgent {
        target: TurnNodeId,
        input: Box<ActivationInputManifest>,
        rewire_dependents: Vec<TurnNodeId>,
    },
    FinishTurn {
        mode: FinishMode,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlCommandRequest {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub execution_epoch_id: ExecutionEpochId,
    pub expected_turn_revision: u64,
    pub expected_graph_revision: u64,
    pub issued_at_ms: u64,
    pub parameters: ControlParameters,
}

impl ControlCommandRequest {
    /// Bound bytes and reject future schemas before interpreting the payload.
    /// The decoded value is a request, never an authority or an acceptance.
    pub fn decode(bytes: &[u8]) -> Result<Self, ControlCommandError> {
        if bytes.len() > MAX_CONTROL_REQUEST_BYTES {
            return Err(ControlCommandError::Capacity);
        }
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }
        let header: Header = serde_json::from_slice(bytes)?;
        version(header.schema_version)?;
        let request: Self = serde_json::from_slice(bytes)?;
        validate_request(&request)?;
        Ok(request)
    }
}

impl ControlParameters {
    /// Classification only, never authorization. Canonical admission must also
    /// validate every affected node/input, prove replay safety and resolve any
    /// blocker to a trusted machine-resolvable type. Force Finish and human
    /// approval have no delegated classification. An untyped Decline also stays
    /// unclassified until a typed machine-blocker response protocol exists.
    pub fn delegated_operation(&self) -> Option<DelegatedOperation> {
        Some(match self {
            Self::StopActivation { .. } => DelegatedOperation::StopActivation,
            Self::RetryActivation { .. } => DelegatedOperation::RetryActivation,
            Self::SteerActivation {
                mode: SteerMode::NextSafeBoundary,
                ..
            } => DelegatedOperation::SteerActivation,
            Self::SteerActivation {
                mode: SteerMode::InterruptAndRevise,
                ..
            }
            | Self::ReviseActivation { .. } => DelegatedOperation::ReviseActivation,
            Self::ResumeBlocked {
                response: BlockerResponse::Evidence { .. },
                ..
            } => DelegatedOperation::ResumeMachineBlocker,
            Self::ContinueTurn { .. } => DelegatedOperation::ContinueTurn,
            Self::AddAgent { .. } => DelegatedOperation::AddAgent,
            Self::ReplaceFutureAgent { .. } => DelegatedOperation::ReplaceFutureAgent,
            Self::FinishTurn {
                mode: FinishMode::Normal,
            } => DelegatedOperation::FinishNormally,
            Self::ResumeBlocked {
                response: BlockerResponse::Approval { .. } | BlockerResponse::Decline { .. },
                ..
            }
            | Self::FinishTurn {
                mode: FinishMode::ForcePartial { .. },
            } => return None,
        })
    }
}

/// Stored audit attribution. Deserializing this record does not produce a
/// TrustedCommandSource, and callers must never use it as a grant capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandSourceRecord {
    Human {
        session_id: SessionId,
        turn_id: LogicalTurnId,
        request_evidence: EvidenceRef,
    },
    Agent {
        activation: ActivationRef,
        grant_id: String,
        grant_revision: u64,
        grant_evidence: EvidenceRef,
        live_scope: String,
    },
}

/// Not serializable, deserializable, or publicly constructible from a record.
/// This is attribution attested by the host, not operation authorization.
#[derive(Debug)]
pub struct TrustedCommandSource {
    record: CommandSourceRecord,
}

impl TrustedCommandSource {
    /// Inspect host-attested attribution without turning a deserialized record
    /// into authority. The controller still validates its current permission.
    pub fn record(&self) -> &CommandSourceRecord {
        &self.record
    }

    /// Privileged host API: call only after authenticating the human channel and
    /// deriving its Session owner. The evidence is retained attribution, not a
    /// substitute for authentication. Never expose this factory to model tools.
    pub fn human(
        session_id: SessionId,
        turn_id: LogicalTurnId,
        request_evidence: EvidenceRef,
    ) -> Self {
        Self {
            record: CommandSourceRecord::Human {
                session_id,
                turn_id,
                request_evidence,
            },
        }
    }

    /// Only the live authority's validated lease path should call this factory.
    pub(crate) fn agent(
        activation: ActivationRef,
        grant_id: String,
        grant_revision: u64,
        grant_evidence: EvidenceRef,
        live_scope: String,
    ) -> Self {
        Self {
            record: CommandSourceRecord::Agent {
                activation,
                grant_id,
                grant_revision,
                grant_evidence,
                live_scope,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlCommandState {
    Requested,
    Rejected,
    Accepted,
    Applied,
    Settled,
    Failed,
}

impl ControlCommandState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Rejected | Self::Settled | Self::Failed)
    }

    fn remaining_records(self) -> usize {
        match self {
            Self::Requested => 3,
            Self::Accepted => 2,
            Self::Applied => 1,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandFailure {
    pub code: String,
    pub message: String,
    pub evidence: Option<EvidenceRef>,
    pub blocker: Option<EvidenceRef>,
}

/// Host-only lifecycle observations. Applied must refer to an already durable
/// canonical causal batch. Settled must name actual safe-boundary evidence;
/// neither this type nor an untrusted serialized instance proves those facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlTransition {
    Rejected {
        failure: CommandFailure,
    },
    Accepted {
        validation: EvidenceRef,
        pending: EvidenceRef,
    },
    Applied {
        state_transition: EvidenceRef,
        turn_revision: u64,
        graph_revision: u64,
        pending: EvidenceRef,
    },
    Settled {
        result: EvidenceRef,
    },
    Failed {
        failure: CommandFailure,
    },
}

impl ControlTransition {
    fn state(&self) -> ControlCommandState {
        match self {
            Self::Rejected { .. } => ControlCommandState::Rejected,
            Self::Accepted { .. } => ControlCommandState::Accepted,
            Self::Applied { .. } => ControlCommandState::Applied,
            Self::Settled { .. } => ControlCommandState::Settled,
            Self::Failed { .. } => ControlCommandState::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlReceiptUpdate {
    pub update_id: CommandId,
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub execution_epoch_id: ExecutionEpochId,
    pub expected_receipt_revision: u64,
    pub transition: ControlTransition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlCommandEvent {
    Requested {
        request: Box<ControlCommandRequest>,
        source: CommandSourceRecord,
    },
    Transition {
        update: ControlReceiptUpdate,
    },
}

impl ControlCommandEvent {
    fn operation_id(&self) -> &CommandId {
        match self {
            Self::Requested { request, .. } => &request.command_id,
            Self::Transition { update } => &update.update_id,
        }
    }

    fn command_id(&self) -> &CommandId {
        match self {
            Self::Requested { request, .. } => &request.command_id,
            Self::Transition { update } => &update.command_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlCommandRecord {
    pub sequence: u64,
    pub receipt_revision: u64,
    pub event: ControlCommandEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandReceiptView {
    pub request: ControlCommandRequest,
    pub source: CommandSourceRecord,
    pub revision: u64,
    pub state: ControlCommandState,
    pub last_transition: Option<ControlTransition>,
}

/// Storage acknowledgement only; deliberately not a dispatch/control lease.
/// A receipt is a snapshot. Use lookup_request or receipt to obtain current state.
#[derive(Debug)]
pub struct DurableCommandReceipt {
    journal_id: String,
    view: CommandReceiptView,
}

impl DurableCommandReceipt {
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }
    pub fn view(&self) -> &CommandReceiptView {
        &self.view
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ControlCommandError {
    #[error("control-command storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("control-command JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported control-command schema {0}")]
    UnsupportedVersion(u32),
    #[error("invalid control-command contract: {0}")]
    Invalid(&'static str),
    #[error("control-command owner or exact target mismatch")]
    OwnerConflict,
    #[error("command/update identity already has a different immutable payload")]
    CommandConflict,
    #[error("control command was not requested")]
    NotFound,
    #[error("stale receipt revision: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("illegal or already terminal control-command transition")]
    InvalidTransition,
    #[error("retained control-command capacity exhausted")]
    Capacity,
    #[error("uncertain write; reopen before returning any receipt or doing more work")]
    RecoveryRequired,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandData {
    schema_version: u32,
    journal_id: String,
    owner: ControlCommandOwner,
    records: Vec<ControlCommandRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical_journal_id: Option<String>,
}

#[derive(Clone, Default)]
struct Projection {
    receipts: HashMap<CommandId, CommandReceiptView>,
    operations: HashMap<CommandId, ControlCommandEvent>,
}

/// Use a distinct existing, durably provisioned private directory outside the
/// repository mount. The composing v2 controller must hold format ownership;
/// opening this independent receipt store does not perform a legacy migration.
/// All receipts remain retained, including settled and unresolved commands.
pub struct ControlCommandStore {
    dir: SecureDir,
    namespace: Option<OwnedExecutionNamespace>,
    data: CommandData,
    projection: Projection,
    poisoned: bool,
}

impl ControlCommandStore {
    pub fn open(
        path: impl AsRef<Path>,
        owner: ControlCommandOwner,
    ) -> Result<Self, ControlCommandError> {
        bounded_id(&owner.workspace_id)?;
        let dir = SecureDir::open(path)?;
        Self::open_in(dir, owner, None)
    }

    /// Inspect exact existing receipts for a closed canonical turn. This does
    /// not reopen the command store, re-acknowledge writes, reconcile pending
    /// commands or return an executable/durable acknowledgement capability.
    pub fn read_historical_views(
        canonical: &crate::execution_store::SessionExecutionStore,
        turn_id: &LogicalTurnId,
    ) -> Result<Vec<CommandReceiptView>, ControlCommandError> {
        let snapshot = canonical
            .snapshot(turn_id)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if !snapshot
            .contract()
            .state()
            .is_some_and(|state| state.is_closed())
        {
            return Err(ControlCommandError::Invalid(
                "historical command reads require a closed canonical turn",
            ));
        }
        Self::read_retained_views(canonical, turn_id)
    }

    /// Read existing receipts under the retained canonical owner. This is a
    /// presentation-only projection, including a paused recovered turn; it
    /// never reopens a writer, acknowledges a receipt or authorizes a command.
    pub fn read_retained_views(
        canonical: &crate::execution_store::SessionExecutionStore,
        turn_id: &LogicalTurnId,
    ) -> Result<Vec<CommandReceiptView>, ControlCommandError> {
        let snapshot = canonical
            .snapshot(turn_id)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let bytes = canonical
            .read_existing_component(
                &ExecutionComponent::ControlCommands {
                    turn_id: turn_id.clone(),
                },
                Path::new(FILE),
                MAX_STORE_BYTES,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let data: CommandData = serde_json::from_slice(&bytes)?;
        let expected = ControlCommandOwner {
            workspace_id: snapshot.owner().workspace_id.clone(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: turn_id.clone(),
        };
        if data.owner != expected
            || data.canonical_journal_id.as_deref() != Some(snapshot.journal_id())
        {
            return Err(ControlCommandError::OwnerConflict);
        }
        let projection = rebuild(&data)?;
        data.records
            .iter()
            .filter_map(|record| {
                if let ControlCommandEvent::Requested { request, .. } = &record.event {
                    Some(
                        projection
                            .receipts
                            .get(&request.command_id)
                            .cloned()
                            .ok_or(ControlCommandError::NotFound),
                    )
                } else {
                    None
                }
            })
            .collect()
    }

    /// Read the actual live owned journal under its controller's synchronization.
    /// This returns presentation records, never dispatch or recovery authority.
    pub fn read_owned_views(
        &self,
        canonical: &crate::execution_store::SessionExecutionStore,
        turn_id: &LogicalTurnId,
    ) -> Result<Vec<CommandReceiptView>, ControlCommandError> {
        self.ensure_usable()?;
        let identity = canonical
            .identity()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let namespace = self
            .namespace
            .as_ref()
            .ok_or(ControlCommandError::OwnerConflict)?;
        if namespace.identity() != &identity
            || self.data.owner.workspace_id != identity.owner().workspace_id
            || self.data.owner.session_id != identity.owner().session_id
            || &self.data.owner.turn_id != turn_id
            || self.data.canonical_journal_id.as_deref() != Some(identity.journal_id())
        {
            return Err(ControlCommandError::OwnerConflict);
        }
        namespace.require_root(&ExecutionComponent::ControlCommands {
            turn_id: turn_id.clone(),
        })?;
        self.data
            .records
            .iter()
            .filter_map(|record| {
                if let ControlCommandEvent::Requested { request, .. } = &record.event {
                    Some(
                        self.projection
                            .receipts
                            .get(&request.command_id)
                            .cloned()
                            .ok_or(ControlCommandError::NotFound),
                    )
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn open_owned(namespace: OwnedExecutionNamespace) -> Result<Self, ControlCommandError> {
        let ExecutionComponent::ControlCommands { turn_id } = namespace.component() else {
            return Err(ControlCommandError::Invalid("wrong owned component"));
        };
        let turn_id = turn_id.clone();
        namespace.require_root(&ExecutionComponent::ControlCommands {
            turn_id: turn_id.clone(),
        })?;
        let owner = ControlCommandOwner {
            workspace_id: namespace.identity().owner().workspace_id.clone(),
            session_id: namespace.identity().owner().session_id.clone(),
            turn_id,
        };
        let dir = namespace.secure_dir()?;
        Self::open_in(dir, owner, Some(namespace))
    }

    fn open_in(
        dir: SecureDir,
        owner: ControlCommandOwner,
        namespace: Option<OwnedExecutionNamespace>,
    ) -> Result<Self, ControlCommandError> {
        bounded_id(&owner.workspace_id)?;
        dir.restrict_owner_only()?;
        #[cfg(unix)]
        dir.try_lock_exclusive()?;
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "control-command storage requires single-writer directory locking",
        )
        .into());
        let canonical_journal_id = namespace
            .as_ref()
            .map(|ns| ns.identity().journal_id().to_owned());
        let data = match dir.read_limited(FILE, MAX_STORE_BYTES) {
            Ok(bytes) => {
                #[derive(Deserialize)]
                struct Header {
                    schema_version: u32,
                }
                version(serde_json::from_slice::<Header>(&bytes)?.schema_version)?;
                serde_json::from_slice::<CommandData>(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(namespace) = &namespace {
                    namespace.check_journal_creation(FILE)?;
                }
                CommandData {
                    schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
                    journal_id: uuid::Uuid::new_v4().to_string(),
                    owner: owner.clone(),
                    records: Vec::new(),
                    canonical_journal_id: canonical_journal_id.clone(),
                }
            }
            Err(error) => return Err(error.into()),
        };
        if data.owner != owner || data.canonical_journal_id != canonical_journal_id {
            return Err(ControlCommandError::OwnerConflict);
        }
        let projection = rebuild(&data)?;
        if let Some(namespace) = &namespace {
            namespace.mark_journal_initialized(FILE)?;
        }
        dir.verify_ambient_identity()?;
        // A read does not settle a prior rename's uncertain directory fsync.
        // Durably republish the validated history before exposing any receipt.
        dir.atomic_write(FILE, &serde_json::to_vec(&data)?)?;
        Ok(Self {
            dir,
            namespace,
            data,
            projection,
            poisoned: false,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(FILE)
    }

    /// Resolve exact command identity before checking current canonical turn
    /// or graph revisions. This read does not authorize any further operation.
    pub fn lookup_request(
        &self,
        request: &ControlCommandRequest,
    ) -> Result<Option<DurableCommandReceipt>, ControlCommandError> {
        self.ensure_usable()?;
        if let Some(previous) = self.projection.receipts.get(&request.command_id) {
            if &previous.request != request {
                return Err(ControlCommandError::CommandConflict);
            }
            return Ok(Some(self.acknowledgement(previous)));
        }
        if self.projection.operations.contains_key(&request.command_id) {
            return Err(ControlCommandError::CommandConflict);
        }
        Ok(None)
    }

    pub fn record_requested(
        &mut self,
        request: ControlCommandRequest,
        source: TrustedCommandSource,
    ) -> Result<DurableCommandReceipt, ControlCommandError> {
        self.append(ControlCommandEvent::Requested {
            request: Box::new(request),
            source: source.record,
        })
    }

    /// Privileged host path. External clients may request commands; they may
    /// never call this with their own claimed validation/application evidence.
    pub fn advance(
        &mut self,
        update: ControlReceiptUpdate,
    ) -> Result<DurableCommandReceipt, ControlCommandError> {
        self.append(ControlCommandEvent::Transition { update })
    }

    pub fn receipt(
        &self,
        command_id: &CommandId,
    ) -> Result<Option<DurableCommandReceipt>, ControlCommandError> {
        self.ensure_usable()?;
        Ok(self
            .projection
            .receipts
            .get(command_id)
            .map(|view| self.acknowledgement(view)))
    }

    pub fn records(&self) -> Result<&[ControlCommandRecord], ControlCommandError> {
        self.ensure_usable()?;
        Ok(&self.data.records)
    }

    fn acknowledgement(&self, view: &CommandReceiptView) -> DurableCommandReceipt {
        DurableCommandReceipt {
            journal_id: self.data.journal_id.clone(),
            view: view.clone(),
        }
    }

    fn ensure_usable(&self) -> Result<(), ControlCommandError> {
        if self.poisoned {
            return Err(ControlCommandError::RecoveryRequired);
        }
        if let Some(namespace) = &self.namespace {
            namespace.verify_ambient_identity()?;
        }
        self.dir.verify_ambient_identity()?;
        Ok(())
    }

    fn append(
        &mut self,
        event: ControlCommandEvent,
    ) -> Result<DurableCommandReceipt, ControlCommandError> {
        self.append_with(event, |dir, bytes| dir.atomic_write(FILE, bytes))
    }

    fn append_with(
        &mut self,
        event: ControlCommandEvent,
        write: impl FnOnce(&SecureDir, &[u8]) -> std::io::Result<()>,
    ) -> Result<DurableCommandReceipt, ControlCommandError> {
        self.ensure_usable()?;
        let mut projection = self.projection.clone();
        let command_id = event.command_id().clone();
        if let Some(receipt_revision) = apply(&self.data.owner, &mut projection, &event)? {
            let record = ControlCommandRecord {
                sequence: self.data.records.len() as u64 + 1,
                receipt_revision,
                event,
            };
            validate_record_size(&record)?;
            let mut next = self.data.clone();
            next.records.push(record);
            let bytes = serde_json::to_vec(&next)?;
            capacity(&next, &projection, bytes.len())?;
            if let Err(error) = write(&self.dir, &bytes) {
                self.poisoned = true;
                return Err(error.into());
            }
            self.data = next;
            self.projection = projection;
        }
        let view = self
            .projection
            .receipts
            .get(&command_id)
            .ok_or(ControlCommandError::NotFound)?;
        Ok(self.acknowledgement(view))
    }
}

fn apply(
    owner: &ControlCommandOwner,
    projection: &mut Projection,
    event: &ControlCommandEvent,
) -> Result<Option<u64>, ControlCommandError> {
    // Exact replay precedes all stale revision and transition checks.
    if let Some(previous) = projection.operations.get(event.operation_id()) {
        return if previous == event {
            Ok(None)
        } else {
            Err(ControlCommandError::CommandConflict)
        };
    }
    let revision = match event {
        ControlCommandEvent::Requested { request, source } => {
            validate_request(request)?;
            same_owner(owner, &request.session_id, &request.turn_id)?;
            validate_source(request, source)?;
            if projection.receipts.contains_key(&request.command_id) {
                return Err(ControlCommandError::CommandConflict);
            }
            projection.receipts.insert(
                request.command_id.clone(),
                CommandReceiptView {
                    request: (**request).clone(),
                    source: source.clone(),
                    revision: 1,
                    state: ControlCommandState::Requested,
                    last_transition: None,
                },
            );
            1
        }
        ControlCommandEvent::Transition { update } => {
            same_owner(owner, &update.session_id, &update.turn_id)?;
            serialized_bound(update, MAX_UPDATE_BYTES)?;
            if let ControlTransition::Rejected { failure } | ControlTransition::Failed { failure } =
                &update.transition
            {
                bounded_id(&failure.code)?;
                if failure.code.len() > 64
                    || failure.message.is_empty()
                    || failure.message.len() > 1024
                    || failure.message.contains('\0')
                {
                    return Err(ControlCommandError::Invalid("invalid failure description"));
                }
            }
            let view = projection
                .receipts
                .get_mut(&update.command_id)
                .ok_or(ControlCommandError::NotFound)?;
            if view.request.execution_epoch_id != update.execution_epoch_id {
                return Err(ControlCommandError::OwnerConflict);
            }
            if view.revision != update.expected_receipt_revision {
                return Err(ControlCommandError::StaleRevision {
                    expected: update.expected_receipt_revision,
                    actual: view.revision,
                });
            }
            let next = update.transition.state();
            if !matches!(
                (view.state, next),
                (
                    ControlCommandState::Requested,
                    ControlCommandState::Rejected
                ) | (
                    ControlCommandState::Requested,
                    ControlCommandState::Accepted
                ) | (ControlCommandState::Accepted, ControlCommandState::Applied)
                    | (ControlCommandState::Accepted, ControlCommandState::Failed)
                    | (ControlCommandState::Applied, ControlCommandState::Settled)
                    | (ControlCommandState::Applied, ControlCommandState::Failed)
            ) {
                return Err(ControlCommandError::InvalidTransition);
            }
            if let ControlTransition::Applied {
                turn_revision,
                graph_revision,
                ..
            } = &update.transition
            {
                if *turn_revision <= view.request.expected_turn_revision
                    || *graph_revision < view.request.expected_graph_revision
                {
                    return Err(ControlCommandError::Invalid(
                        "application revisions precede requested state",
                    ));
                }
            }
            view.state = next;
            view.last_transition = Some(update.transition.clone());
            view.revision += 1;
            view.revision
        }
    };
    projection
        .operations
        .insert(event.operation_id().clone(), event.clone());
    Ok(Some(revision))
}

fn rebuild(data: &CommandData) -> Result<Projection, ControlCommandError> {
    version(data.schema_version)?;
    bounded_id(&data.owner.workspace_id)?;
    uuid::Uuid::parse_str(&data.journal_id)
        .map_err(|_| ControlCommandError::Invalid("invalid journal identity"))?;
    if data.records.len() > MAX_RECORDS {
        return Err(ControlCommandError::Capacity);
    }
    let mut projection = Projection::default();
    for (index, record) in data.records.iter().enumerate() {
        validate_record_size(record)?;
        if record.sequence != index as u64 + 1
            || apply(&data.owner, &mut projection, &record.event)? != Some(record.receipt_revision)
        {
            return Err(ControlCommandError::Invalid(
                "record sequence or receipt revision",
            ));
        }
    }
    capacity(data, &projection, serde_json::to_vec(data)?.len())?;
    Ok(projection)
}

fn capacity(
    data: &CommandData,
    projection: &Projection,
    size: usize,
) -> Result<(), ControlCommandError> {
    let reserved: usize = projection
        .receipts
        .values()
        .map(|view| view.state.remaining_records())
        .sum();
    // Reserve the complete longest remaining lifecycle, including worst-case
    // escaped evidence/reasons, JSON record envelopes and separating commas.
    if projection.receipts.len() > MAX_COMMANDS
        || data.records.len() + reserved > MAX_RECORDS
        || size + reserved * (MAX_UPDATE_RECORD_BYTES + 1) > MAX_STORE_BYTES
    {
        return Err(ControlCommandError::Capacity);
    }
    Ok(())
}

fn validate_record_size(record: &ControlCommandRecord) -> Result<(), ControlCommandError> {
    let limit = match &record.event {
        ControlCommandEvent::Requested { .. } => MAX_REQUEST_RECORD_BYTES,
        ControlCommandEvent::Transition { .. } => MAX_UPDATE_RECORD_BYTES,
    };
    if serde_json::to_vec(record)?.len() > limit {
        return Err(ControlCommandError::Capacity);
    }
    Ok(())
}

fn version(value: u32) -> Result<(), ControlCommandError> {
    if value != CONTROL_COMMAND_SCHEMA_VERSION {
        return Err(ControlCommandError::UnsupportedVersion(value));
    }
    Ok(())
}

fn serialized_bound<T: Serialize>(value: &T, limit: usize) -> Result<(), ControlCommandError> {
    struct Counter {
        remaining: usize,
        exceeded: bool,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(std::io::Error::other("control-command byte bound"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        remaining: limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        return Err(ControlCommandError::Capacity);
    }
    result?;
    Ok(())
}

fn bounded_id(value: &str) -> Result<(), ControlCommandError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        return Err(ControlCommandError::Invalid("invalid bounded identity"));
    }
    Ok(())
}

fn same_owner(
    owner: &ControlCommandOwner,
    session: &SessionId,
    turn: &LogicalTurnId,
) -> Result<(), ControlCommandError> {
    if session != &owner.session_id || turn != &owner.turn_id {
        return Err(ControlCommandError::OwnerConflict);
    }
    Ok(())
}

fn target(
    request: &ControlCommandRequest,
    activation: &ActivationRef,
    epoch: Option<&ExecutionEpochId>,
) -> Result<(), ControlCommandError> {
    if activation.session_id != request.session_id
        || activation.turn_id != request.turn_id
        || epoch.is_some_and(|e| e != &activation.execution_epoch_id)
    {
        return Err(ControlCommandError::OwnerConflict);
    }
    if activation.generation == 0 {
        return Err(ControlCommandError::Invalid("zero activation generation"));
    }
    Ok(())
}

fn unique<T: Eq + std::hash::Hash>(values: &[T]) -> Result<(), ControlCommandError> {
    if values.len() > MAX_REFERENCES || values.iter().collect::<HashSet<_>>().len() != values.len()
    {
        return Err(ControlCommandError::Invalid(
            "duplicate or excessive references",
        ));
    }
    Ok(())
}

fn activation_list(
    request: &ControlCommandRequest,
    values: &[ActivationRef],
) -> Result<(), ControlCommandError> {
    if values.len() > MAX_REFERENCES {
        return Err(ControlCommandError::Capacity);
    }
    let mut seen = HashSet::new();
    for value in values {
        target(request, value, Some(&request.execution_epoch_id))?;
        if !seen.insert(&value.activation_id) {
            return Err(ControlCommandError::Invalid(
                "duplicate activation reference",
            ));
        }
    }
    Ok(())
}

fn input(
    request: &ControlCommandRequest,
    value: &ActivationInputManifest,
    epoch: &ExecutionEpochId,
) -> Result<(), ControlCommandError> {
    target(request, &value.activation, Some(epoch))?;
    unique(&value.guidance)?;
    unique(&value.attachments)?;
    if value.parents.len() > MAX_REFERENCES {
        return Err(ControlCommandError::Capacity);
    }
    let mut parents = HashSet::new();
    for parent in &value.parents {
        target(request, &parent.activation, None)?;
        if !parents.insert(&parent.activation.node_id)
            || parent.checkpoint.session_id != request.session_id
            || parent.checkpoint.source
                != (CheckpointSource::Accepted {
                    activation: parent.activation.clone(),
                })
        {
            return Err(ControlCommandError::Invalid(
                "invalid exact accepted parent",
            ));
        }
    }
    if let ConversationSavepoint::Checkpoint { checkpoint } = &value.starting_savepoint {
        if checkpoint.session_id != request.session_id
            || checkpoint.conversation_id != value.conversation_id
        {
            return Err(ControlCommandError::OwnerConflict);
        }
        if let CheckpointSource::Accepted { activation } = &checkpoint.source {
            target(request, activation, None)?;
            if activation.node_id != value.activation.node_id {
                return Err(ControlCommandError::OwnerConflict);
            }
        }
    }
    if value.grant.as_ref().is_some_and(|g| g.revision == 0) {
        return Err(ControlCommandError::Invalid("zero input grant revision"));
    }
    if let Some(context) = &value.revision_context {
        target(request, &context.activation, None)?;
        if context.activation.node_id != value.activation.node_id {
            return Err(ControlCommandError::OwnerConflict);
        }
    }
    Ok(())
}

fn retry(
    request: &ControlCommandRequest,
    previous: &ActivationRef,
    next: &ActivationInputManifest,
    epoch: &ExecutionEpochId,
) -> Result<(), ControlCommandError> {
    // A retained accepted or blocked target can predate the current epoch.
    // Canonical latest-generation eligibility belongs to the controller.
    target(request, previous, None)?;
    input(request, next, epoch)?;
    if previous.node_id != next.activation.node_id
        || previous.generation.checked_add(1) != Some(next.activation.generation)
        || previous.activation_id == next.activation.activation_id
    {
        return Err(ControlCommandError::Invalid(
            "retry must allocate exact next generation",
        ));
    }
    Ok(())
}

fn decisions(
    request: &ControlCommandRequest,
    values: &[InvocationDecisionRef],
    previous: Option<&ActivationRef>,
) -> Result<(), ControlCommandError> {
    if values.len() > MAX_REFERENCES {
        return Err(ControlCommandError::Capacity);
    }
    let mut seen = HashSet::new();
    for value in values {
        target(request, &value.activation, None)?;
        if previous.is_some_and(|p| p != &value.activation) || !seen.insert(&value.invocation_id) {
            return Err(ControlCommandError::Invalid(
                "mismatched or duplicate replay decision",
            ));
        }
    }
    Ok(())
}

fn validate_request(request: &ControlCommandRequest) -> Result<(), ControlCommandError> {
    version(request.schema_version)?;
    serialized_bound(request, MAX_CONTROL_REQUEST_BYTES)?;
    let epoch = &request.execution_epoch_id;
    match &request.parameters {
        ControlParameters::StopActivation { activation }
        | ControlParameters::SteerActivation { activation, .. }
        | ControlParameters::ResumeBlocked { activation, .. } => {
            target(request, activation, Some(epoch))?
        }
        ControlParameters::RetryActivation {
            activation,
            input,
            replay_decisions,
        } => {
            target(request, activation, Some(epoch))?;
            retry(request, activation, input, epoch)?;
            decisions(request, replay_decisions, Some(activation))?;
        }
        ControlParameters::ReviseActivation {
            activation,
            input,
            invalidate,
            ..
        } => {
            retry(request, activation, input, epoch)?;
            if invalidate.len() > MAX_REFERENCES {
                return Err(ControlCommandError::Capacity);
            }
            let mut seen = HashSet::new();
            for invalidated in invalidate {
                target(request, invalidated, None)?;
                if !seen.insert(&invalidated.activation_id) {
                    return Err(ControlCommandError::Invalid(
                        "duplicate invalidation target",
                    ));
                }
            }
        }
        ControlParameters::ContinueTurn {
            plan,
            replay_decisions,
        } => {
            if &plan.source_epoch_id != epoch || plan.epoch_id == plan.source_epoch_id {
                return Err(ControlCommandError::OwnerConflict);
            }
            unique(&plan.condition_runs)?;
            if plan.condition_runs.len() > MAX_COMPLETION_CONDITIONS {
                return Err(ControlCommandError::Capacity);
            }
            if plan.selections.len() > MAX_REFERENCES {
                return Err(ControlCommandError::Capacity);
            }
            let mut nodes = HashSet::new();
            for selection in &plan.selections {
                let node_id = match selection {
                    ContinuationSelection::RetainAccepted { activation }
                    | ContinuationSelection::LeaveBlocked { activation, .. } => {
                        target(request, activation, None)?;
                        &activation.node_id
                    }
                    ContinuationSelection::Retry { previous, input } => {
                        retry(request, previous, input, &plan.epoch_id)?;
                        &previous.node_id
                    }
                    ContinuationSelection::Rebase {
                        previous,
                        input: next,
                    } => {
                        retry(request, previous, next, &plan.epoch_id)?;
                        &previous.node_id
                    }
                    ContinuationSelection::Revise {
                        previous,
                        input: next,
                        invalidated_descendants,
                        evidence,
                    } => {
                        retry(request, previous, next, &plan.epoch_id)?;
                        if invalidated_descendants.len() > MAX_REFERENCES
                            || !next.guidance.contains(evidence)
                        {
                            return Err(ControlCommandError::Invalid(
                                "revision continuation has invalid captured context",
                            ));
                        }
                        let mut seen = HashSet::new();
                        for invalidated in invalidated_descendants {
                            target(request, invalidated, None)?;
                            if !seen.insert(&invalidated.activation_id) {
                                return Err(ControlCommandError::Invalid(
                                    "duplicate invalidation target",
                                ));
                            }
                        }
                        &previous.node_id
                    }
                    ContinuationSelection::PrepareUnmaterialized { input: next } => {
                        input(request, next, &plan.epoch_id)?;
                        if next.activation.generation != 1 {
                            return Err(ControlCommandError::Invalid(
                                "new continuation node must begin at generation one",
                            ));
                        }
                        &next.activation.node_id
                    }
                    ContinuationSelection::AwaitDependencies { node_id }
                    | ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. } => node_id,
                };
                if !nodes.insert(node_id) {
                    return Err(ControlCommandError::Invalid("duplicate continuation node"));
                }
            }
            decisions(request, replay_decisions, None)?;
        }
        ControlParameters::AddAgent {
            input: manifest,
            dependencies,
        } => {
            input(request, manifest, epoch)?;
            unique(dependencies)?;
            if manifest.activation.generation != 1
                || dependencies.contains(&manifest.activation.node_id)
            {
                return Err(ControlCommandError::Invalid("invalid new agent target"));
            }
        }
        ControlParameters::ReplaceFutureAgent {
            target,
            input: manifest,
            rewire_dependents,
        } => {
            input(request, manifest, epoch)?;
            unique(rewire_dependents)?;
            if manifest.activation.generation != 1
                || target == &manifest.activation.node_id
                || rewire_dependents.contains(target)
                || rewire_dependents.contains(&manifest.activation.node_id)
            {
                return Err(ControlCommandError::Invalid(
                    "invalid future replacement target",
                ));
            }
        }
        ControlParameters::FinishTurn {
            mode:
                FinishMode::ForcePartial {
                    missing_conditions,
                    missing_condition_ids,
                    stop_activations,
                    selected_activations,
                    ..
                },
        } => {
            unique(missing_conditions)?;
            unique(missing_condition_ids)?;
            activation_list(request, stop_activations)?;
            if selected_activations.len() > MAX_REFERENCES {
                return Err(ControlCommandError::Capacity);
            }
            let mut selected = HashSet::new();
            for activation in selected_activations {
                target(request, activation, None)?;
                if !selected.insert(&activation.activation_id) {
                    return Err(ControlCommandError::Invalid(
                        "duplicate partial Finish selection",
                    ));
                }
            }
        }
        ControlParameters::FinishTurn {
            mode: FinishMode::Normal,
        } => {}
    }
    Ok(())
}

fn validate_source(
    request: &ControlCommandRequest,
    source: &CommandSourceRecord,
) -> Result<(), ControlCommandError> {
    match source {
        CommandSourceRecord::Human {
            session_id,
            turn_id,
            ..
        } => {
            if session_id != &request.session_id || turn_id != &request.turn_id {
                return Err(ControlCommandError::OwnerConflict);
            }
        }
        CommandSourceRecord::Agent {
            activation,
            grant_id,
            grant_revision,
            live_scope,
            ..
        } => {
            target(request, activation, Some(&request.execution_epoch_id))?;
            GrantId::new(grant_id.clone())
                .map_err(|_| ControlCommandError::Invalid("invalid source grant"))?;
            bounded_id(live_scope)?;
            if *grant_revision == 0 {
                return Err(ControlCommandError::Invalid("zero source grant revision"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Scenario {
        request: ControlCommandRequest,
        updates: Vec<ControlReceiptUpdate>,
    }

    fn scenario() -> Scenario {
        serde_json::from_str(include_str!(
            "../tests/fixtures/control-command-stop.v1.json"
        ))
        .unwrap()
    }

    fn owner() -> ControlCommandOwner {
        let request = scenario().request;
        ControlCommandOwner {
            workspace_id: "workspace-client-a".into(),
            session_id: request.session_id,
            turn_id: request.turn_id,
        }
    }

    fn requested(request: ControlCommandRequest) -> ControlCommandEvent {
        ControlCommandEvent::Requested {
            source: CommandSourceRecord::Human {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                request_evidence: EvidenceRef::new("authenticated-human-action").unwrap(),
            },
            request: Box::new(request),
        }
    }

    #[test]
    fn every_uncertain_publication_requires_reopen_before_any_acknowledgement() {
        let fixture = scenario();
        let mut events = vec![requested(fixture.request.clone())];
        events.extend(
            fixture
                .updates
                .iter()
                .cloned()
                .map(|update| ControlCommandEvent::Transition { update }),
        );
        for cut in 0..events.len() {
            let root = tempfile::TempDir::new().unwrap();
            let mut store = ControlCommandStore::open(root.path(), owner()).unwrap();
            for event in &events[..cut] {
                store.append(event.clone()).unwrap();
            }
            let error = store.append_with(events[cut].clone(), |dir, bytes| {
                dir.atomic_write(FILE, bytes)?;
                Err(std::io::Error::other(
                    "simulated lost durability acknowledgement",
                ))
            });
            assert!(matches!(error, Err(ControlCommandError::Io(_))));
            assert!(matches!(
                store.records(),
                Err(ControlCommandError::RecoveryRequired)
            ));
            assert!(matches!(
                store.append(events[cut].clone()),
                Err(ControlCommandError::RecoveryRequired)
            ));
            assert!(matches!(
                store.lookup_request(&fixture.request),
                Err(ControlCommandError::RecoveryRequired)
            ));
            drop(store);
            let mut reopened = ControlCommandStore::open(root.path(), owner()).unwrap();
            let recovered = reopened.lookup_request(&fixture.request).unwrap().unwrap();
            assert_eq!(recovered.view().revision, cut as u64 + 1);
            assert_eq!(reopened.records().unwrap().len(), cut + 1);
            assert_eq!(
                reopened.append(events[cut].clone()).unwrap().view(),
                recovered.view()
            );
            assert_eq!(reopened.records().unwrap().len(), cut + 1);
        }
    }

    fn append_memory(
        data: &mut CommandData,
        projection: &mut Projection,
        event: ControlCommandEvent,
    ) -> Result<(), ControlCommandError> {
        let mut next_projection = projection.clone();
        let Some(revision) = apply(&data.owner, &mut next_projection, &event)? else {
            return Ok(());
        };
        let record = ControlCommandRecord {
            sequence: data.records.len() as u64 + 1,
            receipt_revision: revision,
            event,
        };
        validate_record_size(&record)?;
        let mut next = data.clone();
        next.records.push(record);
        capacity(&next, &next_projection, serde_json::to_vec(&next)?.len())?;
        *data = next;
        *projection = next_projection;
        Ok(())
    }

    fn empty_data() -> CommandData {
        CommandData {
            schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
            journal_id: uuid::Uuid::new_v4().to_string(),
            owner: owner(),
            records: vec![],
            canonical_journal_id: None,
        }
    }

    #[test]
    fn receipt_count_bound_preserves_all_identities_and_their_future_lifecycles() {
        let mut data = empty_data();
        let mut projection = Projection::default();
        for index in 0..MAX_COMMANDS {
            let mut request = scenario().request;
            request.command_id = CommandId::new(format!("stop-{index}")).unwrap();
            append_memory(&mut data, &mut projection, requested(request)).unwrap();
        }
        let bytes = serde_json::to_vec(&data).unwrap();
        let event = requested(scenario().request);
        assert!(matches!(
            append_memory(&mut data, &mut projection, event),
            Err(ControlCommandError::Capacity)
        ));
        assert_eq!(serde_json::to_vec(&data).unwrap(), bytes);
        let first = data.records[0].event.clone();
        append_memory(&mut data, &mut projection, first).unwrap();
        assert_eq!(data.records.len(), MAX_COMMANDS);
        assert_eq!(projection.receipts.len(), MAX_COMMANDS);
        assert_eq!(rebuild(&data).unwrap().receipts.len(), MAX_COMMANDS);
    }

    #[test]
    fn byte_capacity_reserves_complete_remaining_lifecycle_including_escaped_failure() {
        let mut data = empty_data();
        let mut projection = Projection::default();
        let mut admitted = vec![];
        for index in 0..MAX_COMMANDS {
            let mut request = scenario().request;
            request.command_id = CommandId::new(format!("finish-{index}")).unwrap();
            let ControlParameters::StopActivation { activation } = &request.parameters else {
                unreachable!()
            };
            let stop_activations = (0..MAX_REFERENCES)
                .map(|n| {
                    let mut activation = activation.clone();
                    activation.activation_id = crate::turn_contract::ActivationId::new(format!(
                        "activation-{n}-{}",
                        "a".repeat(90)
                    ))
                    .unwrap();
                    activation.node_id =
                        TurnNodeId::new(format!("node-{n}-{}", "n".repeat(100))).unwrap();
                    activation
                })
                .collect();
            request.parameters = ControlParameters::FinishTurn {
                mode: FinishMode::ForcePartial {
                    approval: EvidenceRef::new("exact-human-partial-finish").unwrap(),
                    missing_conditions: (0..MAX_REFERENCES)
                        .map(|n| {
                            EvidenceRef::new(format!("condition-{n}-{}", "c".repeat(100))).unwrap()
                        })
                        .collect(),
                    stop_activations,
                    missing_condition_ids: vec![],
                    selected_activations: vec![],
                },
            };
            match append_memory(&mut data, &mut projection, requested(request.clone())) {
                Ok(()) => admitted.push(request),
                Err(ControlCommandError::Capacity) => break,
                Err(error) => panic!("unexpected admission error: {error}"),
            }
        }
        assert!(!admitted.is_empty() && admitted.len() < MAX_COMMANDS);
        for request in &admitted {
            let evidence = || EvidenceRef::new("e".repeat(128)).unwrap();
            let transitions = [
                ControlTransition::Accepted {
                    validation: evidence(),
                    pending: evidence(),
                },
                ControlTransition::Applied {
                    state_transition: evidence(),
                    turn_revision: 8,
                    graph_revision: 2,
                    pending: evidence(),
                },
                ControlTransition::Failed {
                    failure: CommandFailure {
                        code: "f".repeat(64),
                        message: "\u{1}".repeat(1024),
                        evidence: Some(evidence()),
                        blocker: Some(evidence()),
                    },
                },
            ];
            for (index, transition) in transitions.into_iter().enumerate() {
                let update = ControlReceiptUpdate {
                    update_id: CommandId::new(format!(
                        "{}-stage-{index}",
                        request.command_id.as_str()
                    ))
                    .unwrap(),
                    command_id: request.command_id.clone(),
                    session_id: request.session_id.clone(),
                    turn_id: request.turn_id.clone(),
                    execution_epoch_id: request.execution_epoch_id.clone(),
                    expected_receipt_revision: index as u64 + 1,
                    transition,
                };
                append_memory(
                    &mut data,
                    &mut projection,
                    ControlCommandEvent::Transition { update },
                )
                .unwrap();
            }
        }
        assert_eq!(projection.receipts.len(), admitted.len());
        assert_eq!(data.records.len(), admitted.len() * 4);
        assert!(projection
            .receipts
            .values()
            .all(|receipt| receipt.state == ControlCommandState::Failed));
        assert_eq!(rebuild(&data).unwrap().receipts.len(), admitted.len());
    }
}

#[cfg(all(test, unix))]
mod historical_read_tests {
    use super::*;
    use crate::execution_content::{ExecutionContentStore, ExecutionRequestContent};
    use crate::execution_ownership::LegacyFormatOwnership;
    use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use crate::turn_contract::{
        TurnClosure, TurnContractEnvelope, TurnContractEvent, TURN_CONTRACT_SCHEMA_VERSION,
    };
    use std::sync::Arc;

    #[test]
    fn historical_command_views_preserve_pending_receipts_without_recovery_or_authority() {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
        )).unwrap();
        let begin: TurnContractEnvelope =
            serde_json::from_value(source["steps"][0]["envelope"].clone()).unwrap();
        let TurnContractEvent::Begin {
            epoch_id, graph, ..
        } = &begin.event
        else {
            panic!("Begin fixture")
        };
        let root = tempfile::tempdir().unwrap();
        let mut canonical = SessionExecutionStore::open(
            Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            ),
            ExecutionStoreOwner {
                workspace_id: "history-read-workspace".into(),
                session_id: begin.session_id.clone(),
            },
        )
        .unwrap();
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let request = content
            .retain_request(ExecutionRequestContent {
                turn_id: begin.turn_id.clone(),
                recorded_at_unix_ms: 1,
                display_input: "Retained historical request".into(),
                effective_input: "Retained historical request".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        canonical
            .begin_with_request(begin.clone(), &request)
            .unwrap();
        let mut commands = ControlCommandStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ControlCommands {
                    turn_id: begin.turn_id.clone(),
                })
                .unwrap(),
        )
        .unwrap();
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/control-command-stop.v1.json"
        ))
        .unwrap();
        let mut request: ControlCommandRequest =
            serde_json::from_value(source["request"].clone()).unwrap();
        request.session_id = begin.session_id.clone();
        request.turn_id = begin.turn_id.clone();
        request.execution_epoch_id = epoch_id.clone();
        request.expected_turn_revision = 1;
        request.expected_graph_revision = graph.revision;
        let ControlParameters::StopActivation { activation } = &mut request.parameters else {
            panic!("Stop fixture")
        };
        activation.session_id = begin.session_id.clone();
        activation.turn_id = begin.turn_id.clone();
        activation.execution_epoch_id = epoch_id.clone();
        activation.node_id = graph.nodes[0].node_id.clone();
        let receipt = commands
            .record_requested(
                request,
                TrustedCommandSource::human(
                    begin.session_id.clone(),
                    begin.turn_id.clone(),
                    EvidenceRef::new("retained-host-request").unwrap(),
                ),
            )
            .unwrap();
        assert_eq!(receipt.view().state, ControlCommandState::Requested);
        assert!(ControlCommandStore::read_historical_views(&canonical, &begin.turn_id).is_err());
        canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("historical-close").unwrap(),
                expected_revision: 1,
                session_id: begin.session_id.clone(),
                turn_id: begin.turn_id.clone(),
                event: TurnContractEvent::Close {
                    closure: TurnClosure::Cancelled,
                },
            })
            .unwrap();
        let canonical_before = std::fs::read(canonical.path()).unwrap();
        let command_before = std::fs::read(commands.path()).unwrap();
        let views = ControlCommandStore::read_historical_views(&canonical, &begin.turn_id).unwrap();
        assert_eq!(views, vec![receipt.view().clone()]);
        assert_eq!(views[0].state, ControlCommandState::Requested);
        assert_eq!(std::fs::read(canonical.path()).unwrap(), canonical_before);
        assert_eq!(std::fs::read(commands.path()).unwrap(), command_before);
        assert_eq!(commands.records().unwrap().len(), 1);
    }
}
