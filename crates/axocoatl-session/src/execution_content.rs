//! Retained execution content, separate from lifecycle and dispatch authority.
//!
//! A content receipt proves an immutable body was acknowledged by storage. Only
//! the canonical Session journal can bind a request to Begin or accept an output.
//! Projections resolve those exact references; a missing body remains missing.
//! Tool reservations retain protected arguments and reserve a bounded terminal
//! result before dispatch. An oversized result retains an explicitly truncated
//! prefix, never an invented complete result. These receipts do not permit replay.
//! Repository checks likewise reserve terminal evidence before a canonical
//! condition intent. Their retained argv and repository descriptions grant no
//! execution authority; observed transport output is distinct from unseen bytes.
//!
//! `open_owned` retains the canonical format and Session locks. The standalone
//! opener exists for migration tooling and isolated stores; its caller must hold
//! the corresponding execution owner for the entire store lifetime.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;

use axocoatl_core::{SecureDir, TokenUsageStats};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
use crate::execution_legacy::{LegacyHistorySource, OwnedLegacyHistorySnapshot};
use crate::execution_namespace::{
    check_journal_creation, mark_journal_initialized, ExecutionComponent, OwnedExecutionNamespace,
};
use crate::execution_store::{
    DurableLegacySeal, DurableSessionIdentity, DurableTurnSnapshot, ExecutionStoreOwner,
    SessionExecutionStore,
};
use crate::invocation_audit::ProtectedArguments;
use crate::turn_contract::{
    ActivationGuidance, ActivationInputManifest, ActivationRef, ActivationState, AgentDefinitionId,
    CheckpointRef, ConditionKind, ConditionRunId, ConditionRunRef, ContractActivation, EpochState,
    EvidenceRef, ExecutionEpoch, InvocationId, InvocationOutcome, LogicalTurnId, LogicalTurnState,
    RepositoryInput, MAX_CONTRACT_NODES,
};
use crate::turn_ledger::{SessionTurn, SessionTurnContextReference, SessionTurnLifecycle};

#[path = "execution_content_stream.rs"]
mod stream;
pub use stream::{ActivationStreamContent, ActivationStreamPayload, ActivationStreamView};

#[path = "execution_content_provider.rs"]
mod provider;
pub use provider::RetainedProviderProfile;
#[path = "execution_content_turn_admission.rs"]
mod turn_admission;
pub use turn_admission::{TurnAdmissionContent, TurnAdmissionNodeInput};

#[path = "execution_content_attachment.rs"]
mod attachment;
#[path = "execution_content_repository.rs"]
mod repository_snapshot;
pub use attachment::RetainedBinaryAttachment;
use repository_snapshot::StandingRepositoryCheck;
pub use repository_snapshot::{
    is_capture_baseline, ActivationRepositorySnapshot, ActivationRepositorySnapshotView,
    RepositoryComparison, RepositorySnapshotPhase, MAX_COMPARED_PATH_BYTES,
    REPOSITORY_CAPTURE_SCRIPT, REPOSITORY_SNAPSHOT_COMMAND, REPOSITORY_SNAPSHOT_COMMAND_V1,
};

#[path = "execution_content_reattachment.rs"]
mod reattachment;
pub use reattachment::{RepositoryReattachment, RepositoryReattachmentView};

const SCHEMA: u32 = 1;
const FILE: &str = "execution-content.v1.json";
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 4096;
const MAX_TEXT: usize = 1024 * 1024;
const MAX_CONTEXTS: usize = 128;
const MAX_CONTEXT_BYTES: usize = 256 * 1024;
const MAX_TOOL_BYTES: usize = 1024 * 1024;
const MAX_LEGACY_TURNS: usize = 2048;
const RESULT_OVERHEAD: usize = 8192;
const MAX_PARTIAL_OUTPUT_RECORDS: u32 = 32;
const OUTPUT_OVERHEAD: usize = 8192;
const MAX_CHECK_ARGUMENTS: usize = 128;
const MAX_CHECK_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_CHECK_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Exact executable data, never permission to run a process. The host binds the
/// actual repository capability, isolation, grant and cleanup before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryCheckDefinition {
    #[serde(deserialize_with = "bounded_check_argv")]
    pub argv: Vec<String>,
    pub timeout_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionAcceptedInput {
    pub input: ActivationInputManifest,
    pub checkpoint: CheckpointRef,
    pub output: EvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionArguments {
    run: ConditionRunRef,
    definition_ref: EvidenceRef,
    definition: RepositoryCheckDefinition,
    repository_ref: EvidenceRef,
    #[serde(deserialize_with = "bounded_condition_inputs")]
    inputs: Vec<ConditionAcceptedInput>,
}

/// Acknowledged immutable arguments and terminal capacity. Restoring this
/// receipt cannot authorize execution or prove the run was never dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableConditionArguments {
    identity: DurableSessionIdentity,
    protected: ProtectedArguments,
    arguments: ConditionArguments,
}

impl DurableConditionArguments {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.protected.evidence_ref
    }
    pub fn run(&self) -> &ConditionRunRef {
        &self.arguments.run
    }
    pub fn definition_ref(&self) -> &EvidenceRef {
        &self.arguments.definition_ref
    }
    pub fn definition(&self) -> &RepositoryCheckDefinition {
        &self.arguments.definition
    }
    pub fn repository_ref(&self) -> &EvidenceRef {
        &self.arguments.repository_ref
    }
    pub fn inputs(&self) -> &[ConditionAcceptedInput] {
        &self.arguments.inputs
    }
    pub fn protected_arguments(&self) -> &ProtectedArguments {
        &self.protected
    }
}

/// Observed process disposition. Timeout/interruption alone does not establish
/// that descendants stopped or that side effects were rolled back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConditionProcessStatus {
    NotDispatched,
    Exited { code: i32 },
    Signalled { signal: i32 },
    TimedOut,
    Interrupted,
    LaunchFailed { message: String },
    Uncertain { message: String },
}

/// Retained observations from the host-owned supervisor transport. This is
/// audit evidence, never an executable capability or permission to unlock a
/// live repository; that requires the isolation layer's opaque receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionSupervisionEvidence {
    pub invocation_id: String,
    pub request_sha256: String,
    pub runtime_identity: String,
    pub program_sha256: String,
    pub transport_identity: String,
    pub launched: bool,
    pub quiescent: bool,
    pub primary_exit: Option<ConditionProcessStatus>,
}

/// A bounded prefix plus a digest of bytes actually observed. `complete` means
/// the host observed EOF without upstream loss. A previously truncated sandbox
/// response must be marked incomplete; its digest cannot describe unseen bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionOutputEvidence {
    retained_hex: String,
    retained_sha256: String,
    observed_sha256: String,
    observed_byte_len: u64,
    complete: bool,
    source: ConditionOutputSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOutputSource {
    DirectCapture,
    ObservedTransport,
}

