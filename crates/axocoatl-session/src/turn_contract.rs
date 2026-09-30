//! Versioned logical-turn contracts, independent of the live v1 turn ledger.
//!
//! This fold validates durable event relationships; it neither persists events
//! nor authorizes tools or controllers. Callers must establish ownership,
//! permission, and durability before using its result to execute work. In
//! particular, accepting this schema does not make an old data root v2-safe.
//!
//! The immutable graph binds node identities, dependencies, savepoints and required
//! completion conditions. `Continue` atomically covers every declared graph node
//! and allocates immutable inputs and generations with the epoch. Accepted-input
//! revision explicitly invalidates affected descendants; it does not dispatch
//! work. External evidence resolution and grants require the host. Recorded tool
//! failure is not replay permission. Human authorization and provider-enforced
//! idempotency/reconciliation policies remain outside this fold.
//! A controller must resolve referenced evidence, revalidate repository and
//! environment freshness, restore the selected savepoint on a quiescent actor,
//! and acknowledge the durable event before physical execution. This contract
//! validates the initial DAG and additive/replacement graph revision contracts.
//! Dynamic graph and typed blocker execution still require the host proof join. Condition evidence is host-verified, never
//! authenticated merely by placing a reference in an event.
//!
//! Explicit Cancelled/Finished closure may retain unknown invocation/check evidence.
//! This immutable projection then represents knowledge at closure, not the latest
//! external-effect truth. Independently retained effect evidence must be joined
//! with this projection before live v2 writing; late outcomes must remain recordable
//! without reopening the logical turn or rewriting its accepted outputs.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use serde::{Deserialize, Serialize};

pub const TURN_CONTRACT_SCHEMA_VERSION: u32 = 2;
pub const MAX_CONTRACT_ENVELOPE_BYTES: usize = 256 * 1024;
pub const MAX_INPUT_REFERENCES: usize = 128;
pub const MAX_CONTRACT_NODES: usize = 128;
pub const MAX_GRAPH_EDGES: usize = 512;
pub const MAX_COMPLETION_CONDITIONS: usize = 64;
pub const MAX_CONTRACT_ACTIVATIONS: usize = 1024;
pub const MAX_CONTRACT_CONDITION_RUNS: usize = 1024;
pub const MAX_CONTRACT_COMMANDS: usize = 4096;
pub const MAX_RETAINED_CONTRACT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TurnContractError {
    #[error("invalid contract JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported logical-turn contract schema version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid contract identity")]
    InvalidIdentity,
    #[error("invalid logical-turn transition: {0}")]
    InvalidTransition(&'static str),
    #[error("command id was already used for different content")]
    CommandConflict,
    #[error("stale turn revision: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("turn revision or activation generation overflow")]
    Overflow,
    #[error("logical-turn contract limit exceeded: {0}")]
    LimitExceeded(&'static str),
}

macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, TurnContractError> {
                Self::try_from(value.into())
            }

            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl TryFrom<String> for $name {
            type Error = TurnContractError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                if value.is_empty() || value.len() > 128 || !value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
                }) {
                    return Err(TurnContractError::InvalidIdentity);
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self { value.0 }
        }
    )+};
}

identity!(
    SessionId,
    LogicalTurnId,
    ExecutionEpochId,
    TurnNodeId,
    ActivationId,
    NodeConversationId,
    CommandId,
    InvocationId,
    EvidenceRef,
    InputManifestId,
    CheckpointId,
    AgentDefinitionId,
    GrantId,
    SessionTeamSlotId,
    GraphSnapshotId,
    ConditionId,
    ConditionRunId,
    BlockerTypeId,
    BlockerId,
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationRef {
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub execution_epoch_id: ExecutionEpochId,
    pub node_id: TurnNodeId,
    pub generation: u32,
    pub activation_id: ActivationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionSnapshotRef {
    pub definition_id: AgentDefinitionId,
    pub snapshot: EvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSnapshotRef {
    pub grant_id: GrantId,
    pub revision: u64,
    pub evidence: EvidenceRef,
}

/// Committed references must be resolved by the owning checkpoint store before
/// dispatch. An accepted candidate is usable only through its exact producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointSource {
    Committed { evidence: EvidenceRef },
    Accepted { activation: ActivationRef },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRef {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub conversation_id: NodeConversationId,
    pub source: CheckpointSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConversationSavepoint {
    Empty,
    Checkpoint { checkpoint: Box<CheckpointRef> },
}

/// Authority metadata uses exact retained identities. A task here is the retained
/// task-acceptance evidence inside one logical turn, not ambient or cross-turn scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationScope {
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub task: EvidenceRef,
    pub approved_graph: EvidenceRef,
}

/// Exact allowlist vocabulary. There is deliberately no privilege expansion,
/// ForcePartial Finish, Keep, global definition/config edit, or human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedOperation {
    Inspect,
    AttachEvidence,
    SteerActivation,
    ReviseActivation,
    StopActivation,
    RetryActivation,
    ResumeMachineBlocker,
    ContinueTurn,
    AddAgent,
    ReplaceFutureAgent,
    FinishNormally,
}

/// Each operation carries its exact target scope. The controller must cover
/// every affected node, including rewired/invalidation/continuation targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedOperationPermission {
    pub operation: DelegatedOperation,
    pub targets: DelegatedTargetScope,
}

/// Fixed membership and delegated future subtree growth are distinct authority.
/// Subtree membership requires a canonical graph proof at the current revision;
/// a caller-supplied parent/NodeId cannot establish it. Growth remains bounded by
/// the same templates, profiles, graph limits and cumulative activation budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegatedTargetScope {
    Nodes {
        nodes: Vec<TurnNodeId>,
    },
    Subtree {
        root: TurnNodeId,
        include_future_descendants: bool,
    },
}

/// A definition reference is not proof that a blocker is machine-resolvable.
/// The host must resolve a retained, trusted definition, verify its classification
/// excludes human approval, and check this exact response schema before execution.
/// No machine blocker types or response schemas are granted by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineBlockerPermission {
    pub blocker_type: BlockerTypeId,
    pub definition: EvidenceRef,
    pub response_schema: EvidenceRef,
}