impl ConditionOutputEvidence {
    pub fn retained_bytes(&self) -> Result<Vec<u8>, ExecutionContentError> {
        unhex(&self.retained_hex)
    }
    pub fn retained_byte_len(&self) -> usize {
        self.retained_hex.len() / 2
    }
    pub fn observed_byte_len(&self) -> u64 {
        self.observed_byte_len
    }
    pub fn observed_sha256(&self) -> &str {
        &self.observed_sha256
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn source(&self) -> ConditionOutputSource {
        self.source
    }
    pub fn is_truncated(&self) -> bool {
        self.observed_byte_len > self.retained_byte_len() as u64 || !self.complete
    }
    /// Accepts a host's raw transport observation. The supplied digest covers
    /// observed bytes only; this does not authenticate an arbitrary caller's
    /// claim about a process, unseen output, completion or repository authority.
    pub fn from_observed_transport(
        retained: &[u8],
        observed_byte_len: u64,
        observed_sha256: String,
        complete: bool,
    ) -> Result<Self, ExecutionContentError> {
        if retained.len() > MAX_TOOL_BYTES {
            return Err(ExecutionContentError::Capacity);
        }
        let evidence = Self {
            retained_hex: hex(retained),
            retained_sha256: sha256(retained),
            observed_sha256,
            observed_byte_len,
            complete,
            source: ConditionOutputSource::ObservedTransport,
        };
        validate_condition_output(&evidence)?;
        Ok(evidence)
    }
}

/// Streaming capture hashes all observed bytes without buffering them. The
/// caller must keep draining or report incomplete evidence on read failure.
pub struct ConditionOutputCapture {
    retained: Vec<u8>,
    capacity: usize,
    observed: u64,
    digest: Sha256,
}

impl ConditionOutputCapture {
    pub fn new(capacity: usize) -> Result<Self, ExecutionContentError> {
        if capacity > MAX_TOOL_BYTES {
            return Err(ExecutionContentError::Capacity);
        }
        Ok(Self {
            retained: Vec::new(),
            capacity,
            observed: 0,
            digest: Sha256::new(),
        })
    }
    pub fn observe(&mut self, bytes: &[u8]) -> Result<(), ExecutionContentError> {
        let observed = self
            .observed
            .checked_add(bytes.len() as u64)
            .ok_or(ExecutionContentError::Capacity)?;
        let remaining = self.capacity.saturating_sub(self.retained.len());
        self.retained
            .extend_from_slice(&bytes[..remaining.min(bytes.len())]);
        self.digest.update(bytes);
        self.observed = observed;
        Ok(())
    }
    pub fn finish(self, complete: bool) -> ConditionOutputEvidence {
        ConditionOutputEvidence {
            retained_hex: hex(&self.retained),
            retained_sha256: sha256(&self.retained),
            observed_sha256: format!("{:x}", self.digest.finalize()),
            observed_byte_len: self.observed,
            complete,
            source: ConditionOutputSource::DirectCapture,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionResult {
    reservation_ref: EvidenceRef,
    status: ConditionProcessStatus,
    stdout: ConditionOutputEvidence,
    stderr: ConditionOutputEvidence,
    recorded_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supervision: Option<ConditionSupervisionEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableConditionResult {
    arguments: DurableConditionArguments,
    protected: ProtectedArguments,
    result: ConditionResult,
}

impl DurableConditionResult {
    pub fn supervision(&self) -> Option<&ConditionSupervisionEvidence> {
        self.result.supervision.as_ref()
    }
    pub fn arguments(&self) -> &DurableConditionArguments {
        &self.arguments
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.protected.evidence_ref
    }
    pub fn protected_result(&self) -> &ProtectedArguments {
        &self.protected
    }
    pub fn status(&self) -> &ConditionProcessStatus {
        &self.result.status
    }
    pub fn stdout(&self) -> &ConditionOutputEvidence {
        &self.result.stdout
    }
    pub fn stderr(&self) -> &ConditionOutputEvidence {
        &self.result.stderr
    }
    pub fn recorded_at_unix_ms(&self) -> u64 {
        self.result.recorded_at_unix_ms
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionContentError {
    #[error("execution content I/O: {0}")]
    Io(#[from] io::Error),
    #[error("execution content serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid execution content: {0}")]
    Invalid(&'static str),
    #[error("execution content belongs to another canonical journal or activation")]
    OwnerMismatch,
    #[error("immutable execution content conflicts with retained evidence")]
    Conflict,
    #[error("execution content admission would consume reserved settlement capacity")]
    Capacity,
    #[error("execution content write is uncertain; reopen before further use")]
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionModelRef {
    pub provider_id: String,
    pub model_id: String,
    pub configuration_ref: EvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequestContent {
    pub turn_id: LogicalTurnId,
    pub recorded_at_unix_ms: u64,
    pub display_input: String,
    pub effective_input: String,
    #[serde(deserialize_with = "bounded_contexts")]
    pub context: Vec<SessionTurnContextReference>,
    pub target_definition: Option<AgentDefinitionId>,
    pub model: Option<ExecutionModelRef>,
}

/// Unknown usage preserves only the known subtotal. Neither variant is inferred
/// from text length or from other partial output records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionUsage {
    Measured { usage: TokenUsageStats },
    Unknown { known_subtotal: TokenUsageStats },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Partial,
    Final,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationOutputContent {
    pub activation: ActivationRef,
    pub recorded_at_unix_ms: u64,
    pub text: String,
    pub usage: ExecutionUsage,
    pub kind: OutputKind,
}

/// Space promised before provider work. Streamed partial evidence is optional;
/// terminal settlement always has a separate slot, including failed/Stopped
/// runs whose only truthful output is partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationOutputLimits {
    pub partial_records: u32,
    pub partial_bytes: usize,
    pub settlement_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActivationOutputSlot {
    Partial { sequence: u32 },
    Settlement,
}

/// Immutable bounded evidence. `output.text` may be a UTF-8 prefix; the full
/// observed text digest/length and exact usage are retained independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReservedActivationOutputContent {
    pub reservation_ref: EvidenceRef,
    pub slot: ActivationOutputSlot,
    pub output: ActivationOutputContent,
    pub original_byte_len: u64,
    pub original_sha256: String,
}

impl ReservedActivationOutputContent {
    pub fn is_truncated(&self) -> bool {
        self.original_byte_len > self.output.text.len() as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReservedActivationOutputView {
    pub reference: EvidenceRef,
    pub content: ReservedActivationOutputContent,
}

/// Acknowledged capacity, never permission to start/replay provider work. The
/// host must validate the current canonical activation and provider authority
/// under its own admission gate. Loaded receipts carry no live execution lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableActivationOutputReservation {
    identity: DurableSessionIdentity,
    reference: EvidenceRef,
    activation: ActivationRef,
    limits: ActivationOutputLimits,
}

impl DurableActivationOutputReservation {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }
    pub fn activation(&self) -> &ActivationRef {
        &self.activation
    }
    pub fn limits(&self) -> ActivationOutputLimits {
        self.limits
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableReservedExecutionOutput {
    identity: DurableSessionIdentity,
    reference: EvidenceRef,
    content: ReservedActivationOutputContent,
}

impl DurableReservedExecutionOutput {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }
    pub fn content(&self) -> &ReservedActivationOutputContent {
        &self.content
    }

    /// Only a complete final body can be proposed for canonical acceptance.
    /// This receipt itself still does not accept or promote a checkpoint.
    pub fn complete_output(&self) -> Option<DurableExecutionOutput> {
        (self.content.slot == ActivationOutputSlot::Settlement
            && self.content.output.kind == OutputKind::Final
            && !self.content.is_truncated())
        .then(|| DurableExecutionOutput {
            identity: self.identity.clone(),
            activation: self.content.output.activation.clone(),
            reference: self.reference.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationOutputReservation {
    activation: ActivationRef,
    limits: ActivationOutputLimits,
}

/// Retained semantic inputs. Definition configuration is the exact serialized
/// configuration consumed by the host, not a mutable registry lookup. Repository
/// descriptions require a separate host check of the actual checkout resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActivationEvidenceContent {
    Definition {
        definition_id: AgentDefinitionId,
        revision: u64,
        profile: ExecutionProfile,
        configuration: String,
    },
    Guidance {
        text: String,
    },
    Attachment {
        reference_id: String,
        media_type: String,
        text: String,
    },
    BinaryAttachment {
        attachment: RetainedBinaryAttachment,
    },
    Repository {
        description: String,
        revision: Option<String>,
    },
    Budget {
        limits: GrantLimits,
    },
    Grant {
        policy: AuthorityGrant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableActivationEvidence {
    identity: DurableSessionIdentity,
    reference: EvidenceRef,
}
impl DurableActivationEvidence {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }
}

/// All semantic bodies resolve from this journal. This is not a dispatch grant:
/// the host verifies current grantbook, profile/configuration and resource state;
/// memory independently verifies exact starting and parent checkpoint bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedActivationInput {
    pub definition: ActivationEvidenceContent,
    pub grant: Option<AuthorityGrant>,
    pub budget: GrantLimits,
    pub guidance: Vec<String>,
    pub attachments: Vec<ActivationEvidenceContent>,
    pub repository: Option<ActivationEvidenceContent>,
    pub parents: Vec<ActivationOutputContent>,
    pub revision_context: Option<ActivationOutputContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableExecutionRequest {
    identity: DurableSessionIdentity,
    turn_id: LogicalTurnId,
    reference: EvidenceRef,
}
impl DurableExecutionRequest {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn turn_id(&self) -> &LogicalTurnId {
        &self.turn_id
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableExecutionOutput {
    identity: DurableSessionIdentity,
    activation: ActivationRef,
    reference: EvidenceRef,
}
impl DurableExecutionOutput {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn activation(&self) -> &ActivationRef {
        &self.activation
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyTurnPredecessor {
    pub turn_id: String,
    pub status: SessionTurnLifecycle,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A supported v1 ledger projection, including rows hidden by rewind. Empty is
/// meaningful only when read from the actual owned legacy ledger. The host must
/// reject missing history paired with checkpoint caches before sealing migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyHistoryFrontier {
    pub source: LegacyHistorySource,
    #[serde(deserialize_with = "bounded_legacy_turns")]
    pub turns: Vec<SessionTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableLegacyHistory {
    identity: DurableSessionIdentity,
    reference: EvidenceRef,
    last_predecessor: Option<LegacyTurnPredecessor>,
    source: LegacyHistorySource,
}
impl DurableLegacyHistory {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }
    pub fn last_predecessor(&self) -> Option<&LegacyTurnPredecessor> {
        self.last_predecessor.as_ref()
    }
    pub fn source(&self) -> &LegacyHistorySource {
        &self.source
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolReservation {
    activation: ActivationRef,
    invocation_id: InvocationId,
    arguments_hex: String,
    arguments_sha256: String,
    result_capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolResult {
    reservation_ref: EvidenceRef,
    recorded_at_unix_ms: u64,
    retained_hex: String,
    retained_sha256: String,
    outcome: InvocationOutcome,
    original_sha256: String,
    original_byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableToolArguments {
    identity: DurableSessionIdentity,
    activation: ActivationRef,
    invocation_id: InvocationId,
    protected: ProtectedArguments,
    result_capacity: usize,
}
impl DurableToolArguments {
    pub fn journal_id(&self) -> &str {
        self.identity.journal_id()
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        self.identity.owner()
    }
    pub fn activation(&self) -> &ActivationRef {
        &self.activation
    }
    pub fn invocation_id(&self) -> &InvocationId {
        &self.invocation_id
    }
    pub fn protected_arguments(&self) -> &ProtectedArguments {
        &self.protected
    }
    pub fn result_capacity(&self) -> usize {
        self.result_capacity
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableToolResult {
    arguments: DurableToolArguments,
    protected: ProtectedArguments,
    outcome: InvocationOutcome,
    original_sha256: String,
    original_byte_len: u64,
    recorded_at_unix_ms: u64,
}
impl DurableToolResult {
    pub fn arguments(&self) -> &DurableToolArguments {
        &self.arguments
    }
    pub fn protected_result(&self) -> &ProtectedArguments {
        &self.protected
    }
    pub fn original_byte_len(&self) -> u64 {
        self.original_byte_len
    }
    /// The backend's observed return status survives payload truncation. A
    /// failed return does not establish rollback or absence of external effects.
    pub fn outcome(&self) -> InvocationOutcome {
        self.outcome
    }
    pub fn original_sha256(&self) -> &str {
        &self.original_sha256
    }
    pub fn is_truncated(&self) -> bool {
        self.original_byte_len > self.protected.byte_len
    }
    pub fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ContentResolution<T> {
    NotRecorded,
    Missing { reference: EvidenceRef },
    Available { reference: EvidenceRef, content: T },
}

/// A canonical amendment proves a handoff, never an actor append by itself.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GuidanceDelivery {
    HandoffRecorded,
    /// The exact owned command records the actor's append acknowledgement.
    /// This does not prove that a later provider request consumed the input.
    Delivered {
        receipt_revision: u64,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecutionGuidanceView {
    pub amendment: ActivationGuidance,
    pub instruction: ContentResolution<String>,
    pub delivery: GuidanceDelivery,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecutionActivationView {
    pub activation: ContractActivation,
    /// Human label from the exact definition consumed by this activation.
    /// Mutable Agent settings cannot rename historical work.
    pub definition_name: ContentResolution<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repository_snapshots: Vec<ActivationRepositorySnapshotView>,
    pub currently_accepted: bool,
    pub output: ContentResolution<ActivationOutputContent>,
    /// Partial bodies are evidence of observed streaming output, never acceptance.
    pub partial_outputs: Vec<ActivationOutputContent>,
    /// Includes terminal partials and explicit truncation/usage metadata. These
    /// bodies never become ordinary accepted output merely by being retained.
    pub reserved_outputs: Vec<ReservedActivationOutputView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stream: Vec<ActivationStreamView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guidance: Vec<ExecutionGuidanceView>,
    /// Why a failed activation stopped and the one recovery step that fits,
    /// read from the host-written first line of its terminal output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<ActivationFailureView>,
}

/// A failed activation's cause in plain terms, with its recommended step.
/// The step is a suggestion for a person; nothing runs by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivationFailureView {
    /// provider_incomplete | provider_error | budget_limited | context_limit |
    /// scope_violation | capture_unavailable | admission | other
    pub class: &'static str,
    pub explanation: String,
    /// continue | finish_partial | review_then_finish | inspect
    pub next_step: &'static str,
}

/// Classify the host's `Activation failed: ...` line. Only that host-written
/// first line is read; the model's own text after it never decides the class.
pub fn classify_activation_failure(text: &str) -> Option<ActivationFailureView> {
    let line = text.lines().next()?.strip_prefix("Activation failed: ")?;
    // Match the host's own error prefixes in precedence order; text a tool or
    // model supplied can appear later in the line and must not decide.
    let provider = line.strip_prefix("LLM provider error: ");
    let (class, explanation, next_step) = if line.starts_with("LLM provider stream ended early")
        || provider.is_some_and(|rest| {
            rest.contains("EOF before native terminal")
                || rest.contains("ended before its completion event")
        }) {
        (
            "provider_incomplete",
            "The model provider closed its response before finishing. Nothing from that response ran.",
            "continue",
        )
    } else if provider.is_some_and(|rest| rest.contains("invocation allowance is nearly spent")) {
        (
            "budget_limited",
            "The Agent's invocation allowance ran down to what the host holds back to observe its changes and run required checks.",
            "finish_partial",
        )
    } else if provider.is_some_and(|rest| rest.contains("provider admission failed")) {
        (
            "admission",
            "The next model call could not be admitted under the approved grant.",
            "inspect",
        )
    } else if line.starts_with("This Agent reached its token limit")
        // Written before 1.1.0 named the limit in plain words.
        || line.starts_with("Token budget exceeded")
    {
        (
            "budget_limited",
            "The Agent used its whole per-execution token budget before answering.",
            "finish_partial",
        )
    } else if line.starts_with("The Session budget for this Agent is used up") {
        (
            "budget_limited",
            "The Session budget for this Agent ran out before it answered.",
            "finish_partial",
        )
    } else if line.starts_with("Current request needs") {
        (
            "context_limit",
            "The conversation no longer fits the model's context window.",
            "finish_partial",
        )
    } else if line.starts_with("it changed ")
        && line.contains("outside the paths this Agent may change")
    {
        (
            "scope_violation",
            "The Agent changed files it does not own. The changes are kept; review them before keeping or undoing them.",
            "review_then_finish",
        )
    } else if line.starts_with("its repository captures cannot establish") {
        (
            "capture_unavailable",
            "The host could not observe what the Agent changed, so its work cannot be accepted automatically.",
            "review_then_finish",
        )
    } else if line.starts_with("its admitted write scope cannot be read") {
        (
            "capture_unavailable",
            "The host could not read which files the Agent may change, so its work cannot be accepted automatically.",
            "review_then_finish",
        )
    } else if provider.is_some() {
        (
            "provider_error",
            "The model provider returned an error.",
            "continue",
        )
    } else if line.contains("admission failed") {
        (
            "admission",
            "A tool call could not be admitted under the approved grant.",
            "inspect",
        )
    } else {
        ("other", "The activation stopped with an error.", "inspect")
    };
    Some(ActivationFailureView {
        class,
        explanation: explanation.to_owned(),
        next_step,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExecutionTurnView {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repository_reattachments: Vec<RepositoryReattachmentView>,
    pub superseded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_way: Option<ActivationRef>,
    pub owner: ExecutionStoreOwner,
    pub turn_id: LogicalTurnId,
    pub revision: u64,
    pub state: LogicalTurnState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_requested: Option<crate::turn_contract::TurnStopIntent>,
    pub epochs: Vec<ExecutionEpoch>,
    pub legacy_predecessor: Option<LegacyTurnPredecessor>,
    pub request: ContentResolution<ExecutionRequestContent>,
    pub activations: Vec<ExecutionActivationView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_kind", rename_all = "snake_case", deny_unknown_fields)]
enum Body {
    Request(ExecutionRequestContent),
    WaysSelection {
        selection: crate::ways_decision::WaysSelectedSessionTurn,
        activation: ActivationRef,
    },
    ActivationStream(ActivationStreamContent),
    Output(ActivationOutputContent),
    ActivationOutputReservation(ActivationOutputReservation),
    ReservedOutput(ReservedActivationOutputContent),
    ToolReservation(ToolReservation),
    ToolResult(ToolResult),
    RepositoryCheckDefinition(RepositoryCheckDefinition),
    ConditionArguments(ConditionArguments),
    ConditionResult(ConditionResult),
    LegacyHistory(LegacyHistoryFrontier),
    ActivationEvidence(ActivationEvidenceContent),
    ProviderProfile(RetainedProviderProfile),
    RepositorySnapshot(ActivationRepositorySnapshot),
    RepositoryReattachment(RepositoryReattachment),
    StandingRepositoryCheck(StandingRepositoryCheck),
    TurnAdmission(TurnAdmissionContent),
    DriverHandoff(turn_admission::DriverHandoff),
    ControlDriverHandoff(turn_admission::ControlDriverHandoff),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    reference: EvidenceRef,
    body: Body,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    journal_id: String,
    owner: ExecutionStoreOwner,
    #[serde(deserialize_with = "bounded_records")]
    records: Vec<Record>,
}

enum Storage {
    Standalone(SecureDir),
    Owned(OwnedExecutionNamespace),
}
impl Storage {
    fn verify(&self) -> io::Result<()> {
        match self {
            Self::Standalone(dir) => dir.verify_ambient_identity(),
            Self::Owned(dir) => dir.verify_ambient_identity(),
        }
    }
    fn read(&self, max: usize) -> io::Result<Vec<u8>> {
        match self {
            Self::Standalone(dir) => dir.read_limited(FILE, max),
            Self::Owned(dir) => dir.read_limited(FILE, max),
        }
    }
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Standalone(dir) => dir.atomic_write(FILE, bytes),
            Self::Owned(dir) => dir.atomic_write(FILE, bytes),
        }
    }
    fn check_creation(&self) -> io::Result<()> {
        match self {
            Self::Standalone(dir) => check_journal_creation(dir, Path::new(FILE)),
            Self::Owned(dir) => dir.check_journal_creation(FILE),
        }
    }
    fn mark_initialized(&self, identity: &DurableSessionIdentity) -> io::Result<()> {
        match self {
            Self::Standalone(dir) => mark_journal_initialized(
                dir,
                identity,
                &ExecutionComponent::ExecutionContent,
                Path::new(FILE),
            ),
            Self::Owned(dir) => dir.mark_journal_initialized(FILE),
        }
    }
}

#[derive(Clone, Copy)]
struct Limits {
    bytes: usize,
    records: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: MAX_BYTES,
            records: MAX_RECORDS,
        }
    }
}

pub struct ExecutionContentStore {
    storage: Storage,
    identity: DurableSessionIdentity,
    data: Journal,
    limits: Limits,
    poisoned: bool,
}

impl ExecutionContentStore {
    pub fn open(
        path: impl AsRef<Path>,
        identity: DurableSessionIdentity,
    ) -> Result<Self, ExecutionContentError> {
        let dir = SecureDir::open_existing_all(path)?;
        #[cfg(unix)]
        dir.require_owner_and_private_writes(effective_uid())?;
        dir.restrict_owner_only()?;
        #[cfg(unix)]
        dir.try_lock_exclusive()?;
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "content requires directory locking",
        )
        .into());
        Self::open_at(Storage::Standalone(dir), identity, Limits::default())
    }

    pub fn open_owned(namespace: OwnedExecutionNamespace) -> Result<Self, ExecutionContentError> {
        namespace.require_root(&ExecutionComponent::ExecutionContent)?;
        let identity = namespace.identity().clone();
        Self::open_at(Storage::Owned(namespace), identity, Limits::default())
    }

    fn open_at(
        storage: Storage,
        identity: DurableSessionIdentity,
        limits: Limits,
    ) -> Result<Self, ExecutionContentError> {
        storage.verify()?;
        let data = match storage.read(limits.bytes) {
            Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                storage.check_creation()?;
                Journal {
                    schema_version: SCHEMA,
                    journal_id: identity.journal_id().into(),
                    owner: identity.owner().clone(),
                    records: vec![],
                }
            }
            Err(error) => return Err(error.into()),
        };
        validate_journal(&data, &identity, limits)?;
        let bytes = encode_bounded(&data, limits.bytes)?;
        storage.mark_initialized(&identity)?;
        // Recovery must acknowledge the directory sync even if the previous
        // process disappeared after rename but before acknowledging its write.
        storage.write(&bytes)?;
        storage.verify()?;
        Ok(Self {
            storage,
            identity,
            data,
            limits,
            poisoned: false,
        })
    }

    pub fn retain_request(
        &mut self,
        content: ExecutionRequestContent,
    ) -> Result<DurableExecutionRequest, ExecutionContentError> {
        self.healthy()?;
        validate_request(&content)?;
        let turn_id = content.turn_id.clone();
        let reference = self.append(Body::Request(content))?;
        Ok(DurableExecutionRequest {
            identity: self.identity.clone(),
            turn_id,
            reference,
        })
    }

    /// Recover a request retained before a crash at the canonical Begin seam.
    /// The caller compares the original body with any retried user command;
    /// finding this receipt does not by itself authorize a new Begin.
    pub fn retained_request(
        &self,
        turn_id: &LogicalTurnId,
    ) -> Result<Option<(DurableExecutionRequest, &ExecutionRequestContent)>, ExecutionContentError>
    {
        self.healthy()?;
        Ok(self
            .data
            .records
            .iter()
            .find_map(|record| match &record.body {
                Body::Request(content) if &content.turn_id == turn_id => Some((
                    DurableExecutionRequest {
                        identity: self.identity.clone(),
                        turn_id: turn_id.clone(),
                        reference: record.reference.clone(),
                    },
                    content,
                )),
                _ => None,
            }))
    }

    /// Index the selected Session continuation separately from deletable review
    /// evidence. The existing Keep owner verifies the applied candidate first.
    pub fn retain_ways_selection(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        selection: crate::ways_decision::WaysSelectedSessionTurn,
        activation: ActivationRef,
    ) -> Result<(), ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        if selection.session_id != snapshot.owner().session_id
            || &selection.turn_id != snapshot.turn_id()
        {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        if !matches!(
            self.resolve_activation_evidence(&selection.transcript_receipt_ref)?,
            ActivationEvidenceContent::Guidance { .. }
        ) {
            return Err(ExecutionContentError::Invalid(
                "Ways selection receipt must retain exact continuation evidence",
            ));
        }
        if !snapshot
            .contract()
            .current_accepted_activations()
            .iter()
            .any(|accepted| accepted.activation == activation)
        {
            return Err(ExecutionContentError::Invalid(
                "Selected Way has no accepted canonical result",
            ));
        }
        self.append(Body::WaysSelection {
            selection,
            activation,
        })?;
        Ok(())
    }
    pub fn ways_selections(
        &self,
        canonical: &SessionExecutionStore,
    ) -> Result<Vec<crate::ways_decision::WaysSelectedSessionTurn>, ExecutionContentError> {
        self.verify_canonical_owner(canonical)?;
        Ok(self
            .data
            .records
            .iter()
            .filter_map(|record| match &record.body {
                Body::WaysSelection { selection, .. } => Some(selection.clone()),
                _ => None,
            })
            .collect())
    }

    /// Exact native candidates retained by the host's completed Keep protocol.
    /// A completed candidate alone is not a selected workspace continuation.
    pub fn selected_way_activations(
        &self,
        canonical: &SessionExecutionStore,
    ) -> Result<Vec<ActivationRef>, ExecutionContentError> {
        self.verify_canonical_owner(canonical)?;
        Ok(self
            .data
            .records
            .iter()
            .filter_map(|record| match &record.body {
                Body::WaysSelection { activation, .. } => Some(activation.clone()),
                _ => None,
            })
            .collect())
    }

    pub fn retain_activation_evidence(
        &mut self,
        content: ActivationEvidenceContent,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        validate_activation_evidence(&content)?;
        let reference = self.append(Body::ActivationEvidence(content))?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }

    pub fn retained_check_definition(
        &self,
        definition: &RepositoryCheckDefinition,
    ) -> Result<Option<EvidenceRef>, ExecutionContentError> {
        self.healthy()?;
        Ok(self
            .data
            .records
            .iter()
            .find_map(|record| match &record.body {
                Body::RepositoryCheckDefinition(actual) if actual == definition => {
                    Some(record.reference.clone())
                }
                _ => None,
            }))
    }

    pub fn retain_repository_check_definition(
        &mut self,
        definition: RepositoryCheckDefinition,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        validate_check_definition(&definition)?;
        let reference = self.append(Body::RepositoryCheckDefinition(definition))?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }

    pub fn resolve_repository_check_definition(
        &self,
        reference: &EvidenceRef,
    ) -> Result<&RepositoryCheckDefinition, ExecutionContentError> {
        self.healthy()?;
        match self.record(reference).map(|record| &record.body) {
            Some(Body::RepositoryCheckDefinition(definition)) => Ok(definition),
            _ => Err(ExecutionContentError::Invalid(
                "missing or wrong role check definition",
            )),
        }
    }

    /// Retains the exact proposed execution and reserves its result before the
    /// canonical intent is recorded. The host revalidates canonical currentness,
    /// physical checkpoint/resource ownership and grant at actual dispatch.
    pub fn reserve_condition_arguments(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        run: &ConditionRunRef,
        repository_ref: &EvidenceRef,
    ) -> Result<DurableConditionArguments, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        validate_condition_run(run, self.identity.owner())?;
        if &run.turn_id != snapshot.turn_id() {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        let contract = snapshot.contract();
        let condition = contract
            .graph()
            .and_then(|graph| {
                graph
                    .conditions
                    .iter()
                    .find(|condition| condition.condition_id == run.condition_id)
            })
            .ok_or(ExecutionContentError::Invalid("unknown condition"))?;
        let ConditionKind::RepositoryCheck {
            definition: definition_ref,
        } = &condition.kind
        else {
            return Err(ExecutionContentError::Invalid(
                "condition is not an executable repository check",
            ));
        };
        let definition = self.resolve_repository_check_definition(definition_ref)?;
        if !matches!(
            self.resolve_activation_evidence(repository_ref)?,
            ActivationEvidenceContent::Repository { .. }
        ) {
            return Err(ExecutionContentError::Invalid(
                "wrong role repository evidence",
            ));
        }
        let selected: HashSet<_> = run
            .activations
            .iter()
            .map(|activation| &activation.node_id)
            .collect();
        if selected != condition.nodes.iter().collect::<HashSet<_>>() {
            return Err(ExecutionContentError::Invalid(
                "condition does not select its exact node scope",
            ));
        }
        // Construct a borrowed view first: the bounded writer refuses oversized
        // manifests before cloning any selected inputs into a new journal body.
        #[derive(Serialize)]
        struct Input<'a> {
            input: &'a ActivationInputManifest,
            checkpoint: &'a CheckpointRef,
            output: &'a EvidenceRef,
        }
        #[derive(Serialize)]
        struct Arguments<'a> {
            run: &'a ConditionRunRef,
            definition_ref: &'a EvidenceRef,
            definition: &'a RepositoryCheckDefinition,
            repository_ref: &'a EvidenceRef,
            inputs: Vec<Input<'a>>,
        }
        let mut inputs = Vec::with_capacity(run.activations.len());
        let accepted = contract.current_accepted_activations();
        for activation in &run.activations {
            let current = accepted
                .iter()
                .find(|current| &current.activation == activation)
                .ok_or(ExecutionContentError::Invalid(
                    "condition input is not currently accepted",
                ))?;
            let checkpoint = current
                .checkpoint
                .as_ref()
                .ok_or(ExecutionContentError::Invalid(
                    "accepted input lacks checkpoint",
                ))?;
            let output = current
                .output
                .as_ref()
                .ok_or(ExecutionContentError::Invalid(
                    "accepted input lacks output",
                ))?;
            self.resolve_final_output(activation, output)?;
            inputs.push(Input {
                input: &current.input,
                checkpoint,
                output,
            });
        }
        let encoded = encode_bounded(
            &Arguments {
                run,
                definition_ref,
                definition,
                repository_ref,
                inputs,
            },
            MAX_TOOL_BYTES,
        )?;
        let arguments: ConditionArguments = serde_json::from_slice(&encoded)?;
        // An acknowledged identical reservation remains available after a lost
        // caller acknowledgement. It neither creates another run nor a lease.
        if let Some(record) = self.data.records.iter().find(|record| {
            matches!(&record.body,
            Body::ConditionArguments(old) if old.run.run_id == run.run_id)
        }) {
            return match &record.body {
                Body::ConditionArguments(old) if old == &arguments => {
                    self.condition_arguments_receipt(record.reference.clone(), old)
                }
                _ => Err(ExecutionContentError::Conflict),
            };
        }
        if contract.state() != Some(LogicalTurnState::Running)
            || !contract
                .epochs()
                .last()
                .is_some_and(|epoch| epoch.id == run.epoch_id && epoch.state == EpochState::Running)
            || contract.current_condition(&run.condition_id).is_some()
            || contract.condition_runs().iter().any(|item| {
                item.run.run_id == run.run_id
                    || (item.run.condition_id == run.condition_id && item.resolution.is_none())
            })
        {
            return Err(ExecutionContentError::Invalid(
                "condition execution is not currently admissible",
            ));
        }
        let reference = self.append(Body::ConditionArguments(arguments.clone()))?;
        self.condition_arguments_receipt(reference, &arguments)
    }

    /// Resolves a prior reservation even after interruption/closure. Missing
    /// evidence stays absent; this is not an executable recovery claim.
    pub fn condition_arguments(
        &self,
        snapshot: &DurableTurnSnapshot,
        run_id: &ConditionRunId,
    ) -> Result<Option<DurableConditionArguments>, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        for record in &self.data.records {
            if let Body::ConditionArguments(arguments) = &record.body {
                if &arguments.run.run_id == run_id {
                    if &arguments.run.turn_id != snapshot.turn_id() {
                        return Err(ExecutionContentError::OwnerMismatch);
                    }
                    return self
                        .condition_arguments_receipt(record.reference.clone(), arguments)
                        .map(Some);
                }
            }
        }
        Ok(None)
    }

    /// Writes one immutable observed terminal result using capacity promised by
    /// the argument receipt. Retention after closure does not change readiness.
    pub fn record_condition_result(
        &mut self,
        arguments: &DurableConditionArguments,
        status: ConditionProcessStatus,
        stdout: ConditionOutputEvidence,
        stderr: ConditionOutputEvidence,
        recorded_at_unix_ms: u64,
    ) -> Result<DurableConditionResult, ExecutionContentError> {
        self.record_condition_supervised_result(
            arguments,
            status,
            stdout,
            stderr,
            recorded_at_unix_ms,
            None,
        )
    }

    /// The owning host records verified transport observations alongside the
    /// disposition. Reloading these bytes cannot mint a live cleanup receipt.
    #[allow(clippy::too_many_arguments)]
    pub fn record_condition_supervised_result(
        &mut self,
        arguments: &DurableConditionArguments,
        status: ConditionProcessStatus,
        stdout: ConditionOutputEvidence,
        stderr: ConditionOutputEvidence,
        recorded_at_unix_ms: u64,
        supervision: Option<ConditionSupervisionEvidence>,
    ) -> Result<DurableConditionResult, ExecutionContentError> {
        self.require_condition_arguments(arguments)?;
        let result = ConditionResult {
            reservation_ref: arguments.reference().clone(),
            status,
            stdout,
            stderr,
            recorded_at_unix_ms,
            supervision,
        };
        let reference = self.append(Body::ConditionResult(result.clone()))?;
        self.condition_result_receipt(arguments.clone(), reference, &result)
    }

    pub fn condition_result(
        &self,
        arguments: &DurableConditionArguments,
    ) -> Result<Option<DurableConditionResult>, ExecutionContentError> {
        self.require_condition_arguments(arguments)?;
        for record in &self.data.records {
            if let Body::ConditionResult(result) = &record.body {
                if &result.reservation_ref == arguments.reference() {
                    return self
                        .condition_result_receipt(
                            arguments.clone(),
                            record.reference.clone(),
                            result,
                        )
                        .map(Some);
                }
            }
        }
        Ok(None)
    }

    pub fn resolve_activation_evidence(
        &self,
        reference: &EvidenceRef,
    ) -> Result<&ActivationEvidenceContent, ExecutionContentError> {
        self.healthy()?;
        match self.record(reference).map(|record| &record.body) {
            Some(Body::ActivationEvidence(content)) => Ok(content),
            _ => Err(ExecutionContentError::Invalid(
                "missing or wrong role activation evidence",
            )),
        }
    }

    pub fn validate_input(
        &self,
        snapshot: &DurableTurnSnapshot,
        input: &ActivationInputManifest,
    ) -> Result<ResolvedActivationInput, ExecutionContentError> {
        let canonical = self.require_activation(snapshot, &input.activation)?;
        if canonical.input != *input {
            return Err(ExecutionContentError::Conflict);
        }
        self.validate_proposed_input(snapshot, input)
    }

    /// Resolve retained evidence before a controller admits a proposed input.
    /// This validates physical content roles and ownership only. The controller
    /// must separately preview the canonical transition and validate authority;
    /// a resolved proposal is neither an admitted activation nor a dispatch lease.
    pub fn validate_proposed_input(
        &self,
        snapshot: &DurableTurnSnapshot,
        input: &ActivationInputManifest,
    ) -> Result<ResolvedActivationInput, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        if input
            .parents
            .len()
            .saturating_add(input.guidance.len())
            .saturating_add(input.attachments.len())
            > crate::turn_contract::MAX_INPUT_REFERENCES
        {
            return Err(ExecutionContentError::Capacity);
        }
        // This public preflight may receive a caller-constructed proposal rather
        // than an already bounded canonical envelope. Bound it before cloning
        // any retained evidence bodies.
        encode_bounded(input, crate::turn_contract::MAX_CONTRACT_ENVELOPE_BYTES)?;
        if input.activation.session_id != snapshot.owner().session_id
            || &input.activation.turn_id != snapshot.turn_id()
        {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        let definition = self.resolve_activation_evidence(&input.definition.snapshot)?;
        if !matches!(definition, ActivationEvidenceContent::Definition { definition_id, .. } if definition_id == &input.definition.definition_id)
        {
            return Err(ExecutionContentError::Invalid(
                "definition evidence identity mismatch",
            ));
        }
        let grant = match &input.grant {
            Some(grant) => match self.resolve_activation_evidence(&grant.evidence)? {
                ActivationEvidenceContent::Grant { policy }
                    if policy.id == grant.grant_id.as_str()
                        && policy.revision == grant.revision =>
                {
                    Some(policy.clone())
                }
                _ => {
                    return Err(ExecutionContentError::Invalid(
                        "grant evidence identity mismatch",
                    ))
                }
            },
            None => None,
        };
        let budget = match self.resolve_activation_evidence(&input.budget)? {
            ActivationEvidenceContent::Budget { limits } => limits.clone(),
            _ => return Err(ExecutionContentError::Invalid("wrong budget evidence role")),
        };
        let mut guidance = Vec::with_capacity(input.guidance.len());
        for reference in &input.guidance {
            match self.record(reference).map(|record| &record.body) {
                Some(Body::ActivationEvidence(ActivationEvidenceContent::Guidance { text })) => {
                    guidance.push(text.clone())
                }
                Some(Body::Request(request))
                    if snapshot.request_ref() == Some(reference)
                        && &request.turn_id == snapshot.turn_id() =>
                {
                    guidance.push(request.effective_input.clone())
                }
                _ => {
                    return Err(ExecutionContentError::Invalid(
                        "missing or wrong role guidance evidence",
                    ))
                }
            }
        }
        let mut attachments = Vec::with_capacity(input.attachments.len());
        for reference in &input.attachments {
            let content = self.resolve_activation_evidence(reference)?;
            if !matches!(
                content,
                ActivationEvidenceContent::Attachment { .. }
                    | ActivationEvidenceContent::BinaryAttachment { .. }
            ) {
                return Err(ExecutionContentError::Invalid(
                    "wrong attachment evidence role",
                ));
            }
            attachments.push(content.clone());
        }
        let repository = match &input.repository {
            RepositoryInput::Unavailable => None,
            RepositoryInput::Recorded { snapshot } => {
                let content = self.resolve_activation_evidence(snapshot)?;
                if !matches!(content, ActivationEvidenceContent::Repository { .. }) {
                    return Err(ExecutionContentError::Invalid(
                        "wrong repository evidence role",
                    ));
                }
                Some(content.clone())
            }
        };
        let parents = input
            .parents
            .iter()
            .map(|parent| {
                self.resolve_final_output(&parent.activation, &parent.output)
                    .cloned()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let revision_context = input
            .revision_context
            .as_ref()
            .map(|context| {
                self.resolve_final_output(&context.activation, &context.output)
                    .cloned()
            })
            .transpose()?;
        Ok(ResolvedActivationInput {
            definition: definition.clone(),
            grant,
            budget,
            guidance,
            attachments,
            repository,
            parents,
            revision_context,
        })
    }

    fn resolve_final_output(
        &self,
        activation: &ActivationRef,
        reference: &EvidenceRef,
    ) -> Result<&ActivationOutputContent, ExecutionContentError> {
        match self.record(reference).map(|record| &record.body) {
            Some(Body::Output(content))
                if &content.activation == activation && content.kind == OutputKind::Final =>
            {
                Ok(content)
            }
            Some(Body::ReservedOutput(content))
                if &content.output.activation == activation
                    && content.slot == ActivationOutputSlot::Settlement
                    && content.output.kind == OutputKind::Final
                    && !content.is_truncated() =>
            {
                Ok(&content.output)
            }
            _ => Err(ExecutionContentError::Invalid(
                "missing or wrong producer final output evidence",
            )),
        }
    }

    /// Reserve before provider admission. Exact repeats recover the same
    /// capacity receipt even after interruption; they never authorize replay.
    /// A first reservation requires a Running activation in the supplied live
    /// epoch snapshot. The host must exclude stale snapshots under its gate.
    pub fn reserve_activation_output(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
        limits: ActivationOutputLimits,
    ) -> Result<DurableActivationOutputReservation, ExecutionContentError> {
        let candidate = self.require_activation(snapshot, activation)?;
        validate_output_limits(limits)?;
        if let Some(existing) = self.activation_output_reservation(snapshot, activation)? {
            return if existing.limits == limits {
                Ok(existing)
            } else {
                Err(ExecutionContentError::Conflict)
            };
        }
        if snapshot.contract().state() != Some(LogicalTurnState::Running)
            || candidate.state != ActivationState::Running
            || !snapshot.contract().epochs().last().is_some_and(|epoch| {
                epoch.id == activation.execution_epoch_id
                    && epoch.state == crate::turn_contract::EpochState::Running
            })
        {
            return Err(ExecutionContentError::Invalid(
                "output admission requires a live activation",
            ));
        }
        let body = ActivationOutputReservation {
            activation: activation.clone(),
            limits,
        };
        let reference = self.append(Body::ActivationOutputReservation(body.clone()))?;
        Ok(self.output_reservation_receipt(reference, &body))
    }

    pub fn activation_output_reservation(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<Option<DurableActivationOutputReservation>, ExecutionContentError> {
        self.require_activation(snapshot, activation)?;
        Ok(self
            .data
            .records
            .iter()
            .find_map(|record| match &record.body {
                Body::ActivationOutputReservation(body) if &body.activation == activation => {
                    Some(self.output_reservation_receipt(record.reference.clone(), body))
                }
                _ => None,
            }))
    }

    /// Record one immutable partial evidence slot. Sequences start at zero;
    /// exact repeats are idempotent. Exhausting these slots cannot consume the
    /// separately reserved terminal settlement slot.
    pub fn record_activation_partial(
        &mut self,
        reservation: &DurableActivationOutputReservation,
        sequence: u32,
        content: ActivationOutputContent,
    ) -> Result<DurableReservedExecutionOutput, ExecutionContentError> {
        if content.kind != OutputKind::Partial {
            return Err(ExecutionContentError::Invalid(
                "streamed output must be partial",
            ));
        }
        self.record_reserved_output(
            reservation,
            ActivationOutputSlot::Partial { sequence },
            content,
        )
    }

    /// Settle with one observed Final or terminal Partial body. Failure/Stop
    /// does not invent a final answer or known usage. This may finish after
    /// canonical closure; it does not reopen or accept that logical turn.
    pub fn settle_activation_output(
        &mut self,
        reservation: &DurableActivationOutputReservation,
        content: ActivationOutputContent,
    ) -> Result<DurableReservedExecutionOutput, ExecutionContentError> {
        self.record_reserved_output(reservation, ActivationOutputSlot::Settlement, content)
    }

    pub fn activation_output_settlement(
        &self,
        reservation: &DurableActivationOutputReservation,
    ) -> Result<Option<DurableReservedExecutionOutput>, ExecutionContentError> {
        self.require_output_reservation(reservation)?;
        Ok(self
            .data
            .records
            .iter()
            .find_map(|record| match &record.body {
                Body::ReservedOutput(body)
                    if body.reservation_ref == reservation.reference
                        && body.slot == ActivationOutputSlot::Settlement =>
                {
                    Some(self.reserved_output_receipt(record.reference.clone(), body))
                }
                _ => None,
            }))
    }

    fn record_reserved_output(
        &mut self,
        reservation: &DurableActivationOutputReservation,
        slot: ActivationOutputSlot,
        mut content: ActivationOutputContent,
    ) -> Result<DurableReservedExecutionOutput, ExecutionContentError> {
        self.require_output_reservation(reservation)?;
        if content.activation != reservation.activation {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        let capacity = match slot {
            ActivationOutputSlot::Partial { sequence } => {
                if sequence >= reservation.limits.partial_records {
                    return Err(ExecutionContentError::Capacity);
                }
                reservation.limits.partial_bytes
            }
            ActivationOutputSlot::Settlement => reservation.limits.settlement_bytes,
        };
        let original_byte_len = content.text.len() as u64;
        let original_sha256 = sha256(content.text.as_bytes());
        let mut end = content.text.len().min(capacity);
        while !content.text.is_char_boundary(end) {
            end -= 1;
        }
        content.text.truncate(end);
        let body = ReservedActivationOutputContent {
            reservation_ref: reservation.reference.clone(),
            slot,
            output: content,
            original_byte_len,
            original_sha256,
        };
        let reference = self.append(Body::ReservedOutput(body.clone()))?;
        Ok(self.reserved_output_receipt(reference, &body))
    }

    pub fn retain_output(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        content: ActivationOutputContent,
    ) -> Result<DurableExecutionOutput, ExecutionContentError> {
        self.require_activation(snapshot, &content.activation)?;
        let activation = content.activation.clone();
        validate_output(&content, self.identity.owner())?;
        let reference = self.append(Body::Output(content))?;
        Ok(DurableExecutionOutput {
            identity: self.identity.clone(),
            activation,
            reference,
        })
    }

    /// Retain a freshly captured source from this exact canonical data root.
    /// A caller-constructed or stale in-memory legacy fold is not sufficient.
    pub fn retain_legacy_history(
        &mut self,
        legacy: &OwnedLegacyHistorySnapshot,
    ) -> Result<DurableLegacyHistory, ExecutionContentError> {
        self.require_identity(legacy.identity())?;
        legacy
            .verify_current()
            .map_err(|error| ExecutionContentError::Io(io::Error::other(error.to_string())))?;
        let frontier = LegacyHistoryFrontier {
            source: legacy.source().clone(),
            turns: legacy.turns().to_vec(),
        };
        validate_legacy(&frontier, self.identity.owner())?;
        let last_predecessor = predecessor(&frontier);
        let reference = self.append(Body::LegacyHistory(frontier))?;
        Ok(DurableLegacyHistory {
            identity: self.identity.clone(),
            reference,
            last_predecessor,
            source: legacy.source().clone(),
        })
    }

    pub fn legacy_history(
        &self,
        receipt: &DurableLegacyHistory,
    ) -> Result<&LegacyHistoryFrontier, ExecutionContentError> {
        self.require_identity(&receipt.identity)?;
        match self.record(&receipt.reference).map(|record| &record.body) {
            Some(Body::LegacyHistory(frontier))
                if predecessor(frontier).as_ref() == receipt.last_predecessor() =>
            {
                Ok(frontier)
            }
            _ => Err(ExecutionContentError::Conflict),
        }
    }

    /// Resolves a frontier only after the canonical Session journal sealed it.
    /// Memory migration derives its baseline from these exact retained rows.
    pub fn read_legacy_history(
        &self,
        seal: &DurableLegacySeal,
    ) -> Result<&LegacyHistoryFrontier, ExecutionContentError> {
        self.require_identity(seal.identity())?;
        match self.record(seal.reference()).map(|record| &record.body) {
            Some(Body::LegacyHistory(frontier))
                if predecessor(frontier).as_ref() == seal.last_predecessor() =>
            {
                Ok(frontier)
            }
            _ => Err(ExecutionContentError::Conflict),
        }
    }

    /// Must precede canonical/audit intent admission. The returned receipt is
    /// persistence evidence only; the arbiter still grants actual dispatch.
    pub fn reserve_tool(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
        invocation_id: InvocationId,
        arguments: &[u8],
        result_capacity: usize,
    ) -> Result<DurableToolArguments, ExecutionContentError> {
        let current = self.require_activation(snapshot, activation)?;
        if current.state != ActivationState::Running {
            return Err(ExecutionContentError::Invalid(
                "tool activation is not running",
            ));
        }
        if arguments.len() > MAX_TOOL_BYTES || result_capacity > MAX_TOOL_BYTES {
            return Err(ExecutionContentError::Capacity);
        }
        let body = ToolReservation {
            activation: activation.clone(),
            invocation_id,
            arguments_hex: hex(arguments),
            arguments_sha256: sha256(arguments),
            result_capacity,
        };
        let reference = self.append(Body::ToolReservation(body.clone()))?;
        Ok(self.arguments_receipt(reference, &body))
    }

    /// Restores a storage receipt after restart. This neither creates a new
    /// invocation nor proves the old one was not dispatched.
    pub fn tool_arguments(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
        invocation: &InvocationId,
    ) -> Result<Option<DurableToolArguments>, ExecutionContentError> {
        self.require_activation(snapshot, activation)?;
        for record in &self.data.records {
            if let Body::ToolReservation(body) = &record.body {
                if &body.invocation_id == invocation {
                    if &body.activation != activation {
                        return Err(ExecutionContentError::OwnerMismatch);
                    }
                    return Ok(Some(self.arguments_receipt(record.reference.clone(), body)));
                }
            }
        }
        Ok(None)
    }

    pub fn read_tool_arguments(
        &self,
        receipt: &DurableToolArguments,
    ) -> Result<Vec<u8>, ExecutionContentError> {
        let body = self.require_arguments(receipt)?;
        unhex(&body.arguments_hex)
    }

    /// Late evidence can be retained after closure. Oversized results consume
    /// only reserved bytes while retaining observed return status, full digest,
    /// and original length. Callers must never parse a prefix as a full result.
    pub fn record_tool_result(
        &mut self,
        arguments: &DurableToolArguments,
        outcome: InvocationOutcome,
        bytes: &[u8],
        recorded_at_unix_ms: u64,
    ) -> Result<DurableToolResult, ExecutionContentError> {
        self.require_arguments(arguments)?;
        let retained = &bytes[..bytes.len().min(arguments.result_capacity)];
        let body = ToolResult {
            reservation_ref: arguments.protected.evidence_ref.clone(),
            recorded_at_unix_ms,
            retained_hex: hex(retained),
            retained_sha256: sha256(retained),
            outcome,
            original_sha256: sha256(bytes),
            original_byte_len: bytes.len() as u64,
        };
        let reference = self.append(Body::ToolResult(body.clone()))?;
        Ok(self.result_receipt(arguments.clone(), reference, &body))
    }

    pub fn tool_result(
        &self,
        arguments: &DurableToolArguments,
    ) -> Result<Option<DurableToolResult>, ExecutionContentError> {
        self.require_arguments(arguments)?;
        Ok(self
            .data
            .records
            .iter()
            .find_map(|record| match &record.body {
                Body::ToolResult(body)
                    if body.reservation_ref == arguments.protected.evidence_ref =>
                {
                    Some(self.result_receipt(arguments.clone(), record.reference.clone(), body))
                }
                _ => None,
            }))
    }

    pub fn read_tool_result(
        &self,
        receipt: &DurableToolResult,
    ) -> Result<Vec<u8>, ExecutionContentError> {
        self.require_arguments(&receipt.arguments)?;
        match self
            .record(&receipt.protected.evidence_ref)
            .map(|record| &record.body)
        {
            Some(Body::ToolResult(body))
                if self.result_receipt(
                    receipt.arguments.clone(),
                    receipt.protected.evidence_ref.clone(),
                    body,
                ) == *receipt =>
            {
                unhex(&body.retained_hex)
            }
            _ => Err(ExecutionContentError::Conflict),
        }
    }

    pub fn project(
        &self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<ExecutionTurnView, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        let contract = snapshot.contract();
        let request = match snapshot.request_ref() {
            None => ContentResolution::NotRecorded,
            Some(reference) => match self.record(reference).map(|record| &record.body) {
                None => ContentResolution::Missing {
                    reference: reference.clone(),
                },
                Some(Body::Request(content)) if &content.turn_id == snapshot.turn_id() => {
                    ContentResolution::Available {
                        reference: reference.clone(),
                        content: content.clone(),
                    }
                }
                _ => return Err(ExecutionContentError::Conflict),
            },
        };
        let accepted = contract.current_accepted_activations();
        let mut activations = Vec::with_capacity(contract.activations().len());
        for activation in contract.activations() {
            let output = match &activation.output {
                None => ContentResolution::NotRecorded,
                Some(reference) => match self.record(reference).map(|record| &record.body) {
                    None => ContentResolution::Missing {
                        reference: reference.clone(),
                    },
                    Some(Body::Output(content))
                        if content.activation == activation.activation
                            && content.kind == OutputKind::Final =>
                    {
                        ContentResolution::Available {
                            reference: reference.clone(),
                            content: content.clone(),
                        }
                    }
                    Some(Body::ReservedOutput(content))
                        if content.output.activation == activation.activation
                            && content.slot == ActivationOutputSlot::Settlement
                            && content.output.kind == OutputKind::Final
                            && !content.is_truncated() =>
                    {
                        ContentResolution::Available {
                            reference: reference.clone(),
                            content: content.output.clone(),
                        }
                    }
                    _ => return Err(ExecutionContentError::Conflict),
                },
            };
            let reserved_outputs: Vec<ReservedActivationOutputView> = self
                .data
                .records
                .iter()
                .filter_map(|record| match &record.body {
                    Body::ReservedOutput(body)
                        if body.output.activation == activation.activation =>
                    {
                        Some(ReservedActivationOutputView {
                            reference: record.reference.clone(),
                            content: body.clone(),
                        })
                    }
                    _ => None,
                })
                .collect();
            let partial_outputs = self
                .data
                .records
                .iter()
                .filter_map(|record| match &record.body {
                    Body::Output(content)
                        if content.activation == activation.activation
                            && content.kind == OutputKind::Partial =>
                    {
                        Some(content.clone())
                    }
                    _ => None,
                })
                .collect();
            let guidance = contract
                .guidance()
                .iter()
                .filter(|item| item.activation == activation.activation)
                .map(|amendment| {
                    let instruction = match self
                        .record(&amendment.instruction)
                        .map(|record| &record.body)
                    {
                        None => ContentResolution::Missing {
                            reference: amendment.instruction.clone(),
                        },
                        Some(Body::ActivationEvidence(ActivationEvidenceContent::Guidance {
                            text,
                        })) => ContentResolution::Available {
                            reference: amendment.instruction.clone(),
                            content: text.clone(),
                        },
                        _ => return Err(ExecutionContentError::Conflict),
                    };
                    Ok(ExecutionGuidanceView {
                        amendment: amendment.clone(),
                        instruction,
                        delivery: GuidanceDelivery::HandoffRecorded,
                    })
                })
                .collect::<Result<Vec<_>, ExecutionContentError>>()?;
            activations.push(ExecutionActivationView {
                activation: activation.clone(),
                definition_name: match self
                    .record(&activation.input.definition.snapshot)
                    .map(|record| &record.body)
                {
                    Some(Body::ActivationEvidence(ActivationEvidenceContent::Definition {
                        configuration,
                        ..
                    })) => {
                        match serde_json::from_str::<serde_json::Value>(configuration)
                            .ok()
                            .and_then(|value| {
                                value
                                    .get("name")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned)
                            }) {
                            Some(name) if !name.trim().is_empty() => ContentResolution::Available {
                                reference: activation.input.definition.snapshot.clone(),
                                content: name,
                            },
                            _ => ContentResolution::NotRecorded,
                        }
                    }
                    None => ContentResolution::Missing {
                        reference: activation.input.definition.snapshot.clone(),
                    },
                    _ => return Err(ExecutionContentError::Conflict),
                },
                repository_snapshots: self
                    .repository_snapshots(snapshot, &activation.activation)?,
                currently_accepted: accepted
                    .iter()
                    .any(|current| current.activation == activation.activation),
                output,
                partial_outputs,
                guidance,
                stream: self.activation_stream(snapshot, &activation.activation)?,
                reserved_outputs: reserved_outputs.clone(),
                failure: (activation.state == ActivationState::Failed)
                    .then(|| {
                        reserved_outputs.iter().find_map(|reserved| {
                            classify_activation_failure(&reserved.content.output.text)
                        })
                    })
                    .flatten(),
            });
        }
        Ok(ExecutionTurnView {
            repository_reattachments: self.repository_reattachments(snapshot)?,
            superseded: false,
            kept_way: self
                .data
                .records
                .iter()
                .find_map(|record| match &record.body {
                    Body::WaysSelection {
                        selection,
                        activation,
                    } if &selection.turn_id == snapshot.turn_id() => Some(activation.clone()),
                    _ => None,
                }),
            owner: self.identity.owner().clone(),
            turn_id: snapshot.turn_id().clone(),
            revision: contract.revision(),
            state: contract
                .state()
                .ok_or(ExecutionContentError::Invalid("snapshot has no Begin"))?,
            stop_requested: contract.stop_requested().cloned(),
            epochs: contract.epochs().to_vec(),
            legacy_predecessor: snapshot.legacy_predecessor().cloned(),
            request,
            activations,
        })
    }

    fn healthy(&self) -> Result<(), ExecutionContentError> {
        if self.poisoned {
            return Err(ExecutionContentError::RecoveryRequired);
        }
        self.storage.verify()?;
        Ok(())
    }
    /// Verify a held child store without reopening its exclusive namespace.
    pub fn verify_canonical_owner(
        &self,
        canonical: &SessionExecutionStore,
    ) -> Result<(), ExecutionContentError> {
        self.require_identity(
            &canonical
                .identity()
                .map_err(|error| ExecutionContentError::Io(std::io::Error::other(error)))?,
        )
    }

    /// A copied identity on a standalone journal does not establish ownership
    /// of this Session's retained content namespace.
    pub(crate) fn require_owned_identity(
        &self,
        identity: &DurableSessionIdentity,
    ) -> Result<(), ExecutionContentError> {
        self.require_identity(identity)?;
        if !matches!(&self.storage, Storage::Owned(_)) {
            return Err(ExecutionContentError::Invalid(
                "Session configuration requires actual owned content storage",
            ));
        }
        Ok(())
    }

    fn require_identity(
        &self,
        identity: &DurableSessionIdentity,
    ) -> Result<(), ExecutionContentError> {
        self.healthy()?;
        if identity != &self.identity {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        Ok(())
    }
    fn require_snapshot(
        &self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<(), ExecutionContentError> {
        self.healthy()?;
        if snapshot.journal_id() != self.identity.journal_id()
            || snapshot.owner() != self.identity.owner()
        {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        Ok(())
    }
    fn require_activation<'a>(
        &self,
        snapshot: &'a DurableTurnSnapshot,
        activation: &ActivationRef,
    ) -> Result<&'a ContractActivation, ExecutionContentError> {
        self.require_snapshot(snapshot)?;
        snapshot
            .contract()
            .activations()
            .iter()
            .find(|candidate| &candidate.activation == activation)
            .ok_or(ExecutionContentError::OwnerMismatch)
    }
    fn require_arguments(
        &self,
        receipt: &DurableToolArguments,
    ) -> Result<&ToolReservation, ExecutionContentError> {
        self.require_identity(&receipt.identity)?;
        match self
            .record(&receipt.protected.evidence_ref)
            .map(|record| &record.body)
        {
            Some(Body::ToolReservation(body))
                if self.arguments_receipt(receipt.protected.evidence_ref.clone(), body)
                    == *receipt =>
            {
                Ok(body)
            }
            _ => Err(ExecutionContentError::Conflict),
        }
    }
    fn condition_arguments_receipt(
        &self,
        reference: EvidenceRef,
        arguments: &ConditionArguments,
    ) -> Result<DurableConditionArguments, ExecutionContentError> {
        let encoded = encode_bounded(arguments, MAX_TOOL_BYTES)?;
        Ok(DurableConditionArguments {
            identity: self.identity.clone(),
            protected: ProtectedArguments {
                evidence_ref: reference,
                sha256: sha256(&encoded),
                byte_len: encoded.len() as u64,
            },
            arguments: arguments.clone(),
        })
    }
    fn require_condition_arguments(
        &self,
        receipt: &DurableConditionArguments,
    ) -> Result<&ConditionArguments, ExecutionContentError> {
        self.require_identity(&receipt.identity)?;
        match self.record(receipt.reference()).map(|record| &record.body) {
            Some(Body::ConditionArguments(arguments))
                if self.condition_arguments_receipt(receipt.reference().clone(), arguments)?
                    == *receipt =>
            {
                Ok(arguments)
            }
            _ => Err(ExecutionContentError::Conflict),
        }
    }
    fn condition_result_receipt(
        &self,
        arguments: DurableConditionArguments,
        reference: EvidenceRef,
        result: &ConditionResult,
    ) -> Result<DurableConditionResult, ExecutionContentError> {
        let encoded = encode_bounded(result, MAX_TOOL_BYTES * 2 + RESULT_OVERHEAD)?;
        Ok(DurableConditionResult {
            arguments,
            protected: ProtectedArguments {
                evidence_ref: reference,
                sha256: sha256(&encoded),
                byte_len: encoded.len() as u64,
            },
            result: result.clone(),
        })
    }
    fn require_output_reservation(
        &self,
        receipt: &DurableActivationOutputReservation,
    ) -> Result<&ActivationOutputReservation, ExecutionContentError> {
        self.require_identity(&receipt.identity)?;
        match self.record(&receipt.reference).map(|record| &record.body) {
            Some(Body::ActivationOutputReservation(body))
                if self.output_reservation_receipt(receipt.reference.clone(), body) == *receipt =>
            {
                Ok(body)
            }
            _ => Err(ExecutionContentError::Conflict),
        }
    }
    fn output_reservation_receipt(
        &self,
        reference: EvidenceRef,
        body: &ActivationOutputReservation,
    ) -> DurableActivationOutputReservation {
        DurableActivationOutputReservation {
            identity: self.identity.clone(),
            reference,
            activation: body.activation.clone(),
            limits: body.limits,
        }
    }
    fn reserved_output_receipt(
        &self,
        reference: EvidenceRef,
        body: &ReservedActivationOutputContent,
    ) -> DurableReservedExecutionOutput {
        DurableReservedExecutionOutput {
            identity: self.identity.clone(),
            reference,
            content: body.clone(),
        }
    }
    fn arguments_receipt(
        &self,
        reference: EvidenceRef,
        body: &ToolReservation,
    ) -> DurableToolArguments {
        DurableToolArguments {
            identity: self.identity.clone(),
            activation: body.activation.clone(),
            invocation_id: body.invocation_id.clone(),
            protected: ProtectedArguments {
                evidence_ref: reference,
                sha256: body.arguments_sha256.clone(),
                byte_len: (body.arguments_hex.len() / 2) as u64,
            },
            result_capacity: body.result_capacity,
        }
    }
    fn result_receipt(
        &self,
        arguments: DurableToolArguments,
        reference: EvidenceRef,
        body: &ToolResult,
    ) -> DurableToolResult {
        DurableToolResult {
            arguments,
            outcome: body.outcome,
            original_sha256: body.original_sha256.clone(),
            protected: ProtectedArguments {
                evidence_ref: reference,
                sha256: body.retained_sha256.clone(),
                byte_len: (body.retained_hex.len() / 2) as u64,
            },
            original_byte_len: body.original_byte_len,
            recorded_at_unix_ms: body.recorded_at_unix_ms,
        }
    }
    fn record(&self, reference: &EvidenceRef) -> Option<&Record> {
        self.data
            .records
            .iter()
            .find(|record| &record.reference == reference)
    }

    fn append(&mut self, body: Body) -> Result<EvidenceRef, ExecutionContentError> {
        self.append_with(body, |storage, bytes| storage.write(bytes))
    }
    fn append_with(
        &mut self,
        body: Body,
        write: impl FnOnce(&Storage, &[u8]) -> io::Result<()>,
    ) -> Result<EvidenceRef, ExecutionContentError> {
        self.healthy()?;
        validate_body(&body, self.identity.owner())?;
        let reference = content_reference(&self.identity, &body)?;
        if let Some(existing) = self.record(&reference) {
            return if existing.body == body {
                Ok(reference)
            } else {
                Err(ExecutionContentError::Conflict)
            };
        }
        if self.data.records.len() >= self.limits.records {
            return Err(ExecutionContentError::Capacity);
        }
        validate_next(&self.data.records, &body)?;
        let mut next = self.data.clone();
        next.records.push(Record {
            reference: reference.clone(),
            body,
        });
        validate_capacity(&next, self.limits)?;
        let bytes = encode_bounded(&next, self.limits.bytes)?;
        if write(&self.storage, &bytes)
            .and_then(|()| self.storage.verify())
            .is_err()
        {
            self.poisoned = true;
            return Err(ExecutionContentError::RecoveryRequired);
        }
        self.data = next;
        Ok(reference)
    }
}

fn validate_check_definition(
    value: &RepositoryCheckDefinition,
) -> Result<(), ExecutionContentError> {
    if value.argv.is_empty()
        || value.argv.len() > MAX_CHECK_ARGUMENTS
        || value.argv[0].is_empty()
        || value.argv.iter().any(|arg| arg.contains('\0'))
        || value.timeout_ms == 0
        || value.timeout_ms > MAX_CHECK_TIMEOUT_MS
        || value.stdout_bytes.saturating_add(value.stderr_bytes) > MAX_TOOL_BYTES
    {
        return Err(ExecutionContentError::Invalid(
            "invalid or unbounded repository check definition",
        ));
    }
    encode_bounded(&value.argv, MAX_CHECK_ARGUMENT_BYTES)?;
    Ok(())
}

fn validate_condition_run(
    run: &ConditionRunRef,
    owner: &ExecutionStoreOwner,
) -> Result<(), ExecutionContentError> {
    if run.session_id != owner.session_id {
        return Err(ExecutionContentError::OwnerMismatch);
    }
    if run.activations.is_empty() || run.activations.len() > MAX_CONTRACT_NODES {
        return Err(ExecutionContentError::Capacity);
    }
    let mut nodes = HashSet::new();
    for activation in &run.activations {
        if activation.session_id != run.session_id
            || activation.turn_id != run.turn_id
            || activation.generation == 0
            || !nodes.insert(&activation.node_id)
        {
            return Err(ExecutionContentError::Invalid(
                "invalid condition input scope",
            ));
        }
    }
    Ok(())
}

fn validate_condition_output(value: &ConditionOutputEvidence) -> Result<(), ExecutionContentError> {
    if value.retained_hex.len() > MAX_TOOL_BYTES * 2
        || value.observed_byte_len < value.retained_byte_len() as u64
        || !valid_digest(&value.observed_sha256)
        || sha256(&unhex(&value.retained_hex)?) != value.retained_sha256
        || (value.observed_byte_len == value.retained_byte_len() as u64
            && value.observed_sha256 != value.retained_sha256)
    {
        return Err(ExecutionContentError::Invalid(
            "invalid observed condition output",
        ));
    }
    Ok(())
}

fn validate_condition_supervision(value: &ConditionResult) -> Result<(), ExecutionContentError> {
    let Some(evidence) = &value.supervision else {
        return Ok(());
    };
    for (identity, limit) in [
        (&evidence.invocation_id, 128),
        (&evidence.runtime_identity, 256),
        (&evidence.transport_identity, 256),
    ] {
        if identity.is_empty() || identity.len() > limit || identity.chars().any(char::is_control) {
            return Err(ExecutionContentError::Invalid(
                "invalid supervisor identity",
            ));
        }
    }
    if !valid_digest(&evidence.request_sha256) || !valid_digest(&evidence.program_sha256) {
        return Err(ExecutionContentError::Invalid("invalid supervisor digest"));
    }
    if evidence.primary_exit.as_ref().is_some_and(|status| {
        !evidence.launched || !matches!(status,
            ConditionProcessStatus::Exited { code } if (0..=255).contains(code))
            && !matches!(status, ConditionProcessStatus::Signalled { signal } if (1..=64).contains(signal))
    }) || (!evidence.launched
        && (value.stdout.observed_byte_len != 0 || value.stderr.observed_byte_len != 0)) {
        return Err(ExecutionContentError::Invalid("invalid supervised process observations"));
    }
    let consistent = match &value.status {
        ConditionProcessStatus::Exited { code } => {
            evidence.launched
                && (0..=255).contains(code)
                && evidence.primary_exit.as_ref() == Some(&value.status)
        }
        ConditionProcessStatus::Signalled { signal } => {
            evidence.launched
                && (1..=64).contains(signal)
                && evidence.primary_exit.as_ref() == Some(&value.status)
        }
        ConditionProcessStatus::NotDispatched => !evidence.launched && evidence.quiescent,
        ConditionProcessStatus::LaunchFailed { .. } => !evidence.launched,
        // A primary command may exit before a descendant triggers cancellation
        // or timeout. Keep that independent observation, including exit code 0.
        ConditionProcessStatus::TimedOut
        | ConditionProcessStatus::Interrupted
        | ConditionProcessStatus::Uncertain { .. } => true,
    };
    if !consistent {
        return Err(ExecutionContentError::Invalid(
            "supervisor status conflicts with process observations",
        ));
    }
    Ok(())
}

fn validate_request(value: &ExecutionRequestContent) -> Result<(), ExecutionContentError> {
    if value.display_input.len() > MAX_TEXT
        || value.effective_input.len() > MAX_TEXT
        || value.context.len() > MAX_CONTEXTS
    {
        return Err(ExecutionContentError::Capacity);
    }
    encode_bounded(&value.context, MAX_CONTEXT_BYTES)?;
    if let Some(model) = &value.model {
        bounded_name(&model.provider_id)?;
        bounded_name(&model.model_id)?;
    }
    Ok(())
}
fn validate_output(
    value: &ActivationOutputContent,
    owner: &ExecutionStoreOwner,
) -> Result<(), ExecutionContentError> {
    if value.activation.session_id != owner.session_id {
        return Err(ExecutionContentError::OwnerMismatch);
    }
    if value.text.len() > MAX_TEXT {
        return Err(ExecutionContentError::Capacity);
    }
    Ok(())
}
fn validate_output_limits(limits: ActivationOutputLimits) -> Result<(), ExecutionContentError> {
    if limits.partial_records > MAX_PARTIAL_OUTPUT_RECORDS
        || limits.partial_bytes > MAX_TEXT
        || limits.settlement_bytes == 0
        || limits.settlement_bytes > MAX_TEXT
        || (limits.partial_records == 0) != (limits.partial_bytes == 0)
    {
        return Err(ExecutionContentError::Capacity);
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
fn validate_activation_evidence(
    value: &ActivationEvidenceContent,
) -> Result<(), ExecutionContentError> {
    fn profile(profile: &ExecutionProfile) -> Result<(), ExecutionContentError> {
        for value in [
            &profile.definition,
            &profile.provider,
            &profile.model,
            &profile.isolation,
        ] {
            bounded_name(value)?;
        }
        if profile.tools.len() > 128 {
            return Err(ExecutionContentError::Capacity);
        }
        let mut names = HashSet::new();
        for tool in &profile.tools {
            bounded_name(tool)?;
            if !names.insert(tool) {
                return Err(ExecutionContentError::Invalid("duplicate profile tool"));
            }
        }
        Ok(())
    }
    match value {
        ActivationEvidenceContent::Definition {
            definition_id,
            revision,
            profile: execution_profile,
            configuration,
        } => {
            if *revision == 0
                || execution_profile.definition != definition_id.as_str()
                || configuration.is_empty()
            {
                return Err(ExecutionContentError::Invalid(
                    "invalid definition snapshot",
                ));
            }
            profile(execution_profile)?;
        }
        ActivationEvidenceContent::Attachment {
            reference_id,
            media_type,
            ..
        } => {
            bounded_name(reference_id)?;
            bounded_name(media_type)?;
        }
        ActivationEvidenceContent::BinaryAttachment { attachment } => {
            attachment.validate()?;
            // Binary evidence retains the existing upload byte ceiling. Its
            // encoded body still shares the canonical store's aggregate cap.
            encode_bounded(value, MAX_BYTES)?;
            return Ok(());
        }
        ActivationEvidenceContent::Repository {
            revision: Some(revision),
            ..
        } => bounded_name(revision)?,
        ActivationEvidenceContent::Grant { policy } => {
            bounded_name(&policy.id)?;
            if policy.revision == 0 || policy.descendants.len() > 128 || policy.profiles.len() > 128
            {
                return Err(ExecutionContentError::Invalid("invalid grant snapshot"));
            }
            let unique: HashSet<_> = policy.descendants.iter().collect();
            if unique.len() != policy.descendants.len() {
                return Err(ExecutionContentError::Invalid("duplicate grant descendant"));
            }
            for execution_profile in &policy.profiles {
                profile(execution_profile)?;
            }
        }
        _ => {}
    }
    encode_bounded(value, 512 * 1024)?;
    Ok(())
}
fn validate_legacy(
    value: &LegacyHistoryFrontier,
    owner: &ExecutionStoreOwner,
) -> Result<(), ExecutionContentError> {
    if value.turns.len() > MAX_LEGACY_TURNS {
        return Err(ExecutionContentError::Capacity);
    }
    if !value.source.validate() {
        return Err(ExecutionContentError::Invalid(
            "unsupported legacy source identity",
        ));
    }
    let mut ids = HashSet::new();
    for turn in &value.turns {
        if turn.session_id != owner.session_id.as_str() {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        if !turn.status.is_terminal() {
            return Err(ExecutionContentError::Invalid(
                "legacy history still has a running turn",
            ));
        }
        if !ids.insert(&turn.id) {
            return Err(ExecutionContentError::Invalid("duplicate legacy turn"));
        }
    }
    encode_bounded(value, MAX_BYTES / 2)?;
    Ok(())
}
fn predecessor(value: &LegacyHistoryFrontier) -> Option<LegacyTurnPredecessor> {
    value
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.superseded)
        .map(|turn| LegacyTurnPredecessor {
            turn_id: turn.id.clone(),
            status: turn.status,
            created_at: turn.created_at,
            updated_at: turn.updated_at,
        })
}
fn validate_body(body: &Body, owner: &ExecutionStoreOwner) -> Result<(), ExecutionContentError> {
    match body {
        Body::Request(value) => validate_request(value),
        Body::WaysSelection {
            selection: value,
            activation,
        } => {
            if value.session_id != owner.session_id
                || activation.session_id != value.session_id
                || activation.turn_id != value.turn_id
            {
                return Err(ExecutionContentError::OwnerMismatch);
            }
            Ok(())
        }
        Body::Output(value) => validate_output(value, owner),
        Body::ActivationOutputReservation(value) => {
            if value.activation.session_id != owner.session_id || value.activation.generation == 0 {
                return Err(ExecutionContentError::OwnerMismatch);
            }
            validate_output_limits(value.limits)
        }
        Body::ActivationStream(value) => stream::validate_stream(value, owner),
        Body::ReservedOutput(value) => {
            validate_output(&value.output, owner)?;
            if value.original_byte_len < value.output.text.len() as u64
                || !valid_digest(&value.original_sha256)
                || (!value.is_truncated()
                    && value.original_sha256 != sha256(value.output.text.as_bytes()))
                || (matches!(value.slot, ActivationOutputSlot::Partial { .. })
                    && value.output.kind != OutputKind::Partial)
            {
                return Err(ExecutionContentError::Invalid(
                    "invalid reserved output evidence",
                ));
            }
            Ok(())
        }
        Body::LegacyHistory(value) => validate_legacy(value, owner),
        Body::ActivationEvidence(value) => validate_activation_evidence(value),
        Body::ProviderProfile(value) => provider::validate_provider_profile(value),
        Body::TurnAdmission(value) => turn_admission::validate_admission(value, owner),
        Body::DriverHandoff(_) | Body::ControlDriverHandoff(_) => Ok(()),
        Body::RepositoryCheckDefinition(value) => validate_check_definition(value),
        Body::ConditionArguments(value) => {
            validate_condition_run(&value.run, owner)?;
            validate_check_definition(&value.definition)?;
            if value.inputs.len() != value.run.activations.len() {
                return Err(ExecutionContentError::Invalid(
                    "condition input count mismatch",
                ));
            }
            encode_bounded(value, MAX_TOOL_BYTES)?;
            for (input, activation) in value.inputs.iter().zip(&value.run.activations) {
                if &input.input.activation != activation
                    || input.checkpoint.session_id != owner.session_id
                    || input.checkpoint.conversation_id != input.input.conversation_id
                    || !matches!(&input.checkpoint.source, crate::turn_contract::CheckpointSource::Accepted { activation: producer } if producer == activation)
                {
                    return Err(ExecutionContentError::OwnerMismatch);
                }
            }
            Ok(())
        }
        Body::ConditionResult(value) => {
            validate_condition_output(&value.stdout)?;
            validate_condition_output(&value.stderr)?;
            validate_condition_supervision(value)?;
            match &value.status {
                ConditionProcessStatus::NotDispatched
                    if value.stdout.observed_byte_len != 0
                        || value.stderr.observed_byte_len != 0 =>
                {
                    Err(ExecutionContentError::Invalid(
                        "not-dispatched check cannot have observed process output",
                    ))
                }
                ConditionProcessStatus::LaunchFailed { message }
                | ConditionProcessStatus::Uncertain { message }
                    if message.is_empty() || message.len() > 512 =>
                {
                    Err(ExecutionContentError::Invalid(
                        "invalid condition failure evidence",
                    ))
                }
                ConditionProcessStatus::Signalled { signal } if *signal <= 0 => {
                    Err(ExecutionContentError::Invalid("invalid process signal"))
                }
                _ => Ok(()),
            }
        }
        Body::RepositorySnapshot(value) => repository_snapshot::validate_fields(value, owner),
        Body::RepositoryReattachment(value) => {
            if value.original == value.acquired {
                return Err(ExecutionContentError::Invalid(
                    "reattachment must retain a fresh resource observation",
                ));
            }
            Ok(())
        }
        Body::StandingRepositoryCheck(value) => {
            if value.activation.session_id != owner.session_id {
                return Err(ExecutionContentError::OwnerMismatch);
            }
            encode_bounded(value, 64 * 1024).map(|_| ())
        }
        Body::ToolReservation(value) => {
            if value.activation.session_id != owner.session_id {
                return Err(ExecutionContentError::OwnerMismatch);
            }
            if value.arguments_hex.len() > MAX_TOOL_BYTES * 2
                || value.result_capacity > MAX_TOOL_BYTES
            {
                return Err(ExecutionContentError::Capacity);
            }
            if sha256(&unhex(&value.arguments_hex)?) != value.arguments_sha256 {
                return Err(ExecutionContentError::Invalid(
                    "tool argument digest mismatch",
                ));
            }
            Ok(())
        }
        Body::ToolResult(value) => {
            if value.retained_hex.len() > MAX_TOOL_BYTES * 2
                || value.original_byte_len < (value.retained_hex.len() / 2) as u64
            {
                return Err(ExecutionContentError::Invalid("invalid tool result length"));
            }
            if sha256(&unhex(&value.retained_hex)?) != value.retained_sha256 {
                return Err(ExecutionContentError::Invalid(
                    "tool result digest mismatch",
                ));
            }
            if value.original_sha256.len() != 64
                || value
                    .original_sha256
                    .bytes()
                    .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(ExecutionContentError::Invalid(
                    "invalid original result digest",
                ));
            }
            if value.original_byte_len == (value.retained_hex.len() / 2) as u64
                && value.original_sha256 != value.retained_sha256
            {
                return Err(ExecutionContentError::Invalid(
                    "complete result digest mismatch",
                ));
            }
            Ok(())
        }
    }
}
fn validate_next(records: &[Record], body: &Body) -> Result<(), ExecutionContentError> {
    if let Body::WaysSelection { selection, .. } = body {
        if !records.iter().any(|record| {
            record.reference == selection.transcript_receipt_ref
                && matches!(
                    &record.body,
                    Body::ActivationEvidence(ActivationEvidenceContent::Guidance { .. })
                )
        }) {
            return Err(ExecutionContentError::Invalid(
                "Ways selection receipt is missing",
            ));
        }
    }
    if let Body::ControlDriverHandoff(content) = body {
        turn_admission::validate_control_handoff_next(records, content)?;
    }
    if let Body::TurnAdmission(content) = body {
        turn_admission::validate_admission_next(records, content)?;
    }
    if let Body::DriverHandoff(content) = body {
        turn_admission::validate_handoff_next(records, content)?;
    }
    if let Body::ProviderProfile(profile) = body {
        provider::validate_provider_next(records, profile)?;
    }
    if let Body::ActivationStream(content) = body {
        stream::validate_stream_next(records, content)?;
    }
    for record in records {
        let conflict = match (&record.body, body) {
            (Body::Request(old), Body::Request(new)) => old.turn_id == new.turn_id,
            (
                Body::WaysSelection { selection: old, .. },
                Body::WaysSelection { selection: new, .. },
            ) => old.turn_id == new.turn_id,
            // Pre-seal recapture may retain another immutable candidate after
            // source change. Only the canonical journal selects the one frontier.
            (Body::LegacyHistory(_), Body::LegacyHistory(_)) => false,
            (Body::ToolReservation(old), Body::ToolReservation(new)) => {
                old.invocation_id == new.invocation_id
            }
            (Body::ToolResult(old), Body::ToolResult(new)) => {
                old.reservation_ref == new.reservation_ref
            }
            (Body::ConditionArguments(old), Body::ConditionArguments(new)) => {
                old.run.run_id == new.run.run_id
            }
            (Body::ConditionResult(old), Body::ConditionResult(new)) => {
                old.reservation_ref == new.reservation_ref
            }
            (Body::ActivationOutputReservation(old), Body::ActivationOutputReservation(new)) => {
                old.activation == new.activation
            }
            (Body::ActivationOutputReservation(old), Body::Output(new)) => {
                old.activation == new.activation
            }
            (Body::Output(old), Body::ActivationOutputReservation(new)) => {
                old.activation == new.activation
            }
            (Body::ReservedOutput(old), Body::ReservedOutput(new)) => {
                old.reservation_ref == new.reservation_ref
                    && (old.slot == new.slot || old.slot == ActivationOutputSlot::Settlement)
            }
            _ => false,
        };
        if conflict {
            return Err(ExecutionContentError::Conflict);
        }
    }
    if let Body::RepositoryReattachment(proof) = body {
        for reference in [&proof.original, &proof.acquired] {
            if !records.iter().any(|record| {
                &record.reference == reference
                    && matches!(
                        &record.body,
                        Body::ActivationEvidence(ActivationEvidenceContent::Repository { .. })
                    )
            }) {
                return Err(ExecutionContentError::Invalid(
                    "reattachment lacks retained repository descriptions",
                ));
            }
        }
    }
    if let Body::ConditionArguments(arguments) = body {
        let definition_matches = records.iter().any(|record| record.reference == arguments.definition_ref
            && matches!(&record.body, Body::RepositoryCheckDefinition(definition) if definition == &arguments.definition));
        let repository_matches = records.iter().any(|record| {
            record.reference == arguments.repository_ref
                && matches!(
                    &record.body,
                    Body::ActivationEvidence(ActivationEvidenceContent::Repository { .. })
                )
        });
        if !definition_matches || !repository_matches {
            return Err(ExecutionContentError::Invalid(
                "condition arguments lack retained definition or repository evidence",
            ));
        }
        for input in &arguments.inputs {
            if !records.iter().any(|record| {
                record.reference == input.output
                    && match &record.body {
                        Body::Output(output) => {
                            output.activation == input.input.activation
                                && output.kind == OutputKind::Final
                        }
                        Body::ReservedOutput(output) => {
                            output.output.activation == input.input.activation
                                && output.output.kind == OutputKind::Final
                                && output.slot == ActivationOutputSlot::Settlement
                                && !output.is_truncated()
                        }
                        _ => false,
                    }
            }) {
                return Err(ExecutionContentError::Invalid(
                    "condition arguments lack exact accepted output evidence",
                ));
            }
        }
    }
    if let Body::ConditionResult(result) = body {
        let arguments = records
            .iter()
            .find_map(|record| match &record.body {
                Body::ConditionArguments(arguments)
                    if record.reference == result.reservation_ref =>
                {
                    Some(arguments)
                }
                _ => None,
            })
            .ok_or(ExecutionContentError::Invalid(
                "condition result has no prior reservation",
            ))?;
        if result
            .supervision
            .as_ref()
            .is_some_and(|evidence| evidence.invocation_id != arguments.run.run_id.as_str())
        {
            return Err(ExecutionContentError::Invalid(
                "supervised result belongs to another condition run",
            ));
        }
        for (output, capacity) in [
            (&result.stdout, arguments.definition.stdout_bytes),
            (&result.stderr, arguments.definition.stderr_bytes),
        ] {
            if output.retained_byte_len() != output.observed_byte_len.min(capacity as u64) as usize
            {
                return Err(ExecutionContentError::Invalid(
                    "condition output differs from reserved prefix",
                ));
            }
        }
    }
    if let Body::ReservedOutput(result) = body {
        let reservation = records
            .iter()
            .find_map(|record| match &record.body {
                Body::ActivationOutputReservation(reservation)
                    if record.reference == result.reservation_ref =>
                {
                    Some(reservation)
                }
                _ => None,
            })
            .ok_or(ExecutionContentError::Invalid(
                "output has no prior reservation",
            ))?;
        if result.output.activation != reservation.activation {
            return Err(ExecutionContentError::OwnerMismatch);
        }
        let capacity = match result.slot {
            ActivationOutputSlot::Partial { sequence } => {
                let preceding = records
                    .iter()
                    .filter(|record| {
                        matches!(&record.body,
                    Body::ReservedOutput(old) if old.reservation_ref == result.reservation_ref
                        && matches!(old.slot, ActivationOutputSlot::Partial { .. }))
                    })
                    .count();
                if sequence >= reservation.limits.partial_records || sequence as usize != preceding
                {
                    return Err(ExecutionContentError::Invalid(
                        "partial output slot is out of order or unreserved",
                    ));
                }
                reservation.limits.partial_bytes
            }
            ActivationOutputSlot::Settlement => reservation.limits.settlement_bytes,
        };
        let retained = result.output.text.len();
        if retained > capacity
            || (result.original_byte_len <= capacity as u64
                && result.original_byte_len != retained as u64)
            || (result.original_byte_len > capacity as u64
                && capacity.saturating_sub(retained) >= 4)
        {
            return Err(ExecutionContentError::Invalid(
                "output does not match reserved UTF-8 prefix",
            ));
        }
    }
    if let Body::ToolResult(result) = body {
        let reservation = records
            .iter()
            .find_map(|record| match &record.body {
                Body::ToolReservation(reservation)
                    if record.reference == result.reservation_ref =>
                {
                    Some(reservation)
                }
                _ => None,
            })
            .ok_or(ExecutionContentError::Invalid(
                "result has no prior reservation",
            ))?;
        if result.retained_hex.len() / 2
            != (result
                .original_byte_len
                .min(reservation.result_capacity as u64)) as usize
        {
            return Err(ExecutionContentError::Invalid(
                "result does not match reserved prefix",
            ));
        }
    }
    Ok(())
}
fn validate_journal(
    data: &Journal,
    identity: &DurableSessionIdentity,
    limits: Limits,
) -> Result<(), ExecutionContentError> {
    if data.schema_version != SCHEMA {
        return Err(ExecutionContentError::Invalid("unsupported content schema"));
    }
    if data.journal_id != identity.journal_id() || &data.owner != identity.owner() {
        return Err(ExecutionContentError::OwnerMismatch);
    }
    if data.records.len() > limits.records {
        return Err(ExecutionContentError::Capacity);
    }
    let mut refs = HashSet::new();
    for (index, record) in data.records.iter().enumerate() {
        validate_body(&record.body, &data.owner)?;
        if !refs.insert(&record.reference)
            || content_reference(identity, &record.body)? != record.reference
        {
            return Err(ExecutionContentError::Invalid(
                "content reference mismatch or duplicate",
            ));
        }
        validate_next(&data.records[..index], &record.body)?;
    }
    validate_capacity(data, limits)
}
fn validate_capacity(data: &Journal, limits: Limits) -> Result<(), ExecutionContentError> {
    let settled: HashSet<_> = data
        .records
        .iter()
        .filter_map(|record| match &record.body {
            Body::ToolResult(value) => Some(&value.reservation_ref),
            Body::ConditionResult(value) => Some(&value.reservation_ref),
            _ => None,
        })
        .collect();
    let mut reserved_bytes = 0usize;
    let mut reserved_records = 0usize;
    for record in &data.records {
        if let Body::ConditionArguments(value) = &record.body {
            if !settled.contains(&record.reference) {
                reserved_records = reserved_records.saturating_add(1);
                reserved_bytes = reserved_bytes.saturating_add(
                    value
                        .definition
                        .stdout_bytes
                        .saturating_add(value.definition.stderr_bytes)
                        .saturating_mul(2)
                        .saturating_add(RESULT_OVERHEAD),
                );
            }
        }
        if let Body::ToolReservation(value) = &record.body {
            if !settled.contains(&record.reference) {
                reserved_records = reserved_records.saturating_add(1);
                reserved_bytes = reserved_bytes.saturating_add(
                    value
                        .result_capacity
                        .saturating_mul(2)
                        .saturating_add(RESULT_OVERHEAD),
                );
            }
        }
        if let Body::ActivationOutputReservation(value) = &record.body {
            let outputs = data
                .records
                .iter()
                .filter_map(|candidate| match &candidate.body {
                    Body::ReservedOutput(output) if output.reservation_ref == record.reference => {
                        Some(output)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !outputs
                .iter()
                .any(|output| output.slot == ActivationOutputSlot::Settlement)
            {
                let remaining =
                    (value.limits.partial_records as usize).saturating_sub(outputs.len());
                reserved_records = reserved_records.saturating_add(remaining).saturating_add(1);
                reserved_bytes = reserved_bytes
                    .saturating_add(
                        value
                            .limits
                            .partial_bytes
                            .saturating_mul(6)
                            .saturating_add(OUTPUT_OVERHEAD)
                            .saturating_mul(remaining),
                    )
                    .saturating_add(
                        value
                            .limits
                            .settlement_bytes
                            .saturating_mul(6)
                            .saturating_add(OUTPUT_OVERHEAD),
                    );
            }
        }
    }
    if data.records.len().saturating_add(reserved_records) > limits.records
        || encode_bounded(data, limits.bytes)?
            .len()
            .saturating_add(reserved_bytes)
            > limits.bytes
    {
        return Err(ExecutionContentError::Capacity);
    }
    Ok(())
}
fn content_reference(
    identity: &DurableSessionIdentity,
    body: &Body,
) -> Result<EvidenceRef, ExecutionContentError> {
    let encoded = encode_bounded(
        &(SCHEMA, identity.journal_id(), identity.owner(), body),
        MAX_BYTES,
    )?;
    EvidenceRef::new(format!("content-{}", sha256(&encoded)))
        .map_err(|_| ExecutionContentError::Invalid("content reference"))
}
fn bounded_name(value: &str) -> Result<(), ExecutionContentError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(ExecutionContentError::Invalid("invalid model reference"));
    }
    Ok(())
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: no arguments or failure sentinel on supported Unix platforms.
    unsafe { geteuid() }
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
fn unhex(value: &str) -> Result<Vec<u8>, ExecutionContentError> {
    if !value.len().is_multiple_of(2) || value.len() > MAX_TOOL_BYTES * 2 {
        return Err(ExecutionContentError::Invalid("invalid protected bytes"));
    }
    fn digit(byte: u8) -> Result<u8, ExecutionContentError> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(ExecutionContentError::Invalid("invalid protected bytes")),
        }
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect()
}
fn encode_bounded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, ExecutionContentError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(io::Error::other("content size limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: vec![],
        limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if writer.exceeded {
        return Err(ExecutionContentError::Capacity);
    }
    result?;
    Ok(writer.bytes)
}

fn bounded_vec<'de, D, T, const N: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T, const N: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for Visitor<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {N} retained elements")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while values.len() < N {
                match sequence.next_element()? {
                    Some(value) => values.push(value),
                    None => return Ok(values),
                }
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("retained element limit exceeded"));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Visitor::<T, N>(std::marker::PhantomData))
}
fn bounded_records<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Record>, D::Error> {
    bounded_vec::<D, Record, MAX_RECORDS>(deserializer)
}
fn bounded_check_argv<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    bounded_vec::<D, String, MAX_CHECK_ARGUMENTS>(deserializer)
}
fn bounded_condition_inputs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ConditionAcceptedInput>, D::Error> {
    bounded_vec::<D, ConditionAcceptedInput, MAX_CONTRACT_NODES>(deserializer)
}
fn bounded_contexts<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SessionTurnContextReference>, D::Error> {
    bounded_vec::<D, SessionTurnContextReference, MAX_CONTEXTS>(deserializer)
}
fn bounded_legacy_turns<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SessionTurn>, D::Error> {
    bounded_vec::<D, SessionTurn, MAX_LEGACY_TURNS>(deserializer)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    mod stream_tests {
        include!("execution_content_stream_tests.rs");
    }
    use crate::execution_ownership::LegacyFormatOwnership;
    use crate::execution_store::SessionExecutionStore;
    use crate::turn_contract::{
        ActivationId, ExecutionEpochId, SessionId, TurnContractEnvelope, TurnContractEvent,
        TurnNodeId,
    };
    use std::sync::Arc;

    fn canonical(root: &tempfile::TempDir) -> SessionExecutionStore {
        let guard = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        SessionExecutionStore::open(
            guard,
            ExecutionStoreOwner {
                workspace_id: "workspace-a".into(),
                session_id: SessionId::new("session-a").unwrap(),
            },
        )
        .unwrap()
    }
    fn request(id: &str, text_bytes: usize) -> Body {
        Body::Request(ExecutionRequestContent {
            turn_id: LogicalTurnId::new(id).unwrap(),
            recorded_at_unix_ms: 100,
            display_input: "a".repeat(text_bytes),
            effective_input: "b".repeat(text_bytes),
            context: vec![],
            target_definition: None,
            model: None,
        })
    }
    fn store(
        path: &Path,
        identity: DurableSessionIdentity,
        limits: Limits,
    ) -> ExecutionContentStore {
        let dir = SecureDir::open_existing_all(path).unwrap();
        dir.restrict_owner_only().unwrap();
        dir.try_lock_exclusive().unwrap();
        ExecutionContentStore::open_at(Storage::Standalone(dir), identity, limits).unwrap()
    }
    fn reservation() -> ToolReservation {
        ToolReservation {
            activation: ActivationRef {
                session_id: SessionId::new("session-a").unwrap(),
                turn_id: LogicalTurnId::new("turn-a").unwrap(),
                execution_epoch_id: ExecutionEpochId::new("epoch-a").unwrap(),
                node_id: TurnNodeId::new("node-a").unwrap(),
                generation: 1,
                activation_id: ActivationId::new("activation-a").unwrap(),
            },
            invocation_id: InvocationId::new("tool-a").unwrap(),
            arguments_hex: hex(b"arguments"),
            arguments_sha256: sha256(b"arguments"),
            result_capacity: 128,
        }
    }

    fn condition_fixture(
        canonical: &mut SessionExecutionStore,
        content: &mut ExecutionContentStore,
    ) -> (DurableTurnSnapshot, ConditionRunRef, EvidenceRef) {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
        )).unwrap();
        let mut events: Vec<TurnContractEnvelope> = fixture["steps"].as_array().unwrap()[..3]
            .iter()
            .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
            .collect();
        let definition = content
            .retain_repository_check_definition(RepositoryCheckDefinition {
                argv: vec!["cargo".into(), "test".into(), "argument with spaces".into()],
                timeout_ms: 30_000,
                stdout_bytes: 8,
                stderr_bytes: 4,
            })
            .unwrap();
        let repository = content
            .retain_activation_evidence(ActivationEvidenceContent::Repository {
                description: "retained checkout selection, no execution permission".into(),
                revision: Some("abc123".into()),
            })
            .unwrap();
        let TurnContractEvent::Begin { graph, .. } = &mut events[0].event else {
            panic!("begin fixture");
        };
        graph.conditions[0].kind = ConditionKind::RepositoryCheck {
            definition: definition.reference().clone(),
        };
        let condition_id = graph.conditions[0].condition_id.clone();
        canonical.append(events[0].clone()).unwrap();
        canonical.append(events[1].clone()).unwrap();
        let TurnContractEvent::AcceptActivation {
            activation, output, ..
        } = &mut events[2].event
        else {
            panic!("accept fixture");
        };
        let run = ConditionRunRef {
            session_id: activation.session_id.clone(),
            turn_id: activation.turn_id.clone(),
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id,
            run_id: ConditionRunId::new("check-run-a").unwrap(),
            activations: vec![activation.clone()],
        };
        *output = content
            .retain_output(
                &canonical.snapshot(&run.turn_id).unwrap(),
                ActivationOutputContent {
                    activation: activation.clone(),
                    recorded_at_unix_ms: 20,
                    text: "accepted answer".into(),
                    usage: ExecutionUsage::Unknown {
                        known_subtotal: TokenUsageStats::default(),
                    },
                    kind: OutputKind::Final,
                },
            )
            .unwrap()
            .reference()
            .clone();
        canonical.append(events[2].clone()).unwrap();
        (
            canonical.snapshot(&run.turn_id).unwrap(),
            run,
            repository.reference().clone(),
        )
    }

    fn condition_output(bytes: &[u8], capacity: usize, complete: bool) -> ConditionOutputEvidence {
        let mut capture = ConditionOutputCapture::new(capacity).unwrap();
        for chunk in bytes.chunks(3) {
            capture.observe(chunk).unwrap();
        }
        capture.finish(complete)
    }

    // Storage fixtures describe observations only. They cannot construct the
    // isolation layer's opaque live process-settlement receipt.
    fn condition_supervision(run: &ConditionRunRef) -> ConditionSupervisionEvidence {
        ConditionSupervisionEvidence {
            invocation_id: run.run_id.as_str().into(),
            request_sha256: sha256(b"storage fixture request"),
            runtime_identity: "storage-fixture-runtime".into(),
            program_sha256: sha256(b"storage fixture program identity"),
            transport_identity: "storage-fixture-transport".into(),
            launched: true,
            quiescent: true,
            primary_exit: Some(ConditionProcessStatus::Exited { code: 0 }),
        }
    }

    #[test]
    fn supervised_timeout_and_cancel_preserve_independent_primary_exit_after_reopen() {
        for (status, primary_exit) in [
            (
                ConditionProcessStatus::TimedOut,
                ConditionProcessStatus::Exited { code: 0 },
            ),
            (
                ConditionProcessStatus::Interrupted,
                ConditionProcessStatus::Signalled { signal: 15 },
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut canonical = canonical(&root);
            let dir = tempfile::tempdir().unwrap();
            let identity = canonical.identity().unwrap();
            let mut content = store(dir.path(), identity.clone(), Limits::default());
            let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
            let arguments = content
                .reserve_condition_arguments(&snapshot, &run, &repository)
                .unwrap();
            let mut evidence = condition_supervision(&run);
            evidence.primary_exit = Some(primary_exit.clone());
            let result = content
                .record_condition_supervised_result(
                    &arguments,
                    status.clone(),
                    condition_output(b"stdout\0\xff followed by suffix", 8, true),
                    condition_output(b"stderr suffix", 4, true),
                    100,
                    Some(evidence.clone()),
                )
                .unwrap();
            assert_eq!(result.status(), &status);
            assert_eq!(
                result.supervision().unwrap().primary_exit,
                Some(primary_exit)
            );
            assert!(result.stdout().is_truncated());
            assert_eq!(
                content
                    .record_condition_supervised_result(
                        &arguments,
                        status,
                        result.stdout().clone(),
                        result.stderr().clone(),
                        100,
                        Some(evidence.clone())
                    )
                    .unwrap(),
                result
            );
            drop(content);
            let content = store(dir.path(), identity, Limits::default());
            let restored = content.condition_result(&arguments).unwrap().unwrap();
            assert_eq!(restored, result);
            assert_eq!(restored.supervision(), Some(&evidence));
            assert!(canonical
                .snapshot(&run.turn_id)
                .unwrap()
                .contract()
                .conditions()
                .is_empty());
        }
    }

    #[test]
    fn supervised_result_rejects_conflicting_status_identity_and_bounds_before_retention() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let arguments = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let base = condition_supervision(&run);
        let mut invalid = Vec::new();
        for (field, value) in [
            (0, "another-run".to_string()),
            (0, "r".repeat(129)),
            (1, "".into()),
            (1, "r".repeat(257)),
            (2, "contains\0control".into()),
            (2, "t".repeat(257)),
            (3, "A".repeat(64)),
            (4, "0".repeat(63)),
        ] {
            let mut evidence = base.clone();
            match field {
                0 => evidence.invocation_id = value,
                1 => evidence.runtime_identity = value,
                2 => evidence.transport_identity = value,
                3 => evidence.request_sha256 = value,
                _ => evidence.program_sha256 = value,
            }
            invalid.push((ConditionProcessStatus::Exited { code: 0 }, evidence));
        }
        for primary in [
            None,
            Some(ConditionProcessStatus::Exited { code: 1 }),
            Some(ConditionProcessStatus::TimedOut),
            Some(ConditionProcessStatus::Signalled { signal: 65 }),
        ] {
            let mut evidence = base.clone();
            evidence.primary_exit = primary;
            invalid.push((ConditionProcessStatus::Exited { code: 0 }, evidence));
        }
        for status in [
            ConditionProcessStatus::Exited { code: -1 },
            ConditionProcessStatus::Exited { code: 256 },
            ConditionProcessStatus::Signalled { signal: 0 },
            ConditionProcessStatus::Signalled { signal: 65 },
            ConditionProcessStatus::LaunchFailed {
                message: "cannot launch an already-launched process".into(),
            },
            ConditionProcessStatus::NotDispatched,
        ] {
            invalid.push((status, base.clone()));
        }
        let mut undispatched = base.clone();
        undispatched.launched = false;
        undispatched.primary_exit = None;
        invalid.push((
            ConditionProcessStatus::Exited { code: 0 },
            undispatched.clone(),
        ));
        undispatched.quiescent = false;
        invalid.push((ConditionProcessStatus::NotDispatched, undispatched));
        for (status, evidence) in invalid {
            assert!(content
                .record_condition_supervised_result(
                    &arguments,
                    status,
                    condition_output(b"", 8, true),
                    condition_output(b"", 4, true),
                    100,
                    Some(evidence)
                )
                .is_err());
        }
        assert!(content.condition_result(&arguments).unwrap().is_none());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    }

    #[test]
    fn not_dispatched_requires_zero_observed_output_with_or_without_supervision() {
        for supervised in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut canonical = canonical(&root);
            let dir = tempfile::tempdir().unwrap();
            let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
            let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
            let arguments = content
                .reserve_condition_arguments(&snapshot, &run, &repository)
                .unwrap();
            let evidence = supervised.then(|| {
                let mut evidence = condition_supervision(&run);
                evidence.launched = false;
                evidence.primary_exit = None;
                evidence
            });
            for (stdout, stderr) in [
                (b"x".as_slice(), b"".as_slice()),
                (b"".as_slice(), b"x".as_slice()),
            ] {
                assert!(content
                    .record_condition_supervised_result(
                        &arguments,
                        ConditionProcessStatus::NotDispatched,
                        condition_output(stdout, 8, true),
                        condition_output(stderr, 4, true),
                        100,
                        evidence.clone()
                    )
                    .is_err());
            }
            assert!(content.condition_result(&arguments).unwrap().is_none());
            let result = content
                .record_condition_supervised_result(
                    &arguments,
                    ConditionProcessStatus::NotDispatched,
                    condition_output(b"", 8, true),
                    condition_output(b"", 4, true),
                    100,
                    evidence.clone(),
                )
                .unwrap();
            assert_eq!(result.supervision(), evidence.as_ref());
            assert_eq!(result.stdout().observed_byte_len(), 0);
            assert_eq!(result.stderr().observed_byte_len(), 0);
        }
    }

    #[test]
    fn maximal_supervision_metadata_settles_reserved_capacity_and_reopens() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let (snapshot, mut run, repository) = condition_fixture(&mut canonical, &mut content);
        run.run_id = ConditionRunId::new("r".repeat(128)).unwrap();
        let arguments = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let limits = Limits {
            bytes: encode_bounded(&content.data, MAX_BYTES).unwrap().len() + RESULT_OVERHEAD + 24,
            records: content.data.records.len() + 1,
        };
        content.limits = limits;
        assert!(matches!(
            content.append(request("unrelated", 0)),
            Err(ExecutionContentError::Capacity)
        ));
        drop(content);
        let mut content = store(dir.path(), identity.clone(), limits);
        let mut evidence = condition_supervision(&run);
        evidence.runtime_identity = "\"".repeat(256);
        evidence.transport_identity = "\\".repeat(256);
        evidence.quiescent = false;
        evidence.primary_exit = Some(ConditionProcessStatus::Signalled { signal: 64 });
        let result = content
            .record_condition_supervised_result(
                &arguments,
                ConditionProcessStatus::Uncertain {
                    message: "\u{1}".repeat(512),
                },
                condition_output(b"123456789", 8, false),
                condition_output(b"abcdef", 4, false),
                u64::MAX,
                Some(evidence.clone()),
            )
            .unwrap();
        drop(content);
        let content = store(dir.path(), identity, limits);
        assert_eq!(content.condition_result(&arguments).unwrap(), Some(result));
    }

    #[test]
    fn reopening_rejects_hash_consistent_supervision_for_another_run() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let arguments = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let result = content
            .record_condition_supervised_result(
                &arguments,
                ConditionProcessStatus::Exited { code: 0 },
                condition_output(b"", 8, true),
                condition_output(b"", 4, true),
                100,
                Some(condition_supervision(&run)),
            )
            .unwrap();
        let mut data = content.data.clone();
        let record = data
            .records
            .iter_mut()
            .find(|record| record.reference == *result.reference())
            .unwrap();
        let Body::ConditionResult(result) = &mut record.body else {
            panic!("condition result");
        };
        result.supervision.as_mut().unwrap().invocation_id = "another-run".into();
        record.reference = content_reference(&identity, &record.body).unwrap();
        drop(content);
        std::fs::write(
            dir.path().join(FILE),
            encode_bounded(&data, MAX_BYTES).unwrap(),
        )
        .unwrap();
        let storage = SecureDir::open_existing_all(dir.path()).unwrap();
        storage.restrict_owner_only().unwrap();
        storage.try_lock_exclusive().unwrap();
        assert!(matches!(
            ExecutionContentStore::open_at(
                Storage::Standalone(storage),
                identity,
                Limits::default()
            ),
            Err(ExecutionContentError::Invalid(
                "supervised result belongs to another condition run"
            ))
        ));
    }

    #[test]
    fn condition_reservation_selects_exact_inputs_without_executing_or_creating_intent() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let canonical_before = std::fs::read(canonical.path()).unwrap();
        let receipt = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        assert_eq!(receipt.run(), &run);
        assert_eq!(
            receipt.inputs()[0].input,
            snapshot.contract().activations()[0].input
        );
        assert_eq!(
            receipt.inputs()[0].checkpoint,
            snapshot.contract().activations()[0]
                .checkpoint
                .clone()
                .unwrap()
        );
        assert_eq!(
            receipt.definition().argv,
            ["cargo", "test", "argument with spaces"]
        );
        assert!(content.condition_result(&receipt).unwrap().is_none());
        assert!(snapshot.contract().condition_runs().is_empty());
        assert_eq!(std::fs::read(canonical.path()).unwrap(), canonical_before);
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        assert_eq!(
            content
                .reserve_condition_arguments(&snapshot, &run, &repository)
                .unwrap(),
            receipt
        );
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        assert_eq!(
            content.condition_arguments(&snapshot, &run.run_id).unwrap(),
            Some(receipt)
        );
    }

    #[test]
    fn condition_arguments_reject_missing_foreign_wrong_role_and_stale_inputs_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let missing = EvidenceRef::new("missing-repository").unwrap();
        assert!(content
            .reserve_condition_arguments(&snapshot, &run, &missing)
            .is_err());
        let definition = match &snapshot.contract().graph().unwrap().conditions[0].kind {
            ConditionKind::RepositoryCheck { definition } => definition.clone(),
            _ => panic!("check"),
        };
        assert!(content
            .reserve_condition_arguments(&snapshot, &run, &definition)
            .is_err());
        let mut invalid = run.clone();
        invalid.activations[0].generation += 1;
        assert!(content
            .reserve_condition_arguments(&snapshot, &invalid, &repository)
            .is_err());
        invalid = run.clone();
        invalid.epoch_id = ExecutionEpochId::new("stale").unwrap();
        assert!(content
            .reserve_condition_arguments(&snapshot, &invalid, &repository)
            .is_err());
        invalid = run.clone();
        invalid.turn_id = LogicalTurnId::new("foreign-turn").unwrap();
        assert!(content
            .reserve_condition_arguments(&snapshot, &invalid, &repository)
            .is_err());
        invalid = run.clone();
        invalid.activations.push(invalid.activations[0].clone());
        assert!(content
            .reserve_condition_arguments(&snapshot, &invalid, &repository)
            .is_err());
        let other_root = tempfile::tempdir().unwrap();
        let other = self::canonical(&other_root);
        let other_dir = tempfile::tempdir().unwrap();
        let mut other_content = store(
            other_dir.path(),
            other.identity().unwrap(),
            Limits::default(),
        );
        assert!(matches!(
            other_content.reserve_condition_arguments(&snapshot, &run, &repository),
            Err(ExecutionContentError::OwnerMismatch)
        ));
        // Even an internally retained canonical reference cannot replace the
        // physical output body required by a check reservation.
        let removed = content
            .data
            .records
            .iter()
            .position(|record| matches!(record.body, Body::Output(_)))
            .unwrap();
        let output = content.data.records.remove(removed);
        assert!(content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .is_err());
        content.data.records.insert(removed, output);
        let index = content
            .data
            .records
            .iter()
            .position(|record| record.reference == definition)
            .unwrap();
        let definition_record = content.data.records.remove(index);
        assert!(content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .is_err());
        content.data.records.insert(index, definition_record);
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    }

    #[test]
    fn condition_late_result_retains_real_status_observed_prefix_and_exact_duplicate() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let receipt = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        canonical
            .append(TurnContractEnvelope {
                schema_version: 2,
                command_id: crate::turn_contract::CommandId::new("close-before-late-result")
                    .unwrap(),
                expected_revision: snapshot.contract().revision(),
                session_id: run.session_id.clone(),
                turn_id: run.turn_id.clone(),
                event: TurnContractEvent::Close {
                    closure: crate::turn_contract::TurnClosure::Finished,
                },
            })
            .unwrap();
        let stdout = condition_output(b"abcdefghijk\0\xff", 8, true);
        let stderr = ConditionOutputEvidence::from_observed_transport(
            b"oops",
            8,
            sha256(b"oopsmore"),
            false,
        )
        .unwrap();
        let result = content
            .record_condition_result(
                &receipt,
                ConditionProcessStatus::Exited { code: 7 },
                stdout.clone(),
                stderr.clone(),
                100,
            )
            .unwrap();
        assert_eq!(result.status(), &ConditionProcessStatus::Exited { code: 7 });
        assert_eq!(result.stdout().retained_bytes().unwrap(), b"abcdefgh");
        assert_eq!(
            result.stdout().observed_sha256(),
            sha256(b"abcdefghijk\0\xff")
        );
        assert!(result.stdout().complete());
        assert!(result.stdout().is_truncated());
        assert!(!result.stderr().complete());
        assert_eq!(
            result.stderr().source(),
            ConditionOutputSource::ObservedTransport
        );
        assert_eq!(
            content
                .record_condition_result(
                    &receipt,
                    ConditionProcessStatus::Exited { code: 7 },
                    stdout.clone(),
                    stderr.clone(),
                    100
                )
                .unwrap(),
            result
        );
        assert!(matches!(
            content.record_condition_result(
                &receipt,
                ConditionProcessStatus::Exited { code: 0 },
                stdout,
                stderr,
                100
            ),
            Err(ExecutionContentError::Conflict)
        ));
        drop(content);
        let content = store(dir.path(), identity, Limits::default());
        assert_eq!(content.condition_result(&receipt).unwrap(), Some(result));
        let closed = canonical.snapshot(&run.turn_id).unwrap();
        assert_eq!(
            content.condition_arguments(&closed, &run.run_id).unwrap(),
            Some(receipt)
        );
        assert_eq!(closed.contract().state(), Some(LogicalTurnState::Finished));
        assert!(closed.contract().conditions().is_empty());
    }

    #[test]
    fn condition_pending_intent_and_fresh_observation_refuse_another_run() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let arguments = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let mut append = |name: &str, event: TurnContractEvent| {
            let current = canonical.snapshot(&run.turn_id).unwrap();
            canonical
                .append(TurnContractEnvelope {
                    schema_version: 2,
                    command_id: crate::turn_contract::CommandId::new(name).unwrap(),
                    expected_revision: current.contract().revision(),
                    session_id: run.session_id.clone(),
                    turn_id: run.turn_id.clone(),
                    event,
                })
                .unwrap();
            canonical.snapshot(&run.turn_id).unwrap()
        };
        let pending = append(
            "condition-intent",
            TurnContractEvent::RecordConditionIntent {
                run: run.clone(),
                intent: arguments.reference().clone(),
            },
        );
        let mut second = run.clone();
        second.run_id = ConditionRunId::new("check-run-b").unwrap();
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(content
            .reserve_condition_arguments(&pending, &second, &repository)
            .is_err());
        assert_eq!(
            content
                .reserve_condition_arguments(&pending, &run, &repository)
                .unwrap(),
            arguments
        );
        append(
            "condition-not-dispatched",
            TurnContractEvent::ResolveConditionIntent {
                run_id: run.run_id.clone(),
                resolution: crate::turn_contract::ConditionEffectResolution::NotDispatched {
                    evidence: EvidenceRef::new("host-confirmed-no-process").unwrap(),
                },
            },
        );
        let observed = append(
            "condition-failed-observation",
            TurnContractEvent::RecordCondition {
                epoch_id: run.epoch_id.clone(),
                condition_id: run.condition_id.clone(),
                activations: run.activations.clone(),
                outcome: crate::turn_contract::ConditionOutcome::Failed,
                evidence: EvidenceRef::new("host-review").unwrap(),
            },
        );
        assert!(content
            .reserve_condition_arguments(&observed, &second, &repository)
            .is_err());
        assert!(content.condition_result(&arguments).unwrap().is_none());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    }

    #[test]
    fn condition_result_requires_same_namespace_and_exact_reserved_prefix() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let receipt = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let other_root = tempfile::tempdir().unwrap();
        let other = self::canonical(&other_root);
        let other_dir = tempfile::tempdir().unwrap();
        let mut other_content = store(
            other_dir.path(),
            other.identity().unwrap(),
            Limits::default(),
        );
        assert!(matches!(
            other_content.record_condition_result(
                &receipt,
                ConditionProcessStatus::Exited { code: 0 },
                condition_output(b"", 8, true),
                condition_output(b"", 4, true),
                1
            ),
            Err(ExecutionContentError::OwnerMismatch)
        ));
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(content
            .record_condition_result(
                &receipt,
                ConditionProcessStatus::Exited { code: 0 },
                condition_output(b"0123456789", 9, true),
                condition_output(b"", 4, true),
                1
            )
            .is_err());
        assert!(content
            .record_condition_result(
                &receipt,
                ConditionProcessStatus::Exited { code: 0 },
                condition_output(b"0123456789", 7, true),
                condition_output(b"", 4, true),
                1
            )
            .is_err());
        assert!(content.condition_result(&receipt).unwrap().is_none());
        assert!(content
            .record_condition_result(
                &receipt,
                ConditionProcessStatus::Uncertain {
                    message: "x".repeat(513)
                },
                condition_output(b"", 8, false),
                condition_output(b"", 4, false),
                1
            )
            .is_err());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    }

    #[test]
    fn condition_settlement_bytes_and_slot_survive_capacity_pressure_and_reopen() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let receipt = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let limits = Limits {
            bytes: encode_bounded(&content.data, MAX_BYTES).unwrap().len() + RESULT_OVERHEAD + 24,
            records: content.data.records.len() + 1,
        };
        content.limits = limits;
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(matches!(
            content.append(request("unrelated", 0)),
            Err(ExecutionContentError::Capacity)
        ));
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        drop(content);
        let mut content = store(dir.path(), identity.clone(), limits);
        let result = content
            .record_condition_result(
                &receipt,
                ConditionProcessStatus::LaunchFailed {
                    message: "\u{1}".repeat(512),
                },
                condition_output(b"123456789", 8, false),
                condition_output(b"abcdef", 4, false),
                u64::MAX,
            )
            .unwrap();
        assert!(content.condition_result(&receipt).unwrap().is_some());
        drop(content);
        let content = store(dir.path(), identity, limits);
        assert_eq!(content.condition_result(&receipt).unwrap(), Some(result));
    }

    #[test]
    fn condition_definition_and_observation_bounds_fail_before_retention() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let valid = RepositoryCheckDefinition {
            argv: vec!["check".into()],
            timeout_ms: 1,
            stdout_bytes: 1,
            stderr_bytes: 0,
        };
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let mut variants = Vec::new();
        let mut value = valid.clone();
        value.argv.clear();
        variants.push(value);
        let mut value = valid.clone();
        value.argv[0].push('\0');
        variants.push(value);
        let mut value = valid.clone();
        value.argv.push("x".repeat(MAX_CHECK_ARGUMENT_BYTES));
        variants.push(value);
        let mut value = valid.clone();
        value.timeout_ms = 0;
        variants.push(value);
        let mut value = valid.clone();
        value.stdout_bytes = usize::MAX;
        variants.push(value);
        for value in variants {
            assert!(content.retain_repository_check_definition(value).is_err());
        }
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        assert!(ConditionOutputCapture::new(MAX_TOOL_BYTES + 1).is_err());
        assert!(ConditionOutputEvidence::from_observed_transport(
            b"text",
            3,
            sha256(b"text"),
            true
        )
        .is_err());
        assert!(ConditionOutputEvidence::from_observed_transport(
            b"text",
            4,
            sha256(b"wrong"),
            true
        )
        .is_err());
        assert!(!condition_output(b"", 0, false).complete());
    }

    #[test]
    fn condition_uncertain_reservation_and_result_ack_require_reopen_without_inventing_success() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let receipt = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        let mut arguments = receipt.arguments.clone();
        arguments.run.run_id = ConditionRunId::new("lost-reservation-ack").unwrap();
        assert!(matches!(
            content.append_with(
                Body::ConditionArguments(arguments.clone()),
                |storage, bytes| {
                    storage.write(bytes)?;
                    Err(io::Error::other("lost reservation acknowledgement"))
                }
            ),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        assert!(matches!(
            content.condition_arguments(&snapshot, &arguments.run.run_id),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        drop(content);
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let receipt = content
            .condition_arguments(&snapshot, &arguments.run.run_id)
            .unwrap()
            .unwrap();
        assert!(content.condition_result(&receipt).unwrap().is_none());
        let result = ConditionResult {
            reservation_ref: receipt.reference().clone(),
            supervision: None,
            status: ConditionProcessStatus::Interrupted,
            stdout: condition_output(b"observed", 8, false),
            stderr: condition_output(b"", 4, false),
            recorded_at_unix_ms: 100,
        };
        assert!(matches!(
            content.append_with(Body::ConditionResult(result.clone()), |storage, bytes| {
                storage.write(bytes)?;
                Err(io::Error::other("lost result acknowledgement"))
            }),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        assert!(matches!(
            content.condition_result(&receipt),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        drop(content);
        let content = store(dir.path(), identity, Limits::default());
        assert_eq!(
            content.condition_result(&receipt).unwrap().unwrap().result,
            result
        );
        assert!(snapshot.contract().conditions().is_empty());
    }

    #[test]
    fn owned_condition_intent_crash_prefix_reopens_unknown_and_never_implies_dispatch_permission() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let owner = canonical.owner().clone();
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let (snapshot, run, repository) = condition_fixture(&mut canonical, &mut content);
        let receipt = content
            .reserve_condition_arguments(&snapshot, &run, &repository)
            .unwrap();
        canonical
            .append(TurnContractEnvelope {
                schema_version: 2,
                command_id: crate::turn_contract::CommandId::new("intent-before-crash").unwrap(),
                expected_revision: snapshot.contract().revision(),
                session_id: run.session_id.clone(),
                turn_id: run.turn_id.clone(),
                event: TurnContractEvent::RecordConditionIntent {
                    run: run.clone(),
                    intent: receipt.reference().clone(),
                },
            })
            .unwrap();
        drop(content);
        drop(canonical);
        let guard = Arc::new(
            crate::execution_ownership::UpgradedFormatOwnership::open(root.path()).unwrap(),
        );
        let canonical = SessionExecutionStore::open(guard, owner).unwrap();
        let recovered = canonical.snapshot(&run.turn_id).unwrap();
        assert_eq!(
            recovered.contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        assert!(recovered.contract().has_unknown_effects());
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            content
                .condition_arguments(&recovered, &run.run_id)
                .unwrap(),
            Some(receipt.clone())
        );
        assert!(content.condition_result(&receipt).unwrap().is_none());
        let mut another = run.clone();
        another.run_id = ConditionRunId::new("unsafe-replay").unwrap();
        assert!(content
            .reserve_condition_arguments(&recovered, &another, &repository)
            .is_err());
        // Actual late observed evidence is retainable, but this content store
        // cannot resolve canonical uncertainty or mark a condition as passed.
        let result = content
            .record_condition_result(
                &receipt,
                ConditionProcessStatus::Exited { code: 0 },
                condition_output(b"ok", 8, true),
                condition_output(b"", 4, true),
                123,
            )
            .unwrap();
        assert_eq!(content.condition_result(&receipt).unwrap(), Some(result));
        let unchanged = canonical.snapshot(&run.turn_id).unwrap();
        assert!(unchanged.contract().has_unknown_effects());
        assert!(unchanged.contract().conditions().is_empty());
    }

    fn proposed_input_fixture(
        canonical: &mut SessionExecutionStore,
        content: &mut ExecutionContentStore,
    ) -> (DurableTurnSnapshot, TurnContractEnvelope) {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/partial_finish_is_not_success.json"
        ))
        .unwrap();
        let mut events: Vec<TurnContractEnvelope> = fixture["steps"].as_array().unwrap()[..2]
            .iter()
            .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
            .collect();
        let TurnContractEvent::StartActivation { input } = &mut events[1].event else {
            panic!("fixture must prepare StartActivation")
        };
        let profile = ExecutionProfile {
            definition: input.definition.definition_id.as_str().into(),
            provider: "provider".into(),
            model: "model".into(),
            isolation: "local".into(),
            tools: vec![],
            write_scope: None,
        };
        let limits = GrantLimits {
            activations: 2,
            invocations: 2,
            tokens: 1000,
            cost_microunits: 0,
        };
        let definition = content
            .retain_activation_evidence(ActivationEvidenceContent::Definition {
                definition_id: input.definition.definition_id.clone(),
                revision: 1,
                profile: profile.clone(),
                configuration: "{}".into(),
            })
            .unwrap();
        input.definition.snapshot = definition.reference().clone();
        let grant = input.grant.as_mut().unwrap();
        grant.evidence = content
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: AuthorityGrant {
                    id: grant.grant_id.as_str().into(),
                    revision: grant.revision,
                    issuer_evidence: EvidenceRef::new("issuer").unwrap(),
                    holder: input.activation.node_id.clone(),
                    descendants: vec![],
                    allow_stop_descendants: false,
                    delegation: None,
                    conditions: vec![],
                    profiles: vec![profile],
                    limits: limits.clone(),
                    expires_at_ms: 1000,
                },
            })
            .unwrap()
            .reference()
            .clone();
        input.budget = content
            .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
            .unwrap()
            .reference()
            .clone();
        input.attachments = vec![content
            .retain_activation_evidence(ActivationEvidenceContent::Attachment {
                reference_id: "retained-text".into(),
                media_type: "text/plain".into(),
                text: "captured text".into(),
            })
            .unwrap()
            .reference()
            .clone()];
        input.repository = RepositoryInput::Recorded {
            snapshot: content
                .retain_activation_evidence(ActivationEvidenceContent::Repository {
                    description: "descriptive evidence, not runtime authority".into(),
                    revision: None,
                })
                .unwrap()
                .reference()
                .clone(),
        };
        let Body::Request(body) = request(input.activation.turn_id.as_str(), 8) else {
            panic!("request helper must return request content")
        };
        let request = content.retain_request(body).unwrap();
        input.guidance = vec![request.reference().clone()];
        let TurnContractEvent::Begin { graph, .. } = &mut events[0].event else {
            panic!("fixture must begin a graph")
        };
        graph.nodes[0].definition.snapshot = definition.reference().clone();
        canonical
            .begin_with_request(events[0].clone(), &request)
            .unwrap();
        (
            canonical.snapshot(request.turn_id()).unwrap(),
            events.remove(1),
        )
    }

    #[test]
    fn proposed_input_resolution_is_read_only_and_does_not_admit_an_activation() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, start) = proposed_input_fixture(&mut canonical, &mut content);
        let TurnContractEvent::StartActivation { input } = &start.event else {
            panic!("start")
        };
        let before_content = std::fs::read(dir.path().join(FILE)).unwrap();
        let before_canonical = std::fs::read(canonical.path()).unwrap();
        let resolved = content.validate_proposed_input(&snapshot, input).unwrap();
        assert_eq!(resolved.guidance, vec!["b".repeat(8)]);
        assert_eq!(resolved.attachments.len(), 1);
        assert!(resolved.grant.is_some());
        assert!(resolved.repository.is_some());
        assert_eq!(
            content.validate_proposed_input(&snapshot, input).unwrap(),
            resolved
        );
        assert!(content.validate_input(&snapshot, input).is_err());
        assert!(canonical
            .snapshot(snapshot.turn_id())
            .unwrap()
            .contract()
            .activations()
            .is_empty());
        assert_eq!(
            std::fs::read(dir.path().join(FILE)).unwrap(),
            before_content
        );
        assert_eq!(std::fs::read(canonical.path()).unwrap(), before_canonical);
        canonical.append(start.clone()).unwrap();
        let admitted = canonical.snapshot(snapshot.turn_id()).unwrap();
        assert_eq!(content.validate_input(&admitted, input).unwrap(), resolved);
        let mut changed = input.clone();
        changed.manifest_id =
            crate::turn_contract::InputManifestId::new("unrecorded-input").unwrap();
        assert_eq!(
            content
                .validate_proposed_input(&admitted, &changed)
                .unwrap(),
            resolved
        );
        assert!(matches!(
            content.validate_input(&admitted, &changed),
            Err(ExecutionContentError::Conflict)
        ));
        assert_eq!(
            std::fs::read(dir.path().join(FILE)).unwrap(),
            before_content
        );
    }

    #[test]
    fn proposed_input_resolver_checks_roles_and_exact_retained_identity_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let (snapshot, start) = proposed_input_fixture(&mut canonical, &mut content);
        let TurnContractEvent::StartActivation { input } = &start.event else {
            panic!("start")
        };
        let before_content = std::fs::read(dir.path().join(FILE)).unwrap();
        let before_canonical = std::fs::read(canonical.path()).unwrap();
        for case in [
            "definition",
            "definition_id",
            "grant",
            "grant_revision",
            "budget",
            "guidance",
            "attachment",
            "repository",
        ] {
            let mut proposed = input.clone();
            match case {
                "definition" => proposed.definition.snapshot = input.budget.clone(),
                "definition_id" => {
                    proposed.definition.definition_id =
                        AgentDefinitionId::new("foreign-definition").unwrap()
                }
                "grant" => proposed.grant.as_mut().unwrap().evidence = input.budget.clone(),
                "grant_revision" => proposed.grant.as_mut().unwrap().revision += 1,
                "budget" => proposed.budget = input.attachments[0].clone(),
                "guidance" => proposed.guidance = vec![input.budget.clone()],
                "attachment" => proposed.attachments = vec![input.budget.clone()],
                "repository" => {
                    proposed.repository = RepositoryInput::Recorded {
                        snapshot: input.budget.clone(),
                    }
                }
                _ => panic!("unknown case"),
            }
            assert!(
                matches!(
                    content.validate_proposed_input(&snapshot, &proposed),
                    Err(ExecutionContentError::Invalid(_))
                ),
                "{case}"
            );
            assert_eq!(
                std::fs::read(dir.path().join(FILE)).unwrap(),
                before_content,
                "{case}"
            );
            assert_eq!(
                std::fs::read(canonical.path()).unwrap(),
                before_canonical,
                "{case}"
            );
        }
    }

    #[test]
    fn proposed_input_resolution_rejects_foreign_owner_turn_journal_and_request() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical_store = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(
            dir.path(),
            canonical_store.identity().unwrap(),
            Limits::default(),
        );
        let (snapshot, start) = proposed_input_fixture(&mut canonical_store, &mut content);
        let TurnContractEvent::StartActivation { input } = &start.event else {
            panic!("start")
        };
        for foreign_session in [false, true] {
            let mut proposed = input.clone();
            if foreign_session {
                proposed.activation.session_id = SessionId::new("foreign-session").unwrap();
            } else {
                proposed.activation.turn_id = LogicalTurnId::new("foreign-turn").unwrap();
            }
            assert!(matches!(
                content.validate_proposed_input(&snapshot, &proposed),
                Err(ExecutionContentError::OwnerMismatch)
            ));
        }
        let foreign_root = tempfile::tempdir().unwrap();
        let mut foreign_canonical = canonical(&foreign_root);
        let foreign_dir = tempfile::tempdir().unwrap();
        let mut foreign_content = store(
            foreign_dir.path(),
            foreign_canonical.identity().unwrap(),
            Limits::default(),
        );
        let (foreign_snapshot, _) =
            proposed_input_fixture(&mut foreign_canonical, &mut foreign_content);
        // Session and turn names match, but the retained journal owner does not.
        assert_eq!(snapshot.owner(), foreign_snapshot.owner());
        assert_eq!(snapshot.turn_id(), foreign_snapshot.turn_id());
        assert!(matches!(
            content.validate_proposed_input(&foreign_snapshot, input),
            Err(ExecutionContentError::OwnerMismatch)
        ));
        assert!(matches!(
            foreign_content.validate_proposed_input(&snapshot, input),
            Err(ExecutionContentError::OwnerMismatch)
        ));
        let Body::Request(other_request) = request("foreign-turn", 8) else {
            panic!("request")
        };
        let other = content.retain_request(other_request).unwrap();
        let mut wrong_request = input.clone();
        wrong_request.guidance = vec![other.reference().clone()];
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(matches!(
            content.validate_proposed_input(&snapshot, &wrong_request),
            Err(ExecutionContentError::Invalid(_))
        ));
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), bytes);
    }

    #[test]
    fn legacy_standing_check_record_still_loads() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let limits = Limits {
            bytes: 64 * 1024,
            records: 100,
        };
        // A check result as the removed standing-work inbox recorded it.
        let recorded = |activation: &ActivationRef| -> Body {
            serde_json::from_value(serde_json::json!({
                "record_kind": "standing_repository_check",
                "activation": activation,
                "index": 0,
                "argv": ["npm", "test"],
                "before": "before-capture",
                "after": "after-capture",
                "invocation": "tool-a",
                "outcome": "check-outcome",
                "exit_code": 0,
                "candidate_sha256": null,
                "passed": true,
            }))
            .unwrap()
        };
        let activation = reservation().activation;
        let mut content = store(dir.path(), identity.clone(), limits);
        content.append(recorded(&activation)).unwrap();
        content.append(request("turn-after", 16)).unwrap();
        let mut foreign = activation.clone();
        foreign.session_id = SessionId::new("session-b").unwrap();
        assert!(matches!(
            content.append(recorded(&foreign)),
            Err(ExecutionContentError::OwnerMismatch)
        ));
        drop(content);
        let content = store(dir.path(), identity, limits);
        assert!(content
            .data
            .records
            .iter()
            .any(|record| matches!(&record.body, Body::StandingRepositoryCheck(check) if check.activation == activation)));
        assert!(content
            .data
            .records
            .iter()
            .any(|record| matches!(&record.body, Body::Request(request) if request.turn_id.as_str() == "turn-after")));
    }

    #[test]
    fn admission_fills_available_bytes_but_reserved_result_still_settles_and_reopens() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let limits = Limits {
            bytes: 24 * 1024,
            records: 100,
        };
        let mut content = store(dir.path(), identity.clone(), limits);
        let reservation = reservation();
        let reference = content
            .append(Body::ToolReservation(reservation.clone()))
            .unwrap();
        let receipt = content.arguments_receipt(reference, &reservation);
        let mut admitted = 0;
        for index in 0..100 {
            match content.append(request(&format!("turn-{index}"), 800)) {
                Ok(_) => admitted += 1,
                Err(ExecutionContentError::Capacity) => break,
                Err(error) => panic!("unexpected admission error: {error}"),
            }
        }
        assert!(admitted > 0 && admitted < 100);
        let result = content
            .record_tool_result(&receipt, InvocationOutcome::Succeeded, &[0xff; 512], 101)
            .unwrap();
        assert_eq!(content.read_tool_result(&result).unwrap(), vec![0xff; 128]);
        drop(content);
        let content = store(dir.path(), identity, limits);
        assert_eq!(content.tool_result(&receipt).unwrap(), Some(result));
    }

    #[test]
    fn result_record_slot_is_reserved_and_underfunded_loaded_history_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let limits = Limits {
            bytes: 24 * 1024,
            records: 2,
        };
        let mut content = store(dir.path(), identity.clone(), limits);
        let reservation = reservation();
        let reference = content
            .append(Body::ToolReservation(reservation.clone()))
            .unwrap();
        let receipt = content.arguments_receipt(reference, &reservation);
        assert!(matches!(
            content.append(request("extra", 1)),
            Err(ExecutionContentError::Capacity)
        ));
        let encoded = encode_bounded(&content.data, limits.bytes).unwrap();
        assert!(matches!(
            validate_journal(
                &content.data,
                &identity,
                Limits {
                    bytes: encoded.len(),
                    records: 2
                }
            ),
            Err(ExecutionContentError::Capacity)
        ));
        content
            .record_tool_result(&receipt, InvocationOutcome::Succeeded, b"done", 101)
            .unwrap();
        assert_eq!(content.data.records.len(), 2);
    }

    #[test]
    fn uncertain_post_rename_write_poisoning_requires_durable_reopen_before_receipt() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = canonical.identity().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let body = request("turn-a", 20);
        assert!(matches!(
            content.append_with(body.clone(), |storage, bytes| {
                storage.write(bytes)?;
                Err(io::Error::other("simulated lost acknowledgement"))
            }),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        assert!(matches!(
            content.append(body.clone()),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        drop(content);
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        assert_eq!(
            content.append(body.clone()).unwrap(),
            content_reference(&identity, &body).unwrap()
        );
        assert_eq!(content.data.records.len(), 1);
    }

    fn output_reservation() -> ActivationOutputReservation {
        ActivationOutputReservation {
            activation: reservation().activation,
            limits: ActivationOutputLimits {
                partial_records: 2,
                partial_bytes: 256,
                settlement_bytes: 128,
            },
        }
    }

    fn reserved_text(
        reservation: &DurableActivationOutputReservation,
        text: &str,
    ) -> ActivationOutputContent {
        ActivationOutputContent {
            activation: reservation.activation.clone(),
            recorded_at_unix_ms: 100,
            text: text.to_string(),
            kind: OutputKind::Partial,
            usage: ExecutionUsage::Unknown {
                known_subtotal: TokenUsageStats::default(),
            },
        }
    }

    #[test]
    fn output_settlement_survives_unrelated_capacity_pressure_and_worst_case_json() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let identity = canonical.identity().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            bytes: 64 * 1024,
            records: 100,
        };
        let mut content = store(dir.path(), identity.clone(), limits);
        let body = output_reservation();
        let reference = content
            .append(Body::ActivationOutputReservation(body.clone()))
            .unwrap();
        let receipt = content.output_reservation_receipt(reference, &body);
        let mut admitted = 0;
        for index in 0..100 {
            match content.append(request(&format!("extra-{index}"), 800)) {
                Ok(_) => admitted += 1,
                Err(ExecutionContentError::Capacity) => break,
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
        assert!(admitted > 0 && admitted < 100);
        let raw = reserved_text(&receipt, &"\0".repeat(1024));
        for sequence in 0..2 {
            content
                .record_activation_partial(&receipt, sequence, raw.clone())
                .unwrap();
        }
        assert!(matches!(
            content.record_activation_partial(&receipt, 2, raw.clone()),
            Err(ExecutionContentError::Capacity)
        ));
        let settled = content
            .settle_activation_output(&receipt, raw.clone())
            .unwrap();
        assert_eq!(settled.content.output.text, "\0".repeat(128));
        assert_eq!(settled.content.original_sha256, sha256(raw.text.as_bytes()));
        assert_eq!(settled.content.output.usage, raw.usage);
        assert!(settled.complete_output().is_none());
        drop(content);
        let mut content = store(dir.path(), identity, limits);
        assert_eq!(
            content.activation_output_settlement(&receipt).unwrap(),
            Some(settled.clone())
        );
        assert_eq!(
            content
                .settle_activation_output(&receipt, raw.clone())
                .unwrap(),
            settled
        );
        let mut changed = raw;
        changed.recorded_at_unix_ms += 1;
        assert!(matches!(
            content.settle_activation_output(&receipt, changed),
            Err(ExecutionContentError::Conflict)
        ));
    }

    #[test]
    fn output_record_reservation_rejects_underfunded_reopen_and_forged_receipts() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let identity = canonical.identity().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            bytes: 64 * 1024,
            records: 4,
        };
        let mut content = store(dir.path(), identity.clone(), limits);
        let body = output_reservation();
        let reference = content
            .append(Body::ActivationOutputReservation(body.clone()))
            .unwrap();
        let receipt = content.output_reservation_receipt(reference, &body);
        assert!(matches!(
            content.append(request("unrelated", 1)),
            Err(ExecutionContentError::Capacity)
        ));
        assert!(matches!(
            validate_journal(
                &content.data,
                &identity,
                Limits {
                    bytes: limits.bytes,
                    records: 3
                }
            ),
            Err(ExecutionContentError::Capacity)
        ));
        let mut forged = receipt.clone();
        forged.limits.settlement_bytes += 1;
        assert!(matches!(
            content.settle_activation_output(&forged, reserved_text(&forged, "bad")),
            Err(ExecutionContentError::Conflict)
        ));
        let mut wrong_activation = reserved_text(&receipt, "bad");
        wrong_activation.activation.generation += 1;
        assert!(matches!(
            content.settle_activation_output(&receipt, wrong_activation),
            Err(ExecutionContentError::OwnerMismatch)
        ));
        content
            .settle_activation_output(&receipt, reserved_text(&receipt, "Stopped"))
            .unwrap();
        // Terminal partial settlement releases the unneeded streamed slots.
        content.append(request("after-terminal", 1)).unwrap();
        assert!(matches!(
            content.record_activation_partial(&receipt, 0, reserved_text(&receipt, "late")),
            Err(ExecutionContentError::Conflict)
        ));
    }

    #[test]
    fn lost_reservation_or_settlement_ack_requires_reopen_and_keeps_original_evidence() {
        let root = tempfile::tempdir().unwrap();
        let canonical = canonical(&root);
        let identity = canonical.identity().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let body = output_reservation();
        let record = Body::ActivationOutputReservation(body.clone());
        assert!(matches!(
            content.append_with(record.clone(), |storage, bytes| {
                storage.write(bytes)?;
                Err(io::Error::other("lost reservation acknowledgement"))
            }),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        assert!(matches!(
            content.append(record.clone()),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        drop(content);
        let mut content = store(dir.path(), identity.clone(), Limits::default());
        let reference = content.append(record).unwrap();
        let receipt = content.output_reservation_receipt(reference, &body);
        let raw = reserved_text(&receipt, "known terminal partial");
        let settlement = ReservedActivationOutputContent {
            reservation_ref: receipt.reference.clone(),
            slot: ActivationOutputSlot::Settlement,
            original_byte_len: raw.text.len() as u64,
            original_sha256: sha256(raw.text.as_bytes()),
            output: raw,
        };
        assert!(matches!(
            content.append_with(
                Body::ReservedOutput(settlement.clone()),
                |storage, bytes| {
                    storage.write(bytes)?;
                    Err(io::Error::other("lost settlement acknowledgement"))
                }
            ),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        assert!(matches!(
            content.activation_output_settlement(&receipt),
            Err(ExecutionContentError::RecoveryRequired)
        ));
        drop(content);
        let content = store(dir.path(), identity, Limits::default());
        let settled = content
            .activation_output_settlement(&receipt)
            .unwrap()
            .unwrap();
        assert_eq!(settled.content(), &settlement);
        assert_eq!(content.data.records.len(), 2);
    }
    #[test]
    fn selected_way_stays_in_session_history_independently_of_review_archive() {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let (snapshot, run, _) = condition_fixture(&mut canonical, &mut content);
        let activation = run.activations[0].clone();
        let receipt = content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: "exact selected continuation fixture".into(),
            })
            .unwrap();
        let selection = crate::ways_decision::WaysSelectedSessionTurn {
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            transcript_receipt_ref: receipt.reference().clone(),
        };
        content
            .retain_ways_selection(&snapshot, selection.clone(), activation.clone())
            .unwrap();
        content
            .retain_ways_selection(&snapshot, selection.clone(), activation.clone())
            .unwrap();
        let mut wrong = activation.clone();
        wrong.generation += 1;
        assert!(content
            .retain_ways_selection(&snapshot, selection.clone(), wrong)
            .is_err());
        let other = content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: "changed selected answer".into(),
            })
            .unwrap();
        let mut changed = selection.clone();
        changed.transcript_receipt_ref = other.reference().clone();
        assert!(content
            .retain_ways_selection(&snapshot, changed, activation.clone())
            .is_err());
        drop(content);
        let content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            content.ways_selections(&canonical).unwrap(),
            vec![selection]
        );
        assert_eq!(
            content.selected_way_activations(&canonical).unwrap(),
            vec![activation.clone()]
        );
        assert_eq!(
            content.project(&snapshot).unwrap().kept_way,
            Some(activation)
        );
        assert!(content
            .resolve_activation_evidence(receipt.reference())
            .is_ok());
    }

    #[test]
    fn failed_activations_are_classified_from_the_host_line_only() {
        let class =
            |text: &str| classify_activation_failure(text).map(|view| (view.class, view.next_step));
        assert_eq!(
            class("Activation failed: LLM provider stream ended early: EOF"),
            Some(("provider_incomplete", "continue"))
        );
        assert_eq!(
            class("Activation failed: Token budget exceeded: used 310594, budget 300000"),
            Some(("budget_limited", "finish_partial"))
        );
        assert_eq!(
            class("Activation failed: This Agent reached its token limit for this activation (600,000 tokens; 619,018 needed). Raise the Agent's token budget or narrow the task."),
            Some(("budget_limited", "finish_partial"))
        );
        assert_eq!(
            class("Activation failed: The Session budget for this Agent is used up: 1,000 of its 1,457,714 tokens remain and the next model call needs 36,864."),
            Some(("budget_limited", "finish_partial"))
        );
        assert_eq!(
            class("Activation failed: The Session budget for this Agent is used up: its last 4 invocation(s) are held for the host to observe its changes and run required checks."),
            Some(("budget_limited", "finish_partial"))
        );
        assert_eq!(
            class("Activation failed: it changed lib/paths.js outside the paths this Agent may change (none; this Agent is read-only); the change is kept for review.\n\nmodel text"),
            Some(("scope_violation", "review_then_finish"))
        );
        assert_eq!(
            class("Activation failed: its admitted write scope cannot be read, so its changes cannot be judged; any change is kept for review.\n\nmodel text"),
            Some(("capture_unavailable", "review_then_finish"))
        );
        // The model cannot choose the class: only the host's first line counts.
        assert_eq!(
            class("Activation failed: LLM provider error: 500\nToken budget exceeded"),
            Some(("provider_error", "continue"))
        );
        assert_eq!(
            class("Activation failed: LLM provider error: Streaming error: native Ollama: EOF before native terminal"),
            Some(("provider_incomplete", "continue"))
        );
        assert_eq!(
            class("I finished. Activation failed: Token budget exceeded"),
            None
        );
        assert_eq!(class("Stopped"), None);
        // A reserve refusal of the next model call is a budget limit, not a
        // provider fault that Continue would repeat.
        assert_eq!(
            class("Activation failed: LLM provider error: Invalid request for ollama: provider admission failed: The invocation allowance is nearly spent: 4 remaining invocation(s) are held"),
            Some(("budget_limited", "finish_partial"))
        );
        assert_eq!(
            class("Activation failed: LLM provider error: Invalid request for ollama: provider admission failed: grant expired"),
            Some(("admission", "inspect"))
        );
        // Text a tool returned later in the line cannot pick the class.
        assert_eq!(
            class("Activation failed: Tool call failed: bash - last failure: stream ended early"),
            Some(("other", "inspect"))
        );
    }
}