/// Initial graph identity is immutable for this logical turn. Slots and
/// conversations are distinct even when nodes reuse the same agent definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    pub node_id: TurnNodeId,
    pub slot_id: SessionTeamSlotId,
    pub definition: DefinitionSnapshotRef,
    pub conversation_id: NodeConversationId,
    pub starting_savepoint: ConversationSavepoint,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyEdge {
    pub parent: TurnNodeId,
    pub child: TurnNodeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConditionKind {
    RepositoryCheck { definition: EvidenceRef },
    Review { criterion: EvidenceRef },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionCondition {
    pub condition_id: ConditionId,
    pub kind: ConditionKind,
    pub nodes: Vec<TurnNodeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnGraphSnapshot {
    pub snapshot_id: GraphSnapshotId,
    pub revision: u64,
    pub nodes: Vec<GraphNode>,
    pub dependencies: Vec<DependencyEdge>,
    pub conditions: Vec<CompletionCondition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOutcome {
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConditionObservation {
    pub epoch_id: ExecutionEpochId,
    pub condition_id: ConditionId,
    pub activations: Vec<ActivationRef>,
    pub outcome: ConditionOutcome,
    pub evidence: EvidenceRef,
}

/// One check execution against exact accepted generations. Its epoch belongs to
/// the check; retained accepted inputs may have been produced in older epochs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionRunRef {
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub epoch_id: ExecutionEpochId,
    pub condition_id: ConditionId,
    pub run_id: ConditionRunId,
    pub activations: Vec<ActivationRef>,
}

/// Host-verified execution evidence, not a readiness verdict or replay grant.
/// A returned failure may have changed files or external state too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConditionEffectResolution {
    OutcomeRecorded { evidence: EvidenceRef },
    NotDispatched { evidence: EvidenceRef },
}

impl ConditionEffectResolution {
    pub fn disposition(&self) -> EffectDisposition {
        match self {
            Self::OutcomeRecorded { .. } => EffectDisposition::OutcomeRecorded,
            Self::NotDispatched { .. } => EffectDisposition::NotDispatched,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractConditionRun {
    pub run: ConditionRunRef,
    /// Exact retained executable intent; display previews are not executable data.
    pub intent: EvidenceRef,
    pub resolution: Option<ConditionEffectResolution>,
}

impl ContractConditionRun {
    pub fn disposition(&self) -> EffectDisposition {
        self.resolution.as_ref().map_or(
            EffectDisposition::OutcomeUnknown,
            ConditionEffectResolution::disposition,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionContext {
    pub activation: ActivationRef,
    pub output: EvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedParentInput {
    pub activation: ActivationRef,
    pub checkpoint: CheckpointRef,
    pub output: EvidenceRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryInput {
    Recorded { snapshot: EvidenceRef },
    Unavailable,
}

/// Immutable semantic inputs. References identify retained evidence; they do not
/// authenticate remote content or grant authority merely by being serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationInputManifest {
    pub manifest_id: InputManifestId,
    pub activation: ActivationRef,
    pub definition: DefinitionSnapshotRef,
    pub conversation_id: NodeConversationId,
    pub starting_savepoint: ConversationSavepoint,
    pub parents: Vec<AcceptedParentInput>,
    pub guidance: Vec<EvidenceRef>,
    pub attachments: Vec<EvidenceRef>,
    pub repository: RepositoryInput,
    pub budget: EvidenceRef,
    pub grant: Option<GrantSnapshotRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_context: Option<RevisionContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinuationSelection {
    RetainAccepted {
        activation: ActivationRef,
    },
    Retry {
        previous: ActivationRef,
        input: Box<ActivationInputManifest>,
    },
    LeaveBlocked {
        activation: ActivationRef,
        blocker: EvidenceRef,
    },
    Rebase {
        previous: ActivationRef,
        input: Box<ActivationInputManifest>,
    },
    Revise {
        previous: ActivationRef,
        input: Box<ActivationInputManifest>,
        invalidated_descendants: Vec<ActivationRef>,
        evidence: EvidenceRef,
    },
    PrepareUnmaterialized {
        input: Box<ActivationInputManifest>,
    },
    AwaitDependencies {
        node_id: TurnNodeId,
    },
    LeaveUnmaterializedBlocked {
        node_id: TurnNodeId,
        blocker: EvidenceRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationPlan {
    pub source_epoch_id: ExecutionEpochId,
    pub epoch_id: ExecutionEpochId,
    pub selections: Vec<ContinuationSelection>,
    /// Explicit admission for pending or failed check/review work. Retained
    /// accepted node generations need not be replayed merely to run a check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub condition_runs: Vec<ConditionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalTurnState {
    Running,
    NeedsAttention,
    Completed,
    Cancelled,
    Finished,
}

impl LogicalTurnState {
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Finished)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpochState {
    Running,
    Paused,
    Completed,
    Interrupted,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationState {
    Unstarted,
    Running,
    Accepted,
    Failed,
    Interrupted,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnClosure {
    Completed,
    Cancelled,
    Finished,
}

/// A successor points to closed history instead of reopening its execution.
/// The owning store must resolve this reference against canonical history;
/// serialized caller claims alone do not prove that the predecessor closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosedTurnRef {
    session_id: SessionId,
    turn_id: LogicalTurnId,
    closure_revision: u64,
    closure: TurnClosure,
}

impl ClosedTurnRef {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    pub fn turn_id(&self) -> &LogicalTurnId {
        &self.turn_id
    }
    pub fn closure_revision(&self) -> u64 {
        self.closure_revision
    }
    pub fn closure(&self) -> TurnClosure {
        self.closure
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOutcome {
    Succeeded,
    Failed,
}

/// Failure is retained evidence, not a claim that an external effect was undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDisposition {
    DispatchNotAuthorized,
    NotDispatched,
    OutcomeUnknown,
    OutcomeRecorded,
}

/// Missing observations from legacy/external execution must not manufacture safety.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationEvidence {
    NoIntent {
        instrumented: bool,
    },
    Intent,
    CancelledBeforeDispatch {
        evidence: EvidenceRef,
    },
    Outcome {
        outcome: InvocationOutcome,
        evidence: EvidenceRef,
    },
}

impl InvocationEvidence {
    pub fn disposition(&self) -> EffectDisposition {
        match self {
            Self::NoIntent { instrumented: true } => EffectDisposition::DispatchNotAuthorized,
            Self::NoIntent {
                instrumented: false,
            }
            | Self::Intent => EffectDisposition::OutcomeUnknown,
            Self::CancelledBeforeDispatch { .. } => EffectDisposition::NotDispatched,
            Self::Outcome { .. } => EffectDisposition::OutcomeRecorded,
        }
    }
}

#[path = "turn_contract_dynamic.rs"]
mod dynamic;
pub use dynamic::{
    ContractBlocker, GraphMutation, GraphRevisionRecord, ReplacedTurnNode, TurnBlockerKind,
    TurnBlockerResponse, TurnBlockerState, TypedTurnBlocker,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnContractEvent {
    /// An authenticated host records the human's exact whole-turn closing intent.
    /// It fences new work; terminal/effect evidence remains admissible.
    RequestTurnStop {
        evidence: EvidenceRef,
    },
    /// Authenticated human partial finalization. Selected accepted sinks and
    /// omitted conditions are immutable before cancellation signals are sent.
    RequestPartialFinish {
        approval: EvidenceRef,
        selected_activations: Vec<ActivationRef>,
        stop_activations: Vec<ActivationRef>,
        missing_conditions: Vec<EvidenceRef>,
        missing_condition_ids: Vec<ConditionId>,
    },
    /// Durable handoff at an actor safe boundary. The corresponding command's
    /// Settled receipt, not this event alone, records the actor's input append.
    ApplyGuidance {
        activation: ActivationRef,
        control_command_id: CommandId,
        instruction: EvidenceRef,
        request: EvidenceRef,
    },
    ReviseGraph {
        epoch_id: ExecutionEpochId,
        previous_graph: GraphSnapshotId,
        graph: TurnGraphSnapshot,
        mutation: GraphMutation,
        admission_evidence: EvidenceRef,
    },
    OpenBlocker {
        blocker: TypedTurnBlocker,
    },
    /// Exact live wait owner timed out or disappeared before human delivery.
    /// This records no approval, denial by a person, or effect settlement.
    AbandonBlocker {
        blocker_id: BlockerId,
        activation: ActivationRef,
        evidence: EvidenceRef,
    },
    ResolveBlocker {
        blocker_id: BlockerId,
        activation: ActivationRef,
        response: TurnBlockerResponse,
    },
    Begin {
        epoch_id: ExecutionEpochId,
        graph: TurnGraphSnapshot,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predecessor: Option<ClosedTurnRef>,
    },
    StartActivation {
        input: Box<ActivationInputManifest>,
    },
    ReviseAccepted {
        previous: ActivationRef,
        input: Box<ActivationInputManifest>,
        invalidated_descendants: Vec<ActivationRef>,
        evidence: EvidenceRef,
    },
    RebaseActivation {
        previous: ActivationRef,
        input: Box<ActivationInputManifest>,
    },
    RecordCondition {
        epoch_id: ExecutionEpochId,
        condition_id: ConditionId,
        activations: Vec<ActivationRef>,
        outcome: ConditionOutcome,
        evidence: EvidenceRef,
    },
    RecordConditionIntent {
        run: ConditionRunRef,
        intent: EvidenceRef,
    },
    ResolveConditionIntent {
        run_id: ConditionRunId,
        resolution: ConditionEffectResolution,
    },
    StartPreparedActivation {
        activation: ActivationRef,
    },
    AcceptActivation {
        activation: ActivationRef,
        checkpoint: Box<CheckpointRef>,
        output: EvidenceRef,
    },
    FailActivation {
        activation: ActivationRef,
        evidence: EvidenceRef,
    },
    RecordIntent {
        invocation_id: InvocationId,
        activation: ActivationRef,
    },
    RecordOutcome {
        invocation_id: InvocationId,
        outcome: InvocationOutcome,
        evidence: EvidenceRef,
    },
    ProveNotDispatched {
        invocation_id: InvocationId,
        evidence: EvidenceRef,
    },
    InterruptEpoch {
        epoch_id: ExecutionEpochId,
    },
    PauseEpoch {
        epoch_id: ExecutionEpochId,
    },
    Continue {
        plan: ContinuationPlan,
    },
    Close {
        closure: TurnClosure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnContractEnvelope {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub expected_revision: u64,
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub event: TurnContractEvent,
}

impl TurnContractEnvelope {
    /// Inspect the version before interpreting a future event's payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, TurnContractError> {
        if bytes.len() > MAX_CONTRACT_ENVELOPE_BYTES {
            return Err(TurnContractError::LimitExceeded("envelope bytes"));
        }
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }
        let header: Header = serde_json::from_slice(bytes)?;
        check_version(header.schema_version)?;
        let envelope: Self = serde_json::from_slice(bytes)?;
        envelope.validate_limits()?;
        Ok(envelope)
    }

    fn new_inputs(&self) -> Vec<&ActivationInputManifest> {
        match &self.event {
            TurnContractEvent::StartActivation { input }
            | TurnContractEvent::ReviseAccepted { input, .. }
            | TurnContractEvent::RebaseActivation { input, .. } => vec![input.as_ref()],
            TurnContractEvent::Continue { plan } => plan
                .selections
                .iter()
                .filter_map(|selection| match selection {
                    ContinuationSelection::Retry { input, .. }
                    | ContinuationSelection::Rebase { input, .. }
                    | ContinuationSelection::Revise { input, .. }
                    | ContinuationSelection::PrepareUnmaterialized { input } => {
                        Some(input.as_ref())
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn validate_limits(&self) -> Result<usize, TurnContractError> {
        if matches!(&self.event, TurnContractEvent::Continue { plan } if plan.selections.len() > MAX_CONTRACT_NODES || plan.condition_runs.len() > MAX_COMPLETION_CONDITIONS)
        {
            return Err(TurnContractError::LimitExceeded("continuation selections"));
        }
        if matches!(&self.event, TurnContractEvent::Continue { plan } if plan.selections.iter().any(|selection|
            matches!(selection, ContinuationSelection::Revise { invalidated_descendants, .. }
                if invalidated_descendants.len() > MAX_CONTRACT_NODES)))
        {
            return Err(TurnContractError::LimitExceeded("invalidation references"));
        }
        match &self.event {
            TurnContractEvent::Begin { graph, .. }
            | TurnContractEvent::ReviseGraph { graph, .. } => graph.validate_limits()?,
            TurnContractEvent::ReviseAccepted {
                invalidated_descendants,
                ..
            } if invalidated_descendants.len() > MAX_CONTRACT_NODES => {
                return Err(TurnContractError::LimitExceeded("invalidation references"))
            }
            TurnContractEvent::RecordCondition { activations, .. }
                if activations.len() > MAX_CONTRACT_NODES =>
            {
                return Err(TurnContractError::LimitExceeded("condition references"))
            }
            TurnContractEvent::RecordConditionIntent { run, .. }
                if run.activations.len() > MAX_CONTRACT_NODES =>
            {
                return Err(TurnContractError::LimitExceeded("condition references"))
            }
            _ => {}
        }
        for input in self.new_inputs() {
            if input
                .parents
                .len()
                .saturating_add(input.guidance.len())
                .saturating_add(input.attachments.len())
                > MAX_INPUT_REFERENCES
            {
                return Err(TurnContractError::LimitExceeded("input references"));
            }
        }
        // Count serialization without allocating another copy of an oversized
        // caller-built envelope. Decode has its own byte gate before JSON parse.
        struct Counter(usize);
        impl Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > MAX_CONTRACT_ENVELOPE_BYTES {
                    return Err(std::io::Error::other("contract envelope too large"));
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut counter = Counter(0);
        if serde_json::to_writer(&mut counter, self).is_err() {
            return Err(TurnContractError::LimitExceeded("envelope bytes"));
        }
        Ok(counter.0)
    }
}

impl TurnGraphSnapshot {
    fn validate_limits(&self) -> Result<(), TurnContractError> {
        if self.nodes.len() > MAX_CONTRACT_NODES
            || self.dependencies.len() > MAX_GRAPH_EDGES
            || self.conditions.len() > MAX_COMPLETION_CONDITIONS
            || self
                .conditions
                .iter()
                .any(|item| item.nodes.len() > MAX_CONTRACT_NODES)
        {
            return Err(TurnContractError::LimitExceeded("graph declarations"));
        }
        Ok(())
    }

    pub fn validate(&self, session_id: &SessionId) -> Result<(), TurnContractError> {
        self.validate_limits()?;
        if self.revision != 1
            || self.nodes.is_empty()
            || (!self.nodes.iter().any(|node| node.required) && self.conditions.is_empty())
        {
            return Err(TurnContractError::InvalidTransition(
                "initial graph requires revision one and completion requirements",
            ));
        }
        let mut nodes = HashSet::new();
        let mut slots = HashSet::new();
        let mut conversations = HashSet::new();
        let mut checkpoints = HashMap::new();
        for node in &self.nodes {
            if !nodes.insert(&node.node_id)
                || !slots.insert(&node.slot_id)
                || !conversations.insert(&node.conversation_id)
            {
                return Err(TurnContractError::InvalidTransition(
                    "graph node, slot or conversation is duplicated",
                ));
            }
            if let ConversationSavepoint::Checkpoint { checkpoint } = &node.starting_savepoint {
                if checkpoint.session_id != *session_id
                    || checkpoint.conversation_id != node.conversation_id
                    || !matches!(checkpoint.source, CheckpointSource::Committed { .. })
                {
                    return Err(TurnContractError::InvalidTransition(
                        "initial graph savepoint must be committed to its Session and conversation",
                    ));
                }
                if checkpoints
                    .insert(&checkpoint.checkpoint_id, checkpoint)
                    .is_some()
                {
                    return Err(TurnContractError::InvalidTransition(
                        "graph checkpoint identity is duplicated",
                    ));
                }
            }
        }
        let mut edges = HashSet::new();
        for edge in &self.dependencies {
            if edge.parent == edge.child
                || !nodes.contains(&edge.parent)
                || !nodes.contains(&edge.child)
                || !edges.insert((&edge.parent, &edge.child))
            {
                return Err(TurnContractError::InvalidTransition(
                    "graph dependency is duplicated, dangling or self-referential",
                ));
            }
        }
        let mut visited = HashSet::new();
        loop {
            let previous = visited.len();
            for node in &self.nodes {
                if self
                    .dependencies
                    .iter()
                    .filter(|edge| edge.child == node.node_id)
                    .all(|edge| visited.contains(&edge.parent))
                {
                    visited.insert(&node.node_id);
                }
            }
            if visited.len() == self.nodes.len() {
                break;
            }
            if previous == visited.len() {
                return Err(TurnContractError::InvalidTransition(
                    "graph dependencies contain a cycle",
                ));
            }
        }
        let mut conditions = HashSet::new();
        for condition in &self.conditions {
            if !conditions.insert(&condition.condition_id)
                || condition.nodes.is_empty()
                || condition.nodes.iter().collect::<HashSet<_>>().len() != condition.nodes.len()
                || condition.nodes.iter().any(|node| !nodes.contains(node))
            {
                return Err(TurnContractError::InvalidTransition(
                    "completion condition requires unique declared node scope",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Admission {
    Retry,
    Rebase,
    Revision,
}

fn check_version(version: u32) -> Result<(), TurnContractError> {
    if version != TURN_CONTRACT_SCHEMA_VERSION {
        return Err(TurnContractError::UnsupportedVersion(version));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionEpoch {
    pub id: ExecutionEpochId,
    pub state: EpochState,
    pub continuation: Option<ContinuationPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractActivation {
    pub activation: ActivationRef,
    pub conversation_id: NodeConversationId,
    pub input: ActivationInputManifest,
    pub state: ActivationState,
    pub checkpoint: Option<CheckpointRef>,
    pub output: Option<EvidenceRef>,
    pub failure: Option<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractInvocation {
    pub invocation_id: InvocationId,
    pub activation: ActivationRef,
    pub evidence: InvocationEvidence,
}

/// An append-only amendment to the exact activation's immutable initial input.
/// Source attribution resolves through the retained control request. Handoff is
/// not proof that a provider has consumed this instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivationGuidance {
    pub activation: ActivationRef,
    pub control_command_id: CommandId,
    pub instruction: EvidenceRef,
    pub request: EvidenceRef,
}

/// Immutable closing intent. Never-started declared work is retained explicitly;
/// the initial graph and all prior generations remain reviewable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnStopIntent {
    pub command_id: CommandId,
    pub requested_revision: u64,
    pub evidence: EvidenceRef,
    pub unrun_nodes: Vec<TurnNodeId>,
    // Ordinary Stop predates this field. Preserve its canonical digest so
    // existing promotion manifests remain valid after upgrading the host.
    #[serde(skip_serializing_if = "is_cancelled_closure")]
    pub closure: TurnClosure,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_finish: Option<PartialFinishSelection>,
}

fn is_cancelled_closure(closure: &TurnClosure) -> bool {
    *closure == TurnClosure::Cancelled
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartialFinishSelection {
    pub selected_activations: Vec<ActivationRef>,
    pub stop_activations: Vec<ActivationRef>,
    pub missing_conditions: Vec<EvidenceRef>,
    pub missing_condition_ids: Vec<ConditionId>,
}

/// Rebuild from validated envelopes; deserializing a snapshot cannot bypass replay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TurnContract {
    session_id: Option<SessionId>,
    turn_id: Option<LogicalTurnId>,
    revision: u64,
    state: Option<LogicalTurnState>,
    predecessor: Option<ClosedTurnRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_requested: Option<TurnStopIntent>,
    graph: Option<TurnGraphSnapshot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    graph_history: Vec<GraphRevisionRecord>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    replaced_nodes: Vec<ReplacedTurnNode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blockers: Vec<ContractBlocker>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    guidance: Vec<ActivationGuidance>,
    conditions: Vec<ConditionObservation>,
    condition_runs: Vec<ContractConditionRun>,
    epochs: Vec<ExecutionEpoch>,
    activations: Vec<ContractActivation>,
    invocations: Vec<ContractInvocation>,
    #[serde(skip)]
    commands: HashMap<CommandId, TurnContractEnvelope>,
    #[serde(skip)]
    retained_event_bytes: usize,
}

impl TurnContract {
    pub fn stop_requested(&self) -> Option<&TurnStopIntent> {
        self.stop_requested.as_ref()
    }
    /// Closing selection never mutates raw canonical acceptance.
    pub fn selected_for_finalization(&self, activation: &ActivationRef) -> bool {
        self.stop_requested
            .as_ref()
            .and_then(|intent| intent.partial_finish.as_ref())
            .is_none_or(|selection| selection.selected_activations.contains(activation))
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn state(&self) -> Option<LogicalTurnState> {
        self.state
    }
    pub fn epochs(&self) -> &[ExecutionEpoch] {
        &self.epochs
    }
    pub fn activations(&self) -> &[ContractActivation] {
        &self.activations
    }
    pub fn guidance(&self) -> &[ActivationGuidance] {
        &self.guidance
    }
    pub fn invocations(&self) -> &[ContractInvocation] {
        &self.invocations
    }

    pub fn graph(&self) -> Option<&TurnGraphSnapshot> {
        self.graph.as_ref()
    }
    pub fn conditions(&self) -> &[ConditionObservation] {
        &self.conditions
    }
    pub fn condition_runs(&self) -> &[ContractConditionRun] {
        &self.condition_runs
    }
    pub fn condition_run(&self, run_id: &ConditionRunId) -> Option<&ContractConditionRun> {
        self.condition_runs
            .iter()
            .find(|item| item.run.run_id == *run_id)
    }
    pub fn command_count(&self) -> usize {
        self.commands.len()
    }
    pub fn retained_event_bytes(&self) -> usize {
        self.retained_event_bytes
    }

    /// Accepted history is retained, but only current, non-superseded generations
    /// are eligible for consumption or physical checkpoint promotion.
    pub fn current_accepted_activations(&self) -> Vec<&ContractActivation> {
        self.graph
            .iter()
            .flat_map(|graph| &graph.nodes)
            .filter_map(|node| self.latest_activation(&node.node_id))
            .filter(|item| self.is_current_accepted(&item.activation))
            .collect()
    }

    pub fn predecessor(&self) -> Option<&ClosedTurnRef> {
        self.predecessor.as_ref()
    }

    pub fn closed_reference(&self) -> Result<ClosedTurnRef, TurnContractError> {
        let closure = match self.state {
            Some(LogicalTurnState::Completed) => TurnClosure::Completed,
            Some(LogicalTurnState::Cancelled) => TurnClosure::Cancelled,
            Some(LogicalTurnState::Finished) => TurnClosure::Finished,
            _ => {
                return Err(TurnContractError::InvalidTransition(
                    "predecessor is not closed",
                ))
            }
        };
        Ok(ClosedTurnRef {
            session_id: self
                .session_id
                .clone()
                .ok_or(TurnContractError::InvalidTransition("turn has no Session"))?,
            turn_id: self
                .turn_id
                .clone()
                .ok_or(TurnContractError::InvalidTransition("turn has no identity"))?,
            closure_revision: self.revision,
            closure,
        })
    }

    /// Returns false for exact idempotent replay, even after closure. Rejection
    /// never mutates the projection. Persistence must acknowledge the envelope
    /// separately before a caller treats an applied transition as durable.
    pub fn apply(&mut self, envelope: &TurnContractEnvelope) -> Result<bool, TurnContractError> {
        check_version(envelope.schema_version)?;
        let envelope_bytes = envelope.validate_limits()?;
        if let Some(previous) = self.commands.get(&envelope.command_id) {
            return if previous == envelope {
                Ok(false)
            } else {
                Err(TurnContractError::CommandConflict)
            };
        }
        if envelope.expected_revision != self.revision {
            return Err(TurnContractError::StaleRevision {
                expected: envelope.expected_revision,
                actual: self.revision,
            });
        }
        if self.state.is_some_and(LogicalTurnState::is_closed) {
            return Err(TurnContractError::InvalidTransition(
                "closed turns are immutable",
            ));
        }
        if self.commands.len() >= MAX_CONTRACT_COMMANDS {
            return Err(TurnContractError::LimitExceeded("command count"));
        }
        let inputs = envelope.new_inputs();
        if self.activations.len().saturating_add(inputs.len()) > MAX_CONTRACT_ACTIVATIONS {
            return Err(TurnContractError::LimitExceeded("activation count"));
        }
        let nodes = self
            .activations
            .iter()
            .map(|item| &item.activation.node_id)
            .chain(inputs.iter().map(|input| &input.activation.node_id))
            .collect::<HashSet<_>>();
        if nodes.len() > MAX_CONTRACT_NODES {
            return Err(TurnContractError::LimitExceeded("node count"));
        }
        let retained_event_bytes = self
            .retained_event_bytes
            .checked_add(envelope_bytes)
            .filter(|total| *total <= MAX_RETAINED_CONTRACT_BYTES)
            .ok_or(TurnContractError::LimitExceeded("retained event bytes"))?;
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(TurnContractError::Overflow)?;
        let mut next = self.clone();
        next.apply_event(envelope)?;
        // Only new work or graph promises need capacity admission. An observed
        // failure/interruption must remain recordable even if it makes later
        // required recovery exceed the remaining bound.
        if !inputs.is_empty() || matches!(envelope.event, TurnContractEvent::ReviseGraph { .. }) {
            next.validate_dynamic_graph_capacity()?;
        }
        next.revision = next_revision;
        next.retained_event_bytes = retained_event_bytes;
        next.commands
            .insert(envelope.command_id.clone(), envelope.clone());
        *self = next;
        Ok(true)
    }

    fn apply_event(&mut self, envelope: &TurnContractEnvelope) -> Result<(), TurnContractError> {
        if let TurnContractEvent::Begin {
            epoch_id,
            graph,
            predecessor,
        } = &envelope.event
        {
            if self.state.is_some() {
                return Err(TurnContractError::InvalidTransition("turn already began"));
            }
            if predecessor.as_ref().is_some_and(|previous| {
                previous.session_id != envelope.session_id
                    || previous.turn_id == envelope.turn_id
                    || previous.closure_revision == 0
            }) {
                return Err(TurnContractError::InvalidTransition(
                    "invalid closed predecessor reference",
                ));
            }
            graph.validate(&envelope.session_id)?;
            self.graph = Some(graph.clone());
            self.session_id = Some(envelope.session_id.clone());
            self.turn_id = Some(envelope.turn_id.clone());
            self.state = Some(LogicalTurnState::Running);
            self.predecessor = predecessor.clone();
            self.epochs.push(ExecutionEpoch {
                id: epoch_id.clone(),
                state: EpochState::Running,
                continuation: None,
            });
            return Ok(());
        }
        if self.session_id.as_ref() != Some(&envelope.session_id)
            || self.turn_id.as_ref() != Some(&envelope.turn_id)
        {
            return Err(TurnContractError::InvalidTransition(
                "event does not belong to this turn",
            ));
        }
        // Preserve truthful late outcomes and epoch-loss evidence, but never
        // convert a closing request into new work or accepted late output.
        if self.stop_requested.is_some()
            && !matches!(
                envelope.event,
                TurnContractEvent::FailActivation { .. }
                    | TurnContractEvent::RecordOutcome { .. }
                    | TurnContractEvent::ProveNotDispatched { .. }
                    | TurnContractEvent::RecordCondition { .. }
                    | TurnContractEvent::ResolveConditionIntent { .. }
                    | TurnContractEvent::AbandonBlocker { .. }
                    | TurnContractEvent::InterruptEpoch { .. }
            )
            && !matches!(&envelope.event, TurnContractEvent::Close { closure }
                if Some(*closure) == self.stop_requested.as_ref().map(|intent| intent.closure))
        {
            return Err(TurnContractError::InvalidTransition(
                "whole-turn Stop has closed admission",
            ));
        }
        match &envelope.event {
            TurnContractEvent::RequestTurnStop { evidence }
            | TurnContractEvent::RequestPartialFinish {
                approval: evidence, ..
            } => {
                if self.state.is_none_or(LogicalTurnState::is_closed) {
                    return Err(TurnContractError::InvalidTransition(
                        "whole-turn Stop requires an unfinished turn",
                    ));
                }
                let partial_finish = if let TurnContractEvent::RequestPartialFinish {
                    selected_activations,
                    stop_activations,
                    missing_conditions,
                    missing_condition_ids,
                    ..
                } = &envelope.event
                {
                    let graph = self.graph.as_ref().unwrap();
                    let running = self
                        .activations
                        .iter()
                        .filter(|item| item.state == ActivationState::Running)
                        .map(|item| &item.activation)
                        .collect::<Vec<_>>();
                    let accepted = self.current_accepted_activations();
                    let missing = graph
                        .conditions
                        .iter()
                        .filter(|condition| !self.condition_satisfied(&condition.condition_id))
                        .collect::<Vec<_>>();
                    let mut missing_evidence = vec![];
                    for condition in &missing {
                        let reference = match &condition.kind {
                            ConditionKind::RepositoryCheck { definition } => definition,
                            ConditionKind::Review { criterion } => criterion,
                        };
                        if !missing_evidence.contains(&reference) {
                            missing_evidence.push(reference);
                        }
                    }
                    if missing_condition_ids.len() != missing.len()
                        || missing_condition_ids.iter().enumerate().any(|(index, id)| {
                            missing_condition_ids[..index].contains(id)
                                || !missing
                                    .iter()
                                    .any(|condition| condition.condition_id == *id)
                        })
                        || missing_conditions.len() != missing_evidence.len()
                        || missing_conditions
                            .iter()
                            .any(|reference| !missing_evidence.contains(&reference))
                        || stop_activations.len() != running.len()
                        || stop_activations.iter().enumerate().any(|(index, item)| {
                            !running.contains(&item) || stop_activations[..index].contains(item)
                        })
                        || selected_activations
                            .iter()
                            .enumerate()
                            .any(|(index, item)| {
                                selected_activations[..index].contains(item)
                                    || !accepted.iter().any(|accepted| accepted.activation == *item)
                                    || graph
                                        .dependencies
                                        .iter()
                                        .any(|edge| edge.parent == item.node_id)
                            })
                        || missing_conditions
                            .iter()
                            .enumerate()
                            .any(|(index, item)| missing_conditions[..index].contains(item))
                    {
                        return Err(TurnContractError::InvalidTransition("partial Finish must select exact accepted sinks and all running activations"));
                    }
                    Some(PartialFinishSelection {
                        selected_activations: selected_activations.clone(),
                        stop_activations: stop_activations.clone(),
                        missing_conditions: missing_conditions.clone(),
                        missing_condition_ids: missing_condition_ids.clone(),
                    })
                } else {
                    None
                };
                let unrun_nodes = self
                    .graph
                    .as_ref()
                    .unwrap()
                    .nodes
                    .iter()
                    .filter(|node| {
                        !self.commands.values().any(|record| match &record.event {
                            TurnContractEvent::StartActivation { input } => {
                                input.activation.node_id == node.node_id
                            }
                            TurnContractEvent::StartPreparedActivation { activation } => {
                                activation.node_id == node.node_id
                            }
                            _ => false,
                        })
                    })
                    .map(|node| node.node_id.clone())
                    .collect();
                self.stop_requested = Some(TurnStopIntent {
                    command_id: envelope.command_id.clone(),
                    requested_revision: envelope.expected_revision + 1,
                    evidence: evidence.clone(),
                    unrun_nodes,
                    closure: if partial_finish.is_some() {
                        TurnClosure::Finished
                    } else {
                        TurnClosure::Cancelled
                    },
                    partial_finish,
                });
            }
            TurnContractEvent::ApplyGuidance {
                activation,
                control_command_id,
                instruction,
                request,
            } => {
                self.require_live_activation(activation)?;
                self.require_previous(activation, &activation.node_id)?;
                self.require_unblocked(activation)?;
                let current = self
                    .activations
                    .iter()
                    .find(|item| item.activation == *activation)
                    .ok_or(TurnContractError::InvalidTransition(
                        "guidance activation is absent",
                    ))?;
                if current.state != ActivationState::Running
                    || self
                        .guidance
                        .iter()
                        .any(|item| item.control_command_id == *control_command_id)
                {
                    return Err(TurnContractError::InvalidTransition(
                        "guidance requires running work and a unique control command",
                    ));
                }
                let count = current.input.guidance.len()
                    + current.input.parents.len()
                    + current.input.attachments.len()
                    + self
                        .guidance
                        .iter()
                        .filter(|item| item.activation == *activation)
                        .count();
                if count >= MAX_INPUT_REFERENCES {
                    return Err(TurnContractError::LimitExceeded(
                        "activation guidance references",
                    ));
                }
                self.guidance.push(ActivationGuidance {
                    activation: activation.clone(),
                    control_command_id: control_command_id.clone(),
                    instruction: instruction.clone(),
                    request: request.clone(),
                });
            }
            TurnContractEvent::ReviseGraph {
                epoch_id,
                previous_graph,
                graph,
                mutation,
                admission_evidence,
            } => {
                self.revise_graph(
                    epoch_id,
                    previous_graph,
                    graph,
                    mutation,
                    admission_evidence,
                )?;
            }
            TurnContractEvent::OpenBlocker { blocker } => self.open_blocker(blocker)?,
            TurnContractEvent::AbandonBlocker {
                blocker_id,
                activation,
                evidence,
            } => {
                self.abandon_exact_blocker(blocker_id, activation, evidence)?;
            }
            TurnContractEvent::ResolveBlocker {
                blocker_id,
                activation,
                response,
            } => {
                self.resolve_blocker(blocker_id, activation, response)?;
            }
            TurnContractEvent::Begin { .. } => {
                return Err(TurnContractError::InvalidTransition("turn already began"));
            }
            TurnContractEvent::StartActivation { input } => {
                self.admit_activation(input, ActivationState::Running, Admission::Retry, false)?;
            }
            TurnContractEvent::ReviseAccepted {
                previous,
                input,
                invalidated_descendants,
                evidence,
            } => {
                self.revise_accepted(previous, input, invalidated_descendants)?;
                for activation in invalidated_descendants {
                    self.abandon_blockers(activation, evidence);
                }
            }
            TurnContractEvent::RebaseActivation { previous, input } => {
                self.require_previous(previous, &input.activation.node_id)?;
                self.admit_activation(input, ActivationState::Unstarted, Admission::Rebase, false)?;
            }
            TurnContractEvent::RecordCondition {
                epoch_id,
                condition_id,
                activations,
                outcome,
                evidence,
            } => {
                if !self
                    .epochs
                    .last()
                    .is_some_and(|epoch| epoch.id == *epoch_id)
                {
                    return Err(TurnContractError::InvalidTransition(
                        "condition observation belongs to a stale execution epoch",
                    ));
                }
                self.validate_condition_selection(condition_id, activations)?;
                if self
                    .condition_runs
                    .iter()
                    .any(|item| item.run.condition_id == *condition_id && item.resolution.is_none())
                {
                    return Err(TurnContractError::InvalidTransition(
                        "condition execution still has an unresolved durable intent",
                    ));
                }
                self.conditions.push(ConditionObservation {
                    epoch_id: epoch_id.clone(),
                    condition_id: condition_id.clone(),
                    activations: activations.clone(),
                    outcome: *outcome,
                    evidence: evidence.clone(),
                });
            }
            TurnContractEvent::RecordConditionIntent { run, intent } => {
                if self.session_id.as_ref() != Some(&run.session_id)
                    || self.turn_id.as_ref() != Some(&run.turn_id)
                {
                    return Err(TurnContractError::InvalidTransition(
                        "condition execution belongs to another turn",
                    ));
                }
                self.require_epoch(&run.epoch_id)?;
                self.validate_condition_selection(&run.condition_id, &run.activations)?;
                if self.condition_runs.len() >= MAX_CONTRACT_CONDITION_RUNS {
                    return Err(TurnContractError::LimitExceeded(
                        "condition execution count",
                    ));
                }
                if self.condition_run(&run.run_id).is_some() {
                    return Err(TurnContractError::InvalidTransition(
                        "condition execution identity already exists",
                    ));
                }
                if self.current_condition(&run.condition_id).is_some()
                    || self.condition_runs.iter().any(|item| {
                        item.run.condition_id == run.condition_id && item.resolution.is_none()
                    })
                {
                    return Err(TurnContractError::InvalidTransition(
                        "condition execution requires pending work without an unresolved intent",
                    ));
                }
                self.condition_runs.push(ContractConditionRun {
                    run: run.clone(),
                    intent: intent.clone(),
                    resolution: None,
                });
            }
            TurnContractEvent::ResolveConditionIntent { run_id, resolution } => {
                // An interrupted epoch can receive authoritative late evidence.
                // apply() still refuses all new writes after logical closure.
                // No observation or accepted output is synthesized here.
                let item = self
                    .condition_runs
                    .iter_mut()
                    .find(|item| item.run.run_id == *run_id && item.resolution.is_none())
                    .ok_or(TurnContractError::InvalidTransition(
                        "condition execution has no unresolved durable intent",
                    ))?;
                item.resolution = Some(resolution.clone());
            }
            TurnContractEvent::StartPreparedActivation { activation } => {
                self.require_live_activation(activation)?;
                self.graph_node(&activation.node_id)?;
                self.require_unblocked(activation)?;
                if self.invocations.iter().any(|item| {
                    item.activation.node_id == activation.node_id
                        && item.evidence.disposition() == EffectDisposition::OutcomeUnknown
                }) {
                    return Err(TurnContractError::InvalidTransition(
                        "prepared activation has unresolved effects",
                    ));
                }
                let item = self
                    .activations
                    .iter_mut()
                    .find(|item| {
                        item.activation == *activation && item.state == ActivationState::Unstarted
                    })
                    .ok_or(TurnContractError::InvalidTransition(
                        "exact activation was not prepared",
                    ))?;
                item.state = ActivationState::Running;
            }
            TurnContractEvent::AcceptActivation {
                activation,
                checkpoint,
                output,
            } => {
                self.require_live_activation(activation)?;
                self.require_unblocked(activation)?;
                self.validate_accepted_checkpoint(activation, checkpoint)?;
                if self.invocations.iter().any(|item| {
                    item.activation == *activation
                        && item.evidence.disposition() == EffectDisposition::OutcomeUnknown
                }) {
                    return Err(TurnContractError::InvalidTransition(
                        "acceptance requires settled invocation evidence",
                    ));
                }
                let item = self.running_activation_mut(activation)?;
                item.state = ActivationState::Accepted;
                item.checkpoint = Some(checkpoint.as_ref().clone());
                item.output = Some(output.clone());
            }
            TurnContractEvent::FailActivation {
                activation,
                evidence,
            } => {
                self.require_live_activation(activation)?;
                let item = self.running_activation_mut(activation)?;
                item.state = ActivationState::Failed;
                item.failure = Some(evidence.clone());
                self.abandon_blockers(activation, evidence);
            }
            TurnContractEvent::RecordIntent {
                invocation_id,
                activation,
            } => {
                self.require_live_activation(activation)?;
                self.require_unblocked(activation)?;
                self.running_activation_mut(activation)?;
                if self
                    .invocations
                    .iter()
                    .any(|item| item.invocation_id == *invocation_id)
                {
                    return Err(TurnContractError::InvalidTransition(
                        "invocation identity already exists",
                    ));
                }
                self.invocations.push(ContractInvocation {
                    invocation_id: invocation_id.clone(),
                    activation: activation.clone(),
                    evidence: InvocationEvidence::Intent,
                });
            }
            TurnContractEvent::RecordOutcome {
                invocation_id,
                outcome,
                evidence,
            } => {
                self.unresolved_invocation_mut(invocation_id)?.evidence =
                    InvocationEvidence::Outcome {
                        outcome: *outcome,
                        evidence: evidence.clone(),
                    };
            }
            TurnContractEvent::ProveNotDispatched {
                invocation_id,
                evidence,
            } => {
                self.unresolved_invocation_mut(invocation_id)?.evidence =
                    InvocationEvidence::CancelledBeforeDispatch {
                        evidence: evidence.clone(),
                    };
            }
            TurnContractEvent::InterruptEpoch { epoch_id } => {
                self.require_epoch(epoch_id)?;
                for item in &mut self.activations {
                    if item.state == ActivationState::Running {
                        item.state = ActivationState::Interrupted;
                    }
                }
                self.interrupt_blockers(epoch_id);
                self.current_epoch_mut()?.state = EpochState::Interrupted;
                self.state = Some(LogicalTurnState::NeedsAttention);
            }
            TurnContractEvent::PauseEpoch { epoch_id } => {
                self.require_epoch(epoch_id)?;
                self.require_settled_activations()?;
                self.interrupt_blockers(epoch_id);
                self.current_epoch_mut()?.state = EpochState::Paused;
                self.state = Some(LogicalTurnState::NeedsAttention);
            }
            TurnContractEvent::Continue { plan } => {
                self.apply_continuation(plan)?;
            }
            TurnContractEvent::Close { closure } => {
                self.require_settled_activations()?;
                if *closure == TurnClosure::Completed
                    && (!self.completion_satisfied() || self.has_unknown_effects())
                {
                    return Err(TurnContractError::InvalidTransition(
                        "completion requires required graph nodes, fresh passing conditions and settled effects",
                    ));
                }
                if self.current_epoch_mut()?.state == EpochState::Running {
                    self.current_epoch_mut()?.state = match closure {
                        TurnClosure::Cancelled => EpochState::Cancelled,
                        TurnClosure::Completed | TurnClosure::Finished => EpochState::Completed,
                    };
                }
                for blocker in &mut self.blockers {
                    if blocker.state == TurnBlockerState::Pending {
                        blocker.state = TurnBlockerState::Closed { closure: *closure };
                    }
                }
                self.state = Some(match closure {
                    TurnClosure::Completed => LogicalTurnState::Completed,
                    TurnClosure::Cancelled => LogicalTurnState::Cancelled,
                    TurnClosure::Finished => LogicalTurnState::Finished,
                });
            }
        }
        Ok(())
    }

    fn graph_node(&self, node_id: &TurnNodeId) -> Result<&GraphNode, TurnContractError> {
        self.graph
            .as_ref()
            .and_then(|graph| graph.nodes.iter().find(|node| node.node_id == *node_id))
            .ok_or(TurnContractError::InvalidTransition(
                "activation node is not declared in current graph",
            ))
    }

    fn is_current_accepted(&self, activation: &ActivationRef) -> bool {
        self.latest_activation(&activation.node_id)
            .is_some_and(|item| {
                item.activation == *activation
                    && item.state == ActivationState::Accepted
                    && item.input.parents.iter().all(|parent| {
                        self.latest_activation(&parent.activation.node_id)
                            .is_some_and(|current| {
                                current.activation == parent.activation
                                    && current.state == ActivationState::Accepted
                                    && current.checkpoint.as_ref() == Some(&parent.checkpoint)
                                    && current.output.as_ref() == Some(&parent.output)
                            })
                    })
            })
    }

    fn validate_condition_selection(
        &self,
        condition_id: &ConditionId,
        activations: &[ActivationRef],
    ) -> Result<(), TurnContractError> {
        let condition = self
            .graph
            .as_ref()
            .and_then(|graph| {
                graph
                    .conditions
                    .iter()
                    .find(|item| item.condition_id == *condition_id)
            })
            .ok_or(TurnContractError::InvalidTransition(
                "condition is not declared in current graph",
            ))?;
        let selected = activations
            .iter()
            .map(|item| &item.node_id)
            .collect::<HashSet<_>>();
        if selected.len() != activations.len()
            || selected != condition.nodes.iter().collect::<HashSet<_>>()
            || activations
                .iter()
                .any(|activation| !self.is_current_accepted(activation))
        {
            return Err(TurnContractError::InvalidTransition(
                "condition must select its exact current accepted node scope",
            ));
        }
        Ok(())
    }

    /// A failed observation settles one check execution too. A caller reserving
    /// capacity can distinguish absent/stale evidence from a fresh failed result.
    pub fn current_condition(&self, condition_id: &ConditionId) -> Option<&ConditionObservation> {
        let observation = self
            .conditions
            .iter()
            .rev()
            .find(|item| item.condition_id == *condition_id)?;
        let observed_epoch = self
            .epochs
            .iter()
            .position(|epoch| epoch.id == observation.epoch_id)?;
        let latest_admission = self.epochs.iter().rposition(|epoch| {
            epoch
                .continuation
                .as_ref()
                .is_some_and(|plan| plan.condition_runs.contains(condition_id))
        });
        if latest_admission.is_some_and(|admitted| admitted > observed_epoch) {
            return None;
        }
        self.validate_condition_selection(condition_id, &observation.activations)
            .ok()?;
        Some(observation)
    }

    pub fn condition_satisfied(&self, condition_id: &ConditionId) -> bool {
        self.current_condition(condition_id)
            .is_some_and(|item| item.outcome == ConditionOutcome::Passed)
    }

    pub fn completion_satisfied(&self) -> bool {
        if self
            .blockers
            .iter()
            .any(|item| item.state == TurnBlockerState::Pending)
        {
            return false;
        }
        self.graph.as_ref().is_some_and(|graph| {
            graph.nodes.iter().filter(|node| node.required).all(|node| {
                self.latest_activation(&node.node_id)
                    .is_some_and(|item| self.is_current_accepted(&item.activation))
            }) && graph
                .conditions
                .iter()
                .all(|condition| self.condition_satisfied(&condition.condition_id))
        })
    }

    /// Read-only forecast for a revision waiting on one exact invocation. The
    /// returned result is not an executable event or proof of settlement; the
    /// ordinary append path still requires all actual outcomes. No audit record
    /// is invented, and the hypothetical projection never leaves this method.
    pub fn preview_revision_after_invocation_settles(
        &self,
        envelope: &TurnContractEnvelope,
        invocation: &InvocationId,
    ) -> Result<(), TurnContractError> {
        if !matches!(envelope.event, TurnContractEvent::ReviseAccepted { .. })
            || !self.invocations.iter().any(|item| {
                &item.invocation_id == invocation
                    && item.evidence.disposition() == EffectDisposition::OutcomeUnknown
            })
        {
            return Err(TurnContractError::InvalidTransition(
                "revision forecast needs an exact pending invocation",
            ));
        }
        let mut forecast = self.clone();
        forecast
            .invocations
            .retain(|item| &item.invocation_id != invocation);
        forecast.apply(envelope).map(|_| ())
    }

    fn revise_accepted(
        &mut self,
        previous: &ActivationRef,
        input: &ActivationInputManifest,
        invalidated_descendants: &[ActivationRef],
    ) -> Result<(), TurnContractError> {
        self.require_live_activation(&input.activation)?;
        self.require_previous(previous, &input.activation.node_id)?;
        if !self.is_current_accepted(previous) || self.has_unknown_effects() {
            return Err(TurnContractError::InvalidTransition(
                "revision requires current acceptance and settled effects",
            ));
        }
        let old = self.latest_activation(&previous.node_id).ok_or(
            TurnContractError::InvalidTransition("revision target is absent"),
        )?;
        Self::require_stable_input(&old.input, input)?;
        if old.input.parents != input.parents {
            return Err(TurnContractError::InvalidTransition(
                "accepted revision must preserve current parent selection",
            ));
        }
        if input.revision_context.as_ref().is_some_and(|context| {
            context.activation != *previous || old.output.as_ref() != Some(&context.output)
        }) {
            return Err(TurnContractError::InvalidTransition(
                "revision context must name the target's exact accepted output",
            ));
        }
        if old.input.guidance == input.guidance
            && old.input.attachments == input.attachments
            && old.input.repository == input.repository
            && old.input.revision_context == input.revision_context
        {
            return Err(TurnContractError::InvalidTransition(
                "accepted revision requires an explicit input change",
            ));
        }
        let graph = self
            .graph
            .as_ref()
            .ok_or(TurnContractError::InvalidTransition("turn has no graph"))?;
        let mut affected_nodes = HashSet::from([previous.node_id.clone()]);
        loop {
            let before = affected_nodes.len();
            for edge in &graph.dependencies {
                if affected_nodes.contains(&edge.parent) {
                    affected_nodes.insert(edge.child.clone());
                }
            }
            if before == affected_nodes.len() {
                break;
            }
        }
        let expected = graph
            .nodes
            .iter()
            .filter(|node| {
                node.node_id != previous.node_id && affected_nodes.contains(&node.node_id)
            })
            .filter_map(|node| self.latest_activation(&node.node_id))
            .collect::<Vec<_>>();
        if expected.len() != invalidated_descendants.len()
            || invalidated_descendants
                .iter()
                .map(|item| &item.node_id)
                .collect::<HashSet<_>>()
                .len()
                != invalidated_descendants.len()
            || expected.iter().any(|item| {
                item.state == ActivationState::Running
                    || !invalidated_descendants.contains(&item.activation)
            })
        {
            return Err(TurnContractError::InvalidTransition("revision must invalidate every exact materialized descendant after execution settles"));
        }
        for item in &mut self.activations {
            if item.activation == *previous || invalidated_descendants.contains(&item.activation) {
                item.state = ActivationState::Superseded;
            }
        }
        self.admit_activation(
            input,
            ActivationState::Unstarted,
            Admission::Revision,
            false,
        )
    }

    fn latest_activation(&self, node_id: &TurnNodeId) -> Option<&ContractActivation> {
        self.activations
            .iter()
            .rev()
            .find(|item| item.activation.node_id == *node_id)
    }

    fn validate_checkpoint_identity(
        &self,
        checkpoint: &CheckpointRef,
    ) -> Result<(), TurnContractError> {
        for node in self
            .graph
            .iter()
            .chain(self.graph_history.iter().map(|record| &record.previous))
            .flat_map(|graph| &graph.nodes)
        {
            if let ConversationSavepoint::Checkpoint {
                checkpoint: existing,
            } = &node.starting_savepoint
            {
                if existing.checkpoint_id == checkpoint.checkpoint_id
                    && existing.as_ref() != checkpoint
                {
                    return Err(TurnContractError::InvalidTransition(
                        "checkpoint identity conflicts with a retained graph savepoint",
                    ));
                }
            }
        }
        for item in &self.activations {
            let starting = match &item.input.starting_savepoint {
                ConversationSavepoint::Checkpoint { checkpoint } => Some(checkpoint.as_ref()),
                ConversationSavepoint::Empty => None,
            };
            for existing in starting.into_iter().chain(item.checkpoint.as_ref()) {
                if existing.checkpoint_id == checkpoint.checkpoint_id && existing != checkpoint {
                    return Err(TurnContractError::InvalidTransition(
                        "checkpoint identity changed ownership or content reference",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_accepted_checkpoint(
        &self,
        activation: &ActivationRef,
        checkpoint: &CheckpointRef,
    ) -> Result<(), TurnContractError> {
        let item = self
            .activations
            .iter()
            .find(|item| item.activation == *activation)
            .ok_or(TurnContractError::InvalidTransition(
                "exact activation does not exist",
            ))?;
        if checkpoint.session_id != activation.session_id
            || checkpoint.conversation_id != item.conversation_id
            || checkpoint.source
                != (CheckpointSource::Accepted {
                    activation: activation.clone(),
                })
        {
            return Err(TurnContractError::InvalidTransition(
                "accepted checkpoint must name its exact activation and conversation",
            ));
        }
        self.validate_checkpoint_identity(checkpoint)
    }

    fn validate_inputs(&self, input: &ActivationInputManifest) -> Result<(), TurnContractError> {
        let node = self.graph_node(&input.activation.node_id)?;
        if node.definition != input.definition
            || node.conversation_id != input.conversation_id
            || node.starting_savepoint != input.starting_savepoint
        {
            return Err(TurnContractError::InvalidTransition(
                "input must preserve declared definition, conversation and initial savepoint",
            ));
        }
        let required_parents = self
            .graph
            .iter()
            .flat_map(|graph| &graph.dependencies)
            .filter(|edge| edge.child == input.activation.node_id)
            .map(|edge| &edge.parent)
            .collect::<HashSet<_>>();
        if input
            .parents
            .iter()
            .map(|parent| &parent.activation.node_id)
            .collect::<HashSet<_>>()
            != required_parents
        {
            return Err(TurnContractError::InvalidTransition(
                "input must select every declared direct parent and no other node",
            ));
        }
        if input
            .grant
            .as_ref()
            .is_some_and(|grant| grant.revision == 0)
        {
            return Err(TurnContractError::InvalidTransition(
                "grant snapshot revision must be positive",
            ));
        }
        for references in [&input.guidance, &input.attachments] {
            if references.iter().collect::<HashSet<_>>().len() != references.len() {
                return Err(TurnContractError::InvalidTransition(
                    "duplicate input evidence reference",
                ));
            }
        }
        if let ConversationSavepoint::Checkpoint { checkpoint } = &input.starting_savepoint {
            if checkpoint.session_id != input.activation.session_id
                || checkpoint.conversation_id != input.conversation_id
            {
                return Err(TurnContractError::InvalidTransition(
                    "starting checkpoint belongs to another conversation",
                ));
            }
            self.validate_checkpoint_identity(checkpoint)?;
            if let CheckpointSource::Accepted { activation } = &checkpoint.source {
                let accepted = self.latest_activation(&activation.node_id).filter(|item| {
                    item.activation == *activation
                        && item.state == ActivationState::Accepted
                        && item.checkpoint.as_ref() == Some(checkpoint.as_ref())
                });
                if accepted.is_none() || activation.node_id != input.activation.node_id {
                    return Err(TurnContractError::InvalidTransition(
                        "starting checkpoint is not current accepted node state",
                    ));
                }
            }
        }
        let mut parents = HashSet::new();
        for parent in &input.parents {
            if parent.activation.node_id == input.activation.node_id
                || !parents.insert(&parent.activation.node_id)
            {
                return Err(TurnContractError::InvalidTransition(
                    "parent input is self-referential or duplicated",
                ));
            }
            let accepted = self
                .latest_activation(&parent.activation.node_id)
                .filter(|item| {
                    item.activation == parent.activation
                        && item.state == ActivationState::Accepted
                        && item.checkpoint.as_ref() == Some(&parent.checkpoint)
                        && item.output.as_ref() == Some(&parent.output)
                });
            if accepted.is_none() {
                return Err(TurnContractError::InvalidTransition(
                    "parent input does not select exact current accepted evidence",
                ));
            }
        }
        Ok(())
    }

    fn require_original_retry_input(
        previous: &ActivationInputManifest,
        next: &ActivationInputManifest,
    ) -> Result<(), TurnContractError> {
        if previous.definition != next.definition
            || previous.conversation_id != next.conversation_id
            || previous.starting_savepoint != next.starting_savepoint
            || previous.parents != next.parents
            || previous.guidance != next.guidance
            || previous.attachments != next.attachments
            || previous.repository != next.repository
            || previous.budget != next.budget
            || previous.revision_context != next.revision_context
            || !Self::retry_grant_reference(&previous.grant, &next.grant)
        {
            return Err(TurnContractError::InvalidTransition(
                "retry must preserve its original input manifest and starting savepoint",
            ));
        }
        Ok(())
    }

    fn retry_grant_reference(
        previous: &Option<GrantSnapshotRef>,
        next: &Option<GrantSnapshotRef>,
    ) -> bool {
        match (previous, next) {
            (None, None) => true,
            (Some(previous), Some(next)) if previous.grant_id == next.grant_id => {
                // A narrowed canonical grant can replace its prior snapshot
                // without resetting model inputs or the logical budget. The
                // control arbiter, not this reference, proves the narrowing.
                (next.revision == previous.revision && next.evidence == previous.evidence)
                    || (next.revision > previous.revision && next.evidence != previous.evidence)
            }
            _ => false,
        }
    }

    fn admit_activation(
        &mut self,
        input: &ActivationInputManifest,
        state: ActivationState,
        admission: Admission,
        continuation: bool,
    ) -> Result<(), TurnContractError> {
        let activation = &input.activation;
        self.require_live_activation(activation)?;
        if !continuation && self.current_epoch_blocks(&activation.node_id) {
            return Err(TurnContractError::InvalidTransition(
                "recovery explicitly left this node blocked",
            ));
        }
        if self.activations.iter().any(|item| {
            item.activation.activation_id == activation.activation_id
                || item.input.manifest_id == input.manifest_id
                || (item.activation.node_id != activation.node_id
                    && item.conversation_id == input.conversation_id)
        }) {
            return Err(TurnContractError::InvalidTransition(
                "activation, manifest, or conversation identity is already owned",
            ));
        }
        let previous = self.latest_activation(&activation.node_id);
        let generation = if let Some(previous) = previous {
            // A pending exact-generation wait cannot be bypassed by admitting
            // a new generation directly from a prepared activation.
            self.require_unblocked(&previous.activation)?;
            match admission {
                Admission::Retry => {
                    if !matches!(
                        previous.state,
                        ActivationState::Failed
                            | ActivationState::Interrupted
                            | ActivationState::Unstarted
                    ) {
                        return Err(TurnContractError::InvalidTransition(
                            "node is not eligible for retry",
                        ));
                    }
                    Self::require_original_retry_input(&previous.input, input)?;
                }
                Admission::Rebase => {
                    if previous.state != ActivationState::Superseded {
                        return Err(TurnContractError::InvalidTransition(
                            "rebase requires a superseded generation",
                        ));
                    }
                    Self::require_rebased_input(&previous.input, input)?;
                }
                Admission::Revision => {
                    if previous.state != ActivationState::Superseded {
                        return Err(TurnContractError::InvalidTransition(
                            "revision requires explicit invalidation",
                        ));
                    }
                }
            }
            if !continuation
                && admission == Admission::Retry
                && previous.activation.execution_epoch_id != activation.execution_epoch_id
            {
                return Err(TurnContractError::InvalidTransition(
                    "cross-epoch retry requires atomic continuation",
                ));
            }
            previous
                .activation
                .generation
                .checked_add(1)
                .ok_or(TurnContractError::Overflow)?
        } else {
            if admission != Admission::Retry || input.revision_context.is_some() {
                return Err(TurnContractError::InvalidTransition(
                    "initial activation cannot reference revision history",
                ));
            }
            1
        };
        if activation.generation != generation
            || self.invocations.iter().any(|item| {
                item.activation.node_id == activation.node_id
                    && item.evidence.disposition() == EffectDisposition::OutcomeUnknown
            })
        {
            return Err(TurnContractError::InvalidTransition(
                "retry generation or effect recovery is unresolved",
            ));
        }
        self.validate_inputs(input)?;
        self.activations.push(ContractActivation {
            activation: activation.clone(),
            conversation_id: input.conversation_id.clone(),
            input: input.clone(),
            state,
            checkpoint: None,
            output: None,
            failure: None,
        });
        Ok(())
    }

    fn current_epoch_blocks(&self, node_id: &TurnNodeId) -> bool {
        self.epochs
            .last()
            .and_then(|epoch| epoch.continuation.as_ref())
            .is_some_and(|plan| {
                plan.selections.iter().any(|selection| match selection {
                    ContinuationSelection::LeaveBlocked { activation, .. } => {
                        activation.node_id == *node_id
                    }
                    ContinuationSelection::LeaveUnmaterializedBlocked {
                        node_id: blocked, ..
                    } => blocked == node_id,
                    _ => false,
                })
            })
    }

    fn require_previous(
        &self,
        previous: &ActivationRef,
        node: &TurnNodeId,
    ) -> Result<(), TurnContractError> {
        if previous.node_id != *node
            || !self
                .latest_activation(node)
                .is_some_and(|item| item.activation == *previous)
        {
            return Err(TurnContractError::InvalidTransition(
                "selection is not the latest exact activation",
            ));
        }
        Ok(())
    }

    fn require_stable_input(
        previous: &ActivationInputManifest,
        next: &ActivationInputManifest,
    ) -> Result<(), TurnContractError> {
        if previous.definition != next.definition
            || previous.conversation_id != next.conversation_id
            || previous.starting_savepoint != next.starting_savepoint
            || previous.budget != next.budget
            || !Self::retry_grant_reference(&previous.grant, &next.grant)
        {
            return Err(TurnContractError::InvalidTransition(
                "revision must preserve node ownership, original savepoint and budget identity",
            ));
        }
        Ok(())
    }

    fn require_rebased_input(
        previous: &ActivationInputManifest,
        next: &ActivationInputManifest,
    ) -> Result<(), TurnContractError> {
        Self::require_stable_input(previous, next)?;
        if previous.guidance != next.guidance
            || previous.attachments != next.attachments
            || previous.repository != next.repository
            || next.revision_context.is_some()
        {
            return Err(TurnContractError::InvalidTransition(
                "descendant rebase changes parent selections and removes obsolete revision context",
            ));
        }
        Ok(())
    }

    fn apply_continuation(&mut self, plan: &ContinuationPlan) -> Result<(), TurnContractError> {
        if self.state != Some(LogicalTurnState::NeedsAttention)
            || !self.epochs.last().is_some_and(|epoch| {
                epoch.id == plan.source_epoch_id
                    && matches!(epoch.state, EpochState::Interrupted | EpochState::Paused)
            })
            || self.epochs.iter().any(|epoch| epoch.id == plan.epoch_id)
        {
            return Err(TurnContractError::InvalidTransition(
                "continuation requires the exact settled epoch and a new epoch",
            ));
        }
        self.require_settled_activations()?;
        if self.has_unknown_effects() {
            return Err(TurnContractError::InvalidTransition(
                "unknown invocations require authoritative recovery evidence",
            ));
        }
        let mut revision_nodes = HashSet::new();
        let mut revision_descendants = Vec::new();
        for selection in &plan.selections {
            if let ContinuationSelection::Revise {
                previous,
                invalidated_descendants,
                ..
            } = selection
            {
                if !revision_nodes.insert(previous.node_id.clone()) {
                    return Err(TurnContractError::InvalidTransition(
                        "continuation revisions overlap",
                    ));
                }
                for descendant in invalidated_descendants {
                    if !revision_nodes.insert(descendant.node_id.clone()) {
                        return Err(TurnContractError::InvalidTransition(
                            "continuation revisions overlap",
                        ));
                    }
                    revision_descendants.push(descendant.clone());
                }
            }
        }
        let graph = self
            .graph
            .as_ref()
            .ok_or(TurnContractError::InvalidTransition("turn has no graph"))?;
        let declared = graph
            .nodes
            .iter()
            .map(|node| &node.node_id)
            .collect::<HashSet<_>>();
        let mut covered = HashSet::new();
        let mut prepared = 0;
        for selection in &plan.selections {
            let node = match selection {
                ContinuationSelection::RetainAccepted { activation }
                | ContinuationSelection::LeaveBlocked { activation, .. } => &activation.node_id,
                ContinuationSelection::Retry { previous, .. }
                | ContinuationSelection::Rebase { previous, .. }
                | ContinuationSelection::Revise { previous, .. } => &previous.node_id,
                ContinuationSelection::PrepareUnmaterialized { input } => &input.activation.node_id,
                ContinuationSelection::AwaitDependencies { node_id }
                | ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. } => node_id,
            };
            if !declared.contains(node) || !covered.insert(node) {
                return Err(TurnContractError::InvalidTransition(
                    "recovery must select each declared node exactly once",
                ));
            }
            let previous = self.latest_activation(node);
            match selection {
                ContinuationSelection::RetainAccepted { activation } => {
                    if !self.is_current_accepted(activation)
                        || revision_nodes.contains(&activation.node_id)
                    {
                        return Err(TurnContractError::InvalidTransition(
                            "retained activation is not current accepted evidence",
                        ));
                    }
                }
                ContinuationSelection::LeaveBlocked { activation, .. } => {
                    self.require_previous(activation, node)?;
                    if !previous.is_some_and(|item| {
                        matches!(
                            item.state,
                            ActivationState::Failed
                                | ActivationState::Interrupted
                                | ActivationState::Unstarted
                                | ActivationState::Superseded
                        )
                    }) {
                        return Err(TurnContractError::InvalidTransition(
                            "blocked selection must remain unfinished",
                        ));
                    }
                }
                ContinuationSelection::Retry {
                    previous: selected,
                    input,
                }
                | ContinuationSelection::Rebase {
                    previous: selected,
                    input,
                }
                | ContinuationSelection::Revise {
                    previous: selected,
                    input,
                    ..
                } => {
                    self.require_previous(selected, node)?;
                    if input.activation.node_id != *node
                        || input.activation.execution_epoch_id != plan.epoch_id
                    {
                        return Err(TurnContractError::InvalidTransition(
                            "recovery input must name selected node and new epoch",
                        ));
                    }
                    prepared += 1;
                }
                ContinuationSelection::PrepareUnmaterialized { input } => {
                    if previous.is_some() || input.activation.execution_epoch_id != plan.epoch_id {
                        return Err(TurnContractError::InvalidTransition(
                            "new recovery work must be unmaterialized and owned by new epoch",
                        ));
                    }
                    prepared += 1;
                }
                ContinuationSelection::AwaitDependencies { .. } => {
                    let awaits_revision = previous
                        .is_some_and(|item| revision_descendants.contains(&item.activation));
                    if !awaits_revision
                        && (previous.is_some()
                            || !graph
                                .dependencies
                                .iter()
                                .filter(|edge| edge.child == *node)
                                .any(|edge| {
                                    revision_nodes.contains(&edge.parent)
                                        || !self.latest_activation(&edge.parent).is_some_and(
                                            |item| self.is_current_accepted(&item.activation),
                                        )
                                }))
                    {
                        return Err(TurnContractError::InvalidTransition(
                            "unmaterialized dependency wait requires an unfinished parent",
                        ));
                    }
                }
                ContinuationSelection::LeaveUnmaterializedBlocked { .. } => {
                    if previous.is_some() {
                        return Err(TurnContractError::InvalidTransition(
                            "unmaterialized blocked node already has history",
                        ));
                    }
                }
            }
        }
        let mut conditions = HashSet::new();
        for condition_id in &plan.condition_runs {
            if !conditions.insert(condition_id)
                || !graph
                    .conditions
                    .iter()
                    .any(|condition| condition.condition_id == *condition_id)
            {
                return Err(TurnContractError::InvalidTransition(
                    "recovery check admission requires unique declared conditions",
                ));
            }
        }
        if covered != declared || (prepared == 0 && plan.condition_runs.is_empty()) {
            return Err(TurnContractError::InvalidTransition(
                "recovery must cover every graph node and prepare executable work or select pending checks",
            ));
        }
        // apply() commits the clone only after every generation and its inputs
        // validate. No partial epoch, node selection or activation can escape.
        self.epochs.push(ExecutionEpoch {
            id: plan.epoch_id.clone(),
            state: EpochState::Running,
            continuation: Some(plan.clone()),
        });
        self.state = Some(LogicalTurnState::Running);
        for selection in &plan.selections {
            if let ContinuationSelection::Revise {
                previous,
                input,
                invalidated_descendants,
                evidence,
            } = selection
            {
                if !input.guidance.contains(evidence) {
                    return Err(TurnContractError::InvalidTransition(
                        "revision instruction is absent from its input",
                    ));
                }
                self.revise_accepted(previous, input, invalidated_descendants)?;
                for activation in invalidated_descendants {
                    self.abandon_blockers(activation, evidence);
                }
            }
        }
        for selection in &plan.selections {
            match selection {
                ContinuationSelection::Retry { input, .. }
                | ContinuationSelection::PrepareUnmaterialized { input } => self.admit_activation(
                    input,
                    ActivationState::Unstarted,
                    Admission::Retry,
                    true,
                )?,
                ContinuationSelection::Rebase { input, .. } => self.admit_activation(
                    input,
                    ActivationState::Unstarted,
                    Admission::Rebase,
                    true,
                )?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Unknown tool and check executions survive interruption. Neither a new
    /// epoch nor a readiness observation proves that an old effect is settled.
    pub fn has_unknown_effects(&self) -> bool {
        self.invocations
            .iter()
            .any(|item| item.evidence.disposition() == EffectDisposition::OutcomeUnknown)
            || self
                .condition_runs
                .iter()
                .any(|item| item.resolution.is_none())
    }

    fn require_settled_activations(&self) -> Result<(), TurnContractError> {
        if self
            .activations
            .iter()
            .any(|item| item.state == ActivationState::Running)
        {
            return Err(TurnContractError::InvalidTransition(
                "running activations have not settled",
            ));
        }
        Ok(())
    }

    fn require_epoch(&self, epoch_id: &ExecutionEpochId) -> Result<(), TurnContractError> {
        if self.state != Some(LogicalTurnState::Running)
            || !self
                .epochs
                .last()
                .is_some_and(|item| item.id == *epoch_id && item.state == EpochState::Running)
        {
            return Err(TurnContractError::InvalidTransition(
                "epoch is not the live owner",
            ));
        }
        Ok(())
    }

    fn require_live_activation(&self, activation: &ActivationRef) -> Result<(), TurnContractError> {
        if self.session_id.as_ref() != Some(&activation.session_id)
            || self.turn_id.as_ref() != Some(&activation.turn_id)
        {
            return Err(TurnContractError::InvalidTransition(
                "activation belongs to another turn",
            ));
        }
        self.require_epoch(&activation.execution_epoch_id)
    }

    fn current_epoch_mut(&mut self) -> Result<&mut ExecutionEpoch, TurnContractError> {
        self.epochs
            .last_mut()
            .ok_or(TurnContractError::InvalidTransition("turn has no epoch"))
    }

    fn running_activation_mut(
        &mut self,
        activation: &ActivationRef,
    ) -> Result<&mut ContractActivation, TurnContractError> {
        self.activations
            .iter_mut()
            .find(|item| item.activation == *activation && item.state == ActivationState::Running)
            .ok_or(TurnContractError::InvalidTransition(
                "exact activation is not running",
            ))
    }

    fn unresolved_invocation_mut(
        &mut self,
        invocation_id: &InvocationId,
    ) -> Result<&mut ContractInvocation, TurnContractError> {
        self.invocations
            .iter_mut()
            .find(|item| {
                item.invocation_id == *invocation_id && item.evidence == InvocationEvidence::Intent
            })
            .ok_or(TurnContractError::InvalidTransition(
                "invocation has no unresolved durable intent",
            ))
    }
}
