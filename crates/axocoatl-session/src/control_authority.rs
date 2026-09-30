//! Durable grant limits and one atomic Stop/dispatch gate for a logical turn.
//!
//! These are controller primitives, not an authenticated route or a complete
//! command controller. The host installs human-approved grants and registers
//! activations only after canonical graph/input/epoch admission. Agent-facing
//! callers receive opaque generation leases; JSON cannot manufacture them.
//! Every grant, activation, stop and dispatch reservation is durable before its
//! acknowledgement. Provider/tool execution happens after this mutex is released.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use axocoatl_core::{MeasuredTokenUsage, SecureDir, TokenUsageStats};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::control_command::TrustedCommandSource;
use crate::execution_content::{
    ActivationEvidenceContent, DurableConditionArguments, DurableConditionResult,
    ExecutionContentStore,
};
use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_store::{DurableTurnSnapshot, SessionExecutionStore};
use crate::invocation_audit::{
    DurableIntentReceipt, InvocationAudit, InvocationIntent, ProtectedArguments,
};
use crate::turn_contract::{
    ActivationRef, ActivationState, CompletionCondition, ConditionKind, ConditionRunId,
    ConditionRunRef, DefinitionSnapshotRef, DelegatedOperation, DelegatedOperationPermission,
    DelegatedTargetScope, DelegationScope, EpochState, EvidenceRef, GrantId, GrantSnapshotRef,
    InvocationId, LogicalTurnId, LogicalTurnState, MachineBlockerPermission, SessionId, TurnNodeId,
    MAX_COMPLETION_CONDITIONS, MAX_CONTRACT_NODES, MAX_GRAPH_EDGES,
};

#[path = "control_authority_checks.rs"]
mod checks;
pub use checks::{pays_with_own_shell, RequiredCheckPayer};
#[path = "control_authority_delegation.rs"]
mod delegation;
pub use delegation::DelegatedGrantReservation;

const FILE: &str = "control-authority.v1.json";
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_GRANTS: usize = 64;
const MAX_ACTIVATIONS: usize = 512;
const MAX_CLAIMS: usize = 4096;
/// The host's own repository capture of an activation whose writes are
/// limited to named paths but that has no shell: one fixed, read-only
/// observation before and one after it, so every change it makes is judged.
/// It spends the activation's invocation allowance like any tool; an Agent
/// cannot call it.
pub const REPOSITORY_CAPTURE_PORT: &str = "repository_capture";

pub const MAX_PROVIDER_REQUEST_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PROVIDER_RESPONSE_BYTES: u64 = 1024 * 1024;
// A terminal record has bounded enum/numeric/boolean fields only. Reserve its
// maximum serialized width before dispatch, independently of later admissions.
const PROVIDER_OUTCOME_RESERVE: usize = 1024;
const CONDITION_OUTCOME_RESERVE: usize = 1024;

/// Host port through which a delegation holder admits one fresh helper Agent.
pub const DELEGATE_TOOL: &str = "delegate";
/// Host port for reading the bound workspace and staging knowledge proposals.
pub const KNOWLEDGE_TOOL: &str = "workspace_knowledge";

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("authority storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("authority JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid authority contract: {0}")]
    Invalid(&'static str),
    #[error("stale or foreign authority lease")]
    StaleLease,
    #[error("dispatch/control is outside the current grant")]
    Denied,
    #[error("authority budget or storage capacity exhausted")]
    Capacity,
    #[error("authority write or lock failed; reconstruct before more work")]
    RecoveryRequired,
}

/// Exact execution policy. Tool names are an allowlist; empty means no tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    pub definition: String,
    pub provider: String,
    pub model: String,
    pub isolation: String,
    pub tools: Vec<String>,
    /// Repository path patterns this activation may change (`path_scope`).
    /// Absent leaves every path open and keeps the historical serialized
    /// shape; empty is a read-only activation that may claim no write tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_scope: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantLimits {
    pub activations: u32,
    pub invocations: u32,
    pub tokens: u64,
    pub cost_microunits: u64,
}

/// The host resolves a canonical descendant scope before granting authority.
/// This module checks exact membership, not a model-supplied topology assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityGrant {
    pub id: String,
    pub revision: u64,
    pub issuer_evidence: EvidenceRef,
    pub holder: TurnNodeId,
    pub descendants: Vec<TurnNodeId>,
    /// Compatibility-only explicit v1 permission. Must be false with delegation.
    pub allow_stop_descendants: bool,
    /// Absence preserves the exact legacy serialized shape and grants no new
    /// operations. Presence is the sole operation policy; flags are never merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Box<DelegationPolicy>>,
    pub profiles: Vec<ExecutionProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ConditionPermission>,
    pub limits: GrantLimits,
    pub expires_at_ms: u64,
}

impl AuthorityGrant {
    /// Validate the existing grant contract before host setup writes Begin.
    /// Structural validity is not human approval or execution authority.
    pub fn validate(&self) -> Result<(), AuthorityError> {
        validate_grant(self)
    }
}

/// Explicit operation contract retained in the existing grant journal. Budgets,
/// profiles, expiry, issuer and revocation remain AuthorityGrant/GrantRecord fields.
/// All values require human approval; there are no implicit limits or operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationPolicy {
    pub schema_version: u32,
    pub scope: DelegationScope,
    pub operations: Vec<DelegatedOperationPermission>,
    pub templates: Vec<DefinitionSnapshotRef>,
    /// Exact retained repository/resource policy. Its resolver must enforce the
    /// resource limits and each ExecutionProfile's isolation; an ID alone is not
    /// a capability. The policy is immutable during narrowing.
    pub resource_policy: EvidenceRef,
    pub graph_limits: DelegatedGraphLimits,
    /// These cannot be removed or weakened by a narrowed grant or normal Finish.
    pub required_conditions: Vec<CompletionCondition>,
    pub completion_criteria: Vec<EvidenceRef>,
    pub machine_blockers: Vec<MachineBlockerPermission>,
    pub replay_policy: DelegatedReplayPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedGraphLimits {
    pub max_nodes: u32,
    pub max_edges: u32,
}

/// Permission to request a replay never proves safety or delegates human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedReplayPolicy {
    RequireProvedEffectSafety,
}

/// Exact revocation is projected from the same durable record as dispatch. It is
/// not a caller-supplied policy field that can be reset by resubmitting a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityGrantStatus {
    pub policy: AuthorityGrant,
    pub authority_revision: u64,
    pub revoked_at_revision: Option<u64>,
}

impl AuthorityGrant {
    /// Pure policy membership only. Callers still need a live exact lease,
    /// matching owner/revisions, legal graph, retained evidence and effect checks.
    pub fn permits_operation(&self, operation: DelegatedOperation, node: &TurnNodeId) -> bool {
        if !node_allowed(self, node)
            || (operation == DelegatedOperation::StopActivation && node == &self.holder)
        {
            return false;
        }
        match &self.delegation {
            Some(policy) => {
                !self.allow_stop_descendants
                    && policy.operations.iter().any(|p| {
                        p.operation == operation
                            && match &p.targets {
                                DelegatedTargetScope::Nodes { nodes } => nodes.contains(node),
                                // Only the canonical controller can prove a subtree target.
                                DelegatedTargetScope::Subtree { .. } => false,
                            }
                    })
            }
            None => {
                operation == DelegatedOperation::StopActivation
                    && self.allow_stop_descendants
                    && self.descendants.contains(node)
            }
        }
    }
}

/// Explicit check authority, independent of a completed Agent's closed lease.
/// Resource and definition references identify retained evidence, never ambient
/// paths. The executor must implement the named isolation and enforce bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionPermission {
    pub kind: ConditionKind,
    pub nodes: Vec<TurnNodeId>,
    pub repository: EvidenceRef,
    pub isolation: String,
    pub max_timeout_ms: u64,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionCallRecord {
    pub run: ConditionRunRef,
    pub intent: EvidenceRef,
    pub definition: EvidenceRef,
    pub repository: EvidenceRef,
    pub isolation: String,
    pub timeout_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub arguments: ProtectedArguments,
    pub grant: GrantSnapshotRef,
    pub claimed_at_ms: u64,
    pub dispatch_scope: String,
    pub result: Option<ProtectedArguments>,
}

/// One executable check claim; reopening or repeating a run never recreates it.
#[derive(Debug)]
pub struct ConditionCallClaim {
    journal_id: String,
    record: ConditionCallRecord,
}
impl ConditionCallClaim {
    pub fn run(&self) -> &ConditionRunRef {
        &self.record.run
    }
    pub fn arguments(&self) -> &ProtectedArguments {
        &self.record.arguments
    }
    pub fn grant(&self) -> &GrantSnapshotRef {
        &self.record.grant
    }
    pub fn dispatch_scope(&self) -> &str {
        &self.record.dispatch_scope
    }
}

/// Evidence-only recovery capability. It cannot authorize process execution.
pub struct ConditionSettlementReceipt {
    journal_id: String,
    record: ConditionCallRecord,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantUsage {
    pub activations: u32,
    pub invocations: u32,
    pub tokens: u64,
    pub cost_microunits: u64,
}

/// A bound reserved before dispatch, retained conservatively across retries.
/// Executors must enforce these bounds; an estimate is not a guaranteed cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchReservation {
    pub tokens: u64,
    pub cost_microunits: u64,
}

/// Exact request identity and backend-enforced bounds, supplied by the host.
/// This record cannot itself establish that a provider enforces those bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallIntent {
    pub call_id: String,
    pub provider: String,
    pub model: String,
    pub request_sha256: String,
    pub request_bytes: u64,
    pub reservation: DispatchReservation,
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallTerminal {
    Completed,
    Failed,
    Interrupted,
}

/// Observed accounting, independent of accepted conversation state. An observed
/// overrun remains evidence even though the entire reservation stays charged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallOutcome {
    pub kind: ProviderCallTerminal,
    pub usage: MeasuredTokenUsage,
    pub cost_microunits: Option<u64>,
    pub cost_known: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderUsage {
    pub tokens: MeasuredTokenUsage,
    pub cost_microunits: u64,
    pub cost_known: bool,
    pub calls: u32,
    pub unsettled_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallRecord {
    pub intent: ProviderCallIntent,
    pub activation: ActivationRef,
    pub grant_id: String,
    pub grant_revision: u64,
    pub dispatch_scope: String,
    pub outcome: Option<ProviderCallOutcome>,
}

/// A single durable provider claim. No duplicate call id can mint another
/// dispatch claim, including after restart or acknowledgement loss.
#[derive(Debug)]
pub struct ProviderCallClaim {
    canonical_journal_id: Option<String>,
    record: ProviderCallRecord,
}

impl ProviderCallClaim {
    pub fn call_id(&self) -> &str {
        &self.record.intent.call_id
    }
    pub fn activation(&self) -> &ActivationRef {
        &self.record.activation
    }
    pub fn intent(&self) -> &ProviderCallIntent {
        &self.record.intent
    }
}

/// Recovery can obtain evidence-only settlement capability, never another
/// executable claim. It is intentionally a distinct, opaque type.
pub struct ProviderSettlementReceipt {
    canonical_journal_id: Option<String>,
    record: ProviderCallRecord,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantRecord {
    policy: AuthorityGrant,
    previous_policies: Vec<AuthorityGrant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    expansions: Vec<expansion::ApprovedExpansion>,
    revoked_at_revision: Option<u64>,
    usage: GrantUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_delegation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delegated_from: Option<DelegatedGrantReservation>,
    /// Allowance carried in by the removed standing-work inbox. Read so its
    /// closed turns still balance; nothing writes a new one.
    #[serde(default, rename = "standing", skip_serializing_if = "Option::is_none")]
    legacy_standing: Option<LegacyStandingCarry>,
    /// Permission to run this turn's required checks, when this grant pays
    /// for them. Absent on every other grant and on older stores.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    host_checks: Vec<ConditionPermission>,
}

/// What a standing-work turn's grant carried: usage its shared allowance had
/// already spent and the check permissions it was armed with. Only its usage
/// and permissions are read; its other fields are kept as they were written.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LegacyStandingCarry {
    consumed_before: GrantUsage,
    #[serde(default)]
    conditions: Vec<ConditionPermission>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationRecord {
    activation: ActivationRef,
    grant_id: String,
    grant_revision: u64,
    profile: ExecutionProfile,
    stopped: bool,
    #[serde(default)]
    provider_gated: bool,
    /// Explicit closed coverage for an activation that never acquired a lease.
    /// Absence preserves historical accounting semantics; it is never inferred.
    #[serde(default, skip_serializing_if = "is_false")]
    never_dispatched: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimRecord {
    audit_id: String,
    intent: InvocationIntent,
    invocation: InvocationId,
    activation: ActivationRef,
    grant_id: String,
    grant_revision: u64,
    dispatch_scope: String,
    reservation: DispatchReservation,
    settled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityData {
    schema_version: u32,
    session_id: SessionId,
    turn_id: LogicalTurnId,
    revision: u64,
    closed: bool,
    /// A lifecycle fence is distinct from terminal Stop/Finish. Only a new
    /// authority scope with the exact interrupted canonical owner may clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lifecycle_suspension: Option<String>,
    grants: Vec<GrantRecord>,
    activations: Vec<ActivationRecord>,
    claims: Vec<ClaimRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_calls: Vec<ProviderCallRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    condition_calls: Vec<ConditionCallRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical_journal_id: Option<String>,
}

struct LiveState {
    data: AuthorityData,
    poisoned: bool,
}

/// Not serializable. A lease belongs to exactly one live authority instance.
#[derive(Debug, Clone)]
pub struct ActivationLease {
    scope: String,
    activation: ActivationRef,
    grant_id: String,
    grant_revision: u64,
}

/// Prepare, persist the matching invocation intent, then consume this ticket.
/// Restart creates a new scope; historical intent cannot become fresh dispatch.
#[derive(Debug)]
pub struct DispatchPreparation {
    scope: String,
    lease: ActivationLease,
    invocation: InvocationId,
    tool_name: String,
    reservation: DispatchReservation,
}

impl DispatchPreparation {
    pub fn dispatch_scope(&self) -> &str {
        &self.scope
    }
}

/// A successful claim means dispatch may already have happened. It is never
/// replayable, even when the caller lost its acknowledgement before execution.
#[derive(Debug)]
pub struct DispatchClaim {
    invocation: InvocationId,
    activation: ActivationRef,
}

impl DispatchClaim {
    pub fn invocation_id(&self) -> &InvocationId {
        &self.invocation
    }
    pub fn activation(&self) -> &ActivationRef {
        &self.activation
    }
}

pub struct ControlAuthority {
    dir: SecureDir,
    namespace: Option<OwnedExecutionNamespace>,
    scope: String,
    state: Mutex<LiveState>,
    capacity_bytes: usize,
}

impl ControlAuthority {
    /// Open an existing private, durably provisioned directory. Reopening closes
    /// every prior generation's gate, while preserving all charges and claims.
    pub fn open(
        path: impl AsRef<Path>,
        session_id: SessionId,
        turn_id: LogicalTurnId,
    ) -> Result<Self, AuthorityError> {
        let dir = SecureDir::open(path)?;
        Self::open_in(dir, session_id, turn_id, None)
    }

    pub fn open_owned(namespace: OwnedExecutionNamespace) -> Result<Self, AuthorityError> {
        let ExecutionComponent::ControlAuthority { turn_id } = namespace.component() else {
            return Err(AuthorityError::Invalid("wrong owned component"));
        };
        let turn_id = turn_id.clone();
        namespace.require_root(&ExecutionComponent::ControlAuthority {
            turn_id: turn_id.clone(),
        })?;
        let session_id = namespace.identity().owner().session_id.clone();
        let dir = namespace.secure_dir()?;
        Self::open_in(dir, session_id, turn_id, Some(namespace))
    }

    /// Inspect an existing historical authority without creating a journal,
    /// resetting gates or minting a live lease. The host proves that the exact
    /// canonical turn is closed and selects conversation membership from that
    /// journal. Absent/older provider coverage is an error, never known zero.
    pub fn read_provider_usage_owned(
        namespace: OwnedExecutionNamespace,
        activations: &[ActivationRef],
    ) -> Result<ProviderUsage, AuthorityError> {
        let ExecutionComponent::ControlAuthority { turn_id } = namespace.component() else {
            return Err(AuthorityError::Invalid(
                "wrong historical authority component",
            ));
        };
        namespace.require_root(&ExecutionComponent::ControlAuthority {
            turn_id: turn_id.clone(),
        })?;
        let bytes = namespace.read_limited(FILE, MAX_BYTES)?;
        let data: AuthorityData = serde_json::from_slice(&bytes)?;
        validate_data(&data)?;
        if &data.turn_id != turn_id
            || data.session_id != namespace.identity().owner().session_id
            || data.canonical_journal_id.as_deref() != Some(namespace.identity().journal_id())
            || (!data.closed
                && data
                    .activations
                    .iter()
                    .any(|activation| !activation.stopped))
        {
            return Err(AuthorityError::Invalid(
                "historical authority is foreign or still open",
            ));
        }
        let usage = provider_usage_for(&data, activations)?;
        // A read of a previously uncertain rename is not a durability barrier.
        // Sync the existing file and directory without rewriting its evidence.
        namespace
            .secure_dir()?
            .open_file_limited(FILE, MAX_BYTES)?
            .sync_all()?;
        namespace.sync_all()?;
        if namespace.read_limited(FILE, MAX_BYTES)? != bytes {
            return Err(AuthorityError::RecoveryRequired);
        }
        Ok(usage)
    }

    fn open_in(
        dir: SecureDir,
        session_id: SessionId,
        turn_id: LogicalTurnId,
        namespace: Option<OwnedExecutionNamespace>,
    ) -> Result<Self, AuthorityError> {
        dir.restrict_owner_only()?;
        #[cfg(unix)]
        dir.try_lock_exclusive()?;
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "authority requires directory locking",
        )
        .into());
        let canonical_journal_id = namespace
            .as_ref()
            .map(|ns| ns.identity().journal_id().to_owned());
        let mut data = match dir.read_limited(FILE, MAX_BYTES) {
            Ok(bytes) => serde_json::from_slice::<AuthorityData>(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(namespace) = &namespace {
                    namespace.check_journal_creation(FILE)?;
                }
                AuthorityData {
                    schema_version: 1,
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    revision: 0,
                    closed: false,
                    lifecycle_suspension: None,
                    grants: vec![],
                    activations: vec![],
                    claims: vec![],
                    provider_calls: vec![],
                    condition_calls: vec![],
                    canonical_journal_id: canonical_journal_id.clone(),
                }
            }
            Err(e) => return Err(e.into()),
        };
        validate_data(&data)?;
        if data.session_id != session_id
            || data.turn_id != turn_id
            || data.canonical_journal_id != canonical_journal_id
        {
            return Err(AuthorityError::Invalid("store belongs to another turn"));
        }
        if data.activations.iter().any(|a| !a.stopped) {
            data.revision = data
                .revision
                .checked_add(1)
                .ok_or(AuthorityError::Capacity)?;
            for activation in &mut data.activations {
                activation.stopped = true;
            }
        }
        // Reading an uncertain prior rename is not a durability barrier.
        if let Some(namespace) = &namespace {
            namespace.mark_journal_initialized(FILE)?;
        }
        dir.atomic_write(FILE, &serde_json::to_vec(&data)?)?;
        Ok(Self {
            dir,
            namespace,
            scope: uuid::Uuid::new_v4().to_string(),
            state: Mutex::new(LiveState {
                data,
                poisoned: false,
            }),
            capacity_bytes: MAX_BYTES,
        })
    }

    /// Host-only installation after exact human grant approval. There is no
    /// Agent lease or deserializable source that can call this as a human.
    pub fn install_grant(
        &self,
        grant: AuthorityGrant,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        validate_grant(&grant)?;
        let mut state = self.lock()?;
        validate_grant_owner(&grant, &state.data)?;
        if let Some(old) = state.data.grants.iter().find(|g| g.policy.id == grant.id) {
            return if old.policy == grant && old.revoked_at_revision.is_none() {
                Ok(())
            } else {
                Err(AuthorityError::Denied)
            };
        }
        check_revision(&state.data, expected_revision)?;
        if state.data.closed || state.data.grants.len() >= MAX_GRANTS {
            return Err(AuthorityError::Capacity);
        }
        if grant.revision != 1 {
            return Err(AuthorityError::Invalid(
                "initial grant revision must be one",
            ));
        }
        let mut next = state.data.clone();
        next.grants.push(GrantRecord {
            policy: grant,
            previous_policies: vec![],
            expansions: vec![],
            revoked_at_revision: None,
            usage: GrantUsage::default(),
            native_delegation: None,
            delegated_from: None,
            legacy_standing: None,
            host_checks: vec![],
        });
        self.commit(&mut state, next)
    }

    /// Narrowing does not reset cumulative budgets or revive old generation leases.
    pub fn narrow_grant(
        &self,
        grant: AuthorityGrant,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        validate_grant(&grant)?;
        let mut state = self.lock()?;
        validate_grant_owner(&grant, &state.data)?;
        let index = grant_index(&state.data, &grant.id)?;
        let old = &state.data.grants[index];
        if old.policy == grant && old.revoked_at_revision.is_none() {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        if state.data.closed || old.revoked_at_revision.is_some() || !narrower(&grant, &old.policy)
        {
            return Err(AuthorityError::Denied);
        }
        if old.previous_policies.len() >= 63 {
            return Err(AuthorityError::Capacity);
        }
        let mut next = state.data.clone();
        next.grants[index]
            .previous_policies
            .push(old.policy.clone());
        next.grants[index].policy = grant;
        self.commit(&mut state, next)
    }

    /// Host revocation shares the dispatch mutex. Existing claims may settle;
    /// no later claim under the revoked grant is acknowledged.
    pub fn revoke_grant(
        &self,
        grant_id: &str,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        let index = grant_index(&state.data, grant_id)?;
        if state.data.grants[index].revoked_at_revision.is_some() {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        let mut next = state.data.clone();
        next.grants[index].revoked_at_revision = Some(
            state
                .data
                .revision
                .checked_add(1)
                .ok_or(AuthorityError::Capacity)?,
        );
        // The authority-store revision records revocation. Captured policy
        // revisions remain immutable so historical intent can still resolve them.
        self.commit(&mut state, next)
    }

    /// Called by the controller after durable input and epoch admission. A new
    /// generation consumes the same logical-turn grant's activation allowance.
    pub fn register_activation(
        &self,
        activation: ActivationRef,
        grant_id: &str,
        profile: ExecutionProfile,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<ActivationLease, AuthorityError> {
        self.register_activation_inner(
            activation,
            grant_id,
            profile,
            expected_revision,
            now_ms,
            false,
        )
    }

    /// Registration for a controller that gates every provider call, including
    /// compaction and tool follow-up calls. Legacy tool-only registration cannot
    /// establish provider accounting coverage and never implies known-zero spend.
    pub fn register_provider_activation(
        &self,
        activation: ActivationRef,
        grant_id: &str,
        profile: ExecutionProfile,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<ActivationLease, AuthorityError> {
        self.register_activation_inner(
            activation,
            grant_id,
            profile,
            expected_revision,
            now_ms,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_activation_inner(
        &self,
        activation: ActivationRef,
        grant_id: &str,
        profile: ExecutionProfile,
        expected_revision: u64,
        now_ms: u64,
        provider_gated: bool,
    ) -> Result<ActivationLease, AuthorityError> {
        validate_profile(&profile)?;
        let mut state = self.lock()?;
        check_revision(&state.data, expected_revision)?;
        let (next, grant_revision) = activation_registration_candidate(
            &state.data,
            &activation,
            grant_id,
            &profile,
            now_ms,
            provider_gated,
        )?;
        self.commit(&mut state, next)?;
        Ok(ActivationLease {
            scope: self.scope.clone(),
            activation,
            grant_id: grant_id.to_owned(),
            grant_revision,
        })
    }

    /// Persist known-zero execution before failing a never-registered canonical
    /// activation. This returns no lease and does not consume activation budget.
    /// The host supplies a fresh snapshot under its canonical controller gate
    /// and verifies immutable definition/grant content. Expiry or dispatch closure
    /// cannot erase this historical no-dispatch evidence. Exact repeats remain
    /// inert after canonical failure or restart; missing coverage stays unknown.
    pub fn record_undispatched_activation(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
        profile: ExecutionProfile,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        validate_profile(&profile)?;
        let namespace = self.namespace.as_ref().ok_or(AuthorityError::Denied)?;
        if namespace.identity().journal_id() != snapshot.journal_id()
            || namespace.identity().owner() != snapshot.owner()
            || activation.session_id != snapshot.owner().session_id
            || &activation.turn_id != snapshot.turn_id()
        {
            return Err(AuthorityError::Denied);
        }
        let canonical = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == *activation)
            .ok_or(AuthorityError::Denied)?;
        let grant = canonical
            .input
            .grant
            .as_ref()
            .ok_or(AuthorityError::Denied)?;
        if canonical.input.definition.definition_id.as_str() != profile.definition {
            return Err(AuthorityError::Denied);
        }
        let mut state = self.lock()?;
        if state.data.session_id != activation.session_id
            || state.data.turn_id != activation.turn_id
            || state.data.canonical_journal_id.as_deref() != Some(snapshot.journal_id())
            || state
                .data
                .claims
                .iter()
                .any(|claim| claim.activation.activation_id == activation.activation_id)
            || state
                .data
                .provider_calls
                .iter()
                .any(|call| call.activation.activation_id == activation.activation_id)
            || snapshot
                .contract()
                .invocations()
                .iter()
                .any(|invocation| invocation.activation == *activation)
        {
            return Err(AuthorityError::Denied);
        }
        if let Some(existing) = state
            .data
            .activations
            .iter()
            .find(|item| item.activation.activation_id == activation.activation_id)
        {
            return if existing.activation == *activation
                && existing.profile == profile
                && existing.grant_id == grant.grant_id.as_str()
                && existing.grant_revision == grant.revision
                && existing.stopped
                && existing.provider_gated
                && existing.never_dispatched
            {
                Ok(())
            } else {
                Err(AuthorityError::Denied)
            };
        }
        check_revision(&state.data, expected_revision)?;
        if snapshot.contract().state() != Some(LogicalTurnState::Running)
            || canonical.state != ActivationState::Running
            || snapshot.contract().epochs().last().is_none_or(|epoch| {
                epoch.id != activation.execution_epoch_id || epoch.state != EpochState::Running
            })
        {
            return Err(AuthorityError::Denied);
        }
        if state.data.activations.len() >= MAX_ACTIVATIONS {
            return Err(AuthorityError::Capacity);
        }
        if state.data.activations.iter().any(|previous| {
            previous.activation.node_id == activation.node_id
                && (!previous.stopped || previous.activation.generation >= activation.generation)
        }) {
            return Err(AuthorityError::StaleLease);
        }
        let retained = &state.data.grants[grant_index(&state.data, grant.grant_id.as_str())?];
        let policy = retained
            .previous_policies
            .iter()
            .chain(std::iter::once(&retained.policy))
            .find(|policy| policy.revision == grant.revision)
            .ok_or(AuthorityError::Denied)?;
        if !node_allowed(policy, &activation.node_id) || !profile_allowed(&profile, policy) {
            return Err(AuthorityError::Denied);
        }
        let mut next = state.data.clone();
        next.activations.push(ActivationRecord {
            activation: activation.clone(),
            grant_id: grant.grant_id.as_str().to_owned(),
            grant_revision: grant.revision,
            profile,
            stopped: true,
            provider_gated: true,
            never_dispatched: true,
        });
        self.commit(&mut state, next)
    }

    pub fn prepare_dispatch(
        &self,
        lease: &ActivationLease,
        invocation: InvocationId,
        tool_name: String,
        reservation: DispatchReservation,
        now_ms: u64,
    ) -> Result<DispatchPreparation, AuthorityError> {
        bounded(&tool_name, 256)?;
        let state = self.lock()?;
        self.validate_lease(&state.data, lease, now_ms)?;
        validate_dispatch(&state.data, lease, &invocation, &tool_name, &reservation)?;
        Ok(DispatchPreparation {
            scope: self.scope.clone(),
            lease: lease.clone(),
            invocation,
            tool_name,
            reservation,
        })
    }

    /// The durable intent and every current authority condition are checked
    /// again while holding the same gate used by Stop and revocation. The audit
    /// remains immutably borrowed through claim, so evidence cannot race this check.
    pub fn claim_dispatch(
        &self,
        preparation: DispatchPreparation,
        receipt: &DurableIntentReceipt,
        audit: &InvocationAudit,
        now_ms: u64,
    ) -> Result<DispatchClaim, AuthorityError> {
        let mut state = self.lock()?;
        if audit
            .canonical_identity()
            .map_err(|_| AuthorityError::RecoveryRequired)?
            != self
                .namespace
                .as_ref()
                .map(OwnedExecutionNamespace::identity)
        {
            return Err(AuthorityError::Denied);
        }
        if !audit
            .is_dispatchable_receipt(receipt)
            .map_err(|_| AuthorityError::RecoveryRequired)?
        {
            return Err(AuthorityError::Denied);
        }
        let intent = receipt.intent();
        if preparation.scope != self.scope
            || intent.dispatch_scope != self.scope
            || receipt.invocation_id() != &preparation.invocation
            || receipt.activation() != &preparation.lease.activation
            || intent.tool_name != preparation.tool_name
            || intent.authority.grant_id != preparation.lease.grant_id
            || intent.authority.grant_revision != preparation.lease.grant_revision
        {
            return Err(AuthorityError::StaleLease);
        }
        self.validate_lease(&state.data, &preparation.lease, now_ms)?;
        validate_dispatch(
            &state.data,
            &preparation.lease,
            &preparation.invocation,
            &preparation.tool_name,
            &preparation.reservation,
        )?;
        let index = grant_index(&state.data, &preparation.lease.grant_id)?;
        let mut next = state.data.clone();
        let usage = &mut next.grants[index].usage;
        usage.invocations += 1;
        usage.tokens += preparation.reservation.tokens;
        usage.cost_microunits += preparation.reservation.cost_microunits;
        next.claims.push(ClaimRecord {
            audit_id: receipt.audit_id().to_owned(),
            intent: receipt.intent().clone(),
            invocation: preparation.invocation.clone(),
            activation: preparation.lease.activation.clone(),
            grant_id: preparation.lease.grant_id,
            grant_revision: preparation.lease.grant_revision,
            dispatch_scope: self.scope.clone(),
            reservation: preparation.reservation,
            settled: false,
        });
        self.commit(&mut state, next)?;
        Ok(DispatchClaim {
            invocation: preparation.invocation,
            activation: preparation.lease.activation,
        })
    }

    /// Recheck an already claimed tool immediately before a host-owned process
    /// dispatch. This is read-only: it cannot create another claim, charge twice,
    /// or recover execution permission from a reopened audit.
    pub fn validate_claimed_dispatch(
        &self,
        lease: &ActivationLease,
        intent: &InvocationIntent,
        audit: &InvocationAudit,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        let state = self.lock()?;
        self.validate_lease(&state.data, lease, now_ms)?;
        if audit
            .canonical_identity()
            .map_err(|_| AuthorityError::RecoveryRequired)?
            != self
                .namespace
                .as_ref()
                .map(OwnedExecutionNamespace::identity)
        {
            return Err(AuthorityError::Denied);
        }
        let retained = audit
            .invocation(&intent.invocation_id)
            .map_err(|_| AuthorityError::RecoveryRequired)?
            .ok_or(AuthorityError::Denied)?;
        let claim = state
            .data
            .claims
            .iter()
            .find(|claim| claim.invocation == intent.invocation_id)
            .ok_or(AuthorityError::Denied)?;
        if retained.intent != *intent
            || retained.final_evidence.is_some()
            || claim.audit_id
                != audit
                    .audit_id()
                    .map_err(|_| AuthorityError::RecoveryRequired)?
            || claim.intent != *intent
            || claim.settled
            || claim.activation != lease.activation
            || claim.grant_id != lease.grant_id
            || claim.grant_revision != lease.grant_revision
            || claim.dispatch_scope != self.scope
            || intent.dispatch_scope != self.scope
        {
            return Err(AuthorityError::Denied);
        }
        Ok(())
    }

    /// Persist exact provider intent and charge the guaranteed bound under the
    /// same mutex as Stop. Returning a claim means the remote call may already
    /// have happened; duplicate ids never return another executable capability.
    pub fn claim_provider_call(
        &self,
        lease: &ActivationLease,
        intent: ProviderCallIntent,
        now_ms: u64,
    ) -> Result<ProviderCallClaim, AuthorityError> {
        validate_provider_intent(&intent)?;
        let mut state = self.lock()?;
        self.validate_lease(&state.data, lease, now_ms)?;
        let activation = state
            .data
            .activations
            .iter()
            .find(|item| item.activation == lease.activation)
            .ok_or(AuthorityError::StaleLease)?;
        if !activation.provider_gated
            || activation.profile.provider != intent.provider
            || activation.profile.model != intent.model
            || state.data.provider_calls.iter().any(|call| {
                call.intent.call_id == intent.call_id
                    || (call.activation == lease.activation && call.outcome.is_none())
            })
        {
            return Err(AuthorityError::Denied);
        }
        if total_claims(&state.data) >= MAX_CLAIMS {
            return Err(AuthorityError::Capacity);
        }
        let index = grant_index(&state.data, &lease.grant_id)?;
        validate_reservation_budget(&state.data.grants[index], &intent.reservation)?;
        let record = ProviderCallRecord {
            intent,
            activation: lease.activation.clone(),
            grant_id: lease.grant_id.clone(),
            grant_revision: lease.grant_revision,
            dispatch_scope: self.scope.clone(),
            outcome: None,
        };
        let mut next = state.data.clone();
        let usage = &mut next.grants[index].usage;
        usage.invocations += 1;
        usage.tokens += record.intent.reservation.tokens;
        usage.cost_microunits += record.intent.reservation.cost_microunits;
        next.provider_calls.push(record.clone());
        self.commit(&mut state, next)?;
        Ok(ProviderCallClaim {
            canonical_journal_id: state.data.canonical_journal_id.clone(),
            record,
        })
    }

    /// Charge and retain an exact check intent before executing it. The current
    /// canonical store is borrowed throughout admission; a stale snapshot or a
    /// serialized reservation cannot substitute for its owned live state.
    pub fn claim_condition_run(
        &self,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        arguments: &DurableConditionArguments,
        grant: &GrantSnapshotRef,
        isolation: &str,
        now_ms: u64,
    ) -> Result<ConditionCallClaim, AuthorityError> {
        bounded(isolation, 128)?;
        let mut state = self.lock()?;
        let snapshot = self.condition_snapshot(canonical, arguments.run())?;
        self.require_condition_intent(&snapshot, arguments.run(), arguments.reference())?;
        let retained = content
            .condition_arguments(&snapshot, &arguments.run().run_id)
            .map_err(|_| AuthorityError::Denied)?
            .ok_or(AuthorityError::Denied)?;
        if arguments.journal_id() != snapshot.journal_id()
            || arguments.owner() != snapshot.owner()
            || &retained != arguments
        {
            return Err(AuthorityError::Denied);
        }
        let index = grant_index(&state.data, grant.grant_id.as_str())?;
        let policy = &state.data.grants[index].policy;
        if state.data.closed
            || state.data.grants[index].revoked_at_revision.is_some()
            || policy.revision != grant.revision
            || now_ms >= policy.expires_at_ms
            || state
                .data
                .condition_calls
                .iter()
                .any(|call| call.run.run_id == arguments.run().run_id)
        {
            return Err(AuthorityError::Denied);
        }
        let ActivationEvidenceContent::Grant {
            policy: retained_policy,
        } = content
            .resolve_activation_evidence(&grant.evidence)
            .map_err(|_| AuthorityError::Denied)?
        else {
            return Err(AuthorityError::Denied);
        };
        if retained_policy != policy {
            return Err(AuthorityError::Denied);
        }
        let definition = arguments.definition();
        if snapshot
            .contract()
            .graph()
            .and_then(|graph| {
                graph
                    .conditions
                    .iter()
                    .find(|condition| condition.condition_id == arguments.run().condition_id)
            })
            .is_none_or(|condition| {
                condition.kind
                    != (ConditionKind::RepositoryCheck {
                        definition: arguments.definition_ref().clone(),
                    })
            })
        {
            return Err(AuthorityError::Denied);
        }
        let record = ConditionCallRecord {
            run: arguments.run().clone(),
            intent: arguments.reference().clone(),
            definition: arguments.definition_ref().clone(),
            repository: arguments.repository_ref().clone(),
            isolation: isolation.to_owned(),
            timeout_ms: definition.timeout_ms,
            stdout_bytes: definition.stdout_bytes,
            stderr_bytes: definition.stderr_bytes,
            arguments: arguments.protected_arguments().clone(),
            grant: grant.clone(),
            claimed_at_ms: now_ms,
            dispatch_scope: self.scope.clone(),
            result: None,
        };
        validate_condition_record(&record)?;
        let derived_permission = if !condition_allowed(&record, policy)
            && !checks::condition_allowed(&record, &state.data.grants[index], policy)
        {
            Some(
                checks::replaced_condition_permission(
                    &snapshot,
                    &record,
                    &state.data.grants[index],
                )
                .ok_or(AuthorityError::Denied)?,
            )
        } else {
            None
        };
        if total_claims(&state.data) >= MAX_CLAIMS
            || state.data.grants[index].usage.invocations >= policy.limits.invocations
        {
            return Err(AuthorityError::Capacity);
        }
        let mut next = state.data.clone();
        if let Some(permission) = derived_permission {
            let permissions = &mut next.grants[index].host_checks;
            if permissions.len() >= MAX_COMPLETION_CONDITIONS {
                return Err(AuthorityError::Capacity);
            }
            permissions.push(permission);
        }
        next.grants[index].usage.invocations += 1;
        next.condition_calls.push(record.clone());
        self.commit(&mut state, next)?;
        Ok(ConditionCallClaim {
            journal_id: snapshot.journal_id().to_owned(),
            record,
        })
    }

    /// Recheck the one existing claim immediately before releasing the actual
    /// executor. This never recreates a claim after Stop, narrowing or restart.
    pub fn validate_condition_claim(
        &self,
        canonical: &SessionExecutionStore,
        claim: &ConditionCallClaim,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        let state = self.lock()?;
        let snapshot = self.condition_snapshot(canonical, &claim.record.run)?;
        self.require_condition_intent(&snapshot, &claim.record.run, &claim.record.intent)?;
        let stored = state
            .data
            .condition_calls
            .iter()
            .find(|call| call.run.run_id == claim.record.run.run_id)
            .ok_or(AuthorityError::Denied)?;
        let grant =
            &state.data.grants[grant_index(&state.data, claim.record.grant.grant_id.as_str())?];
        if claim.journal_id != snapshot.journal_id()
            || !same_condition_claim(stored, &claim.record)
            || stored.result.is_some()
            || stored.dispatch_scope != self.scope
            || state.data.closed
            || grant.revoked_at_revision.is_some()
            || grant.policy.revision != stored.grant.revision
            || now_ms >= grant.policy.expires_at_ms
            || (!condition_allowed(stored, &grant.policy)
                && !checks::condition_allowed(stored, grant, &grant.policy))
        {
            return Err(AuthorityError::Denied);
        }
        Ok(())
    }

    pub fn settle_condition_run(
        &self,
        claim: &ConditionCallClaim,
        result: &DurableConditionResult,
    ) -> Result<(), AuthorityError> {
        self.settle_condition_record(&claim.journal_id, &claim.record, result)
    }

    pub fn condition_call(
        &self,
        run_id: &ConditionRunId,
    ) -> Result<Option<ConditionCallRecord>, AuthorityError> {
        Ok(self
            .lock()?
            .data
            .condition_calls
            .iter()
            .find(|call| &call.run.run_id == run_id)
            .cloned())
    }

    pub fn condition_settlement_receipt(
        &self,
        run_id: &ConditionRunId,
    ) -> Result<ConditionSettlementReceipt, AuthorityError> {
        let state = self.lock()?;
        let record = state
            .data
            .condition_calls
            .iter()
            .find(|call| &call.run.run_id == run_id)
            .ok_or(AuthorityError::Denied)?
            .clone();
        Ok(ConditionSettlementReceipt {
            journal_id: state
                .data
                .canonical_journal_id
                .clone()
                .ok_or(AuthorityError::Denied)?,
            record,
        })
    }

    /// Retain independently observed late result evidence, without dispatch.
    pub fn reconcile_condition_run(
        &self,
        receipt: &ConditionSettlementReceipt,
        result: &DurableConditionResult,
    ) -> Result<(), AuthorityError> {
        self.settle_condition_record(&receipt.journal_id, &receipt.record, result)
    }

    fn settle_condition_record(
        &self,
        journal_id: &str,
        record: &ConditionCallRecord,
        result: &DurableConditionResult,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        let arguments = result.arguments();
        if state.data.canonical_journal_id.as_deref() != Some(journal_id)
            || arguments.journal_id() != journal_id
            || arguments.run() != &record.run
            || arguments.reference() != &record.intent
            || arguments.protected_arguments() != &record.arguments
            || arguments.owner().session_id != state.data.session_id
        {
            return Err(AuthorityError::Denied);
        }
        let index = state
            .data
            .condition_calls
            .iter()
            .position(|call| call.run.run_id == record.run.run_id)
            .ok_or(AuthorityError::Denied)?;
        let prior = &state.data.condition_calls[index];
        if !same_condition_claim(prior, record) {
            return Err(AuthorityError::Denied);
        }
        let result = result.protected_result();
        validate_protected_condition(result)?;
        if let Some(existing) = &prior.result {
            return if existing == result {
                Ok(())
            } else {
                Err(AuthorityError::Invalid("condition result is immutable"))
            };
        }
        let mut next = state.data.clone();
        next.condition_calls[index].result = Some(result.clone());
        self.commit(&mut state, next)
    }

    fn condition_snapshot(
        &self,
        canonical: &SessionExecutionStore,
        run: &ConditionRunRef,
    ) -> Result<DurableTurnSnapshot, AuthorityError> {
        let namespace = self.namespace.as_ref().ok_or(AuthorityError::Denied)?;
        let snapshot = canonical
            .snapshot(&run.turn_id)
            .map_err(|_| AuthorityError::Denied)?;
        if snapshot.journal_id() != namespace.identity().journal_id()
            || snapshot.owner() != namespace.identity().owner()
            || run.session_id != snapshot.owner().session_id
            || namespace.component()
                != &(ExecutionComponent::ControlAuthority {
                    turn_id: run.turn_id.clone(),
                })
        {
            return Err(AuthorityError::Denied);
        }
        Ok(snapshot)
    }

    fn require_condition_intent(
        &self,
        snapshot: &DurableTurnSnapshot,
        run: &ConditionRunRef,
        intent: &EvidenceRef,
    ) -> Result<(), AuthorityError> {
        let contract = snapshot.contract();
        let recorded = contract
            .condition_run(&run.run_id)
            .ok_or(AuthorityError::Denied)?;
        let condition = contract
            .graph()
            .and_then(|graph| {
                graph
                    .conditions
                    .iter()
                    .find(|condition| condition.condition_id == run.condition_id)
            })
            .ok_or(AuthorityError::Denied)?;
        let accepted = contract.current_accepted_activations();
        if contract.state() != Some(LogicalTurnState::Running)
            || contract
                .epochs()
                .last()
                .is_none_or(|epoch| epoch.id != run.epoch_id || epoch.state != EpochState::Running)
            || &recorded.run != run
            || &recorded.intent != intent
            || recorded.resolution.is_some()
            || contract.current_condition(&run.condition_id).is_some()
            || condition.nodes.len() != run.activations.len()
            || run.activations.iter().any(|activation| {
                !condition.nodes.contains(&activation.node_id)
                    || !accepted
                        .iter()
                        .any(|current| current.activation == *activation)
            })
        {
            return Err(AuthorityError::Denied);
        }
        Ok(())
    }

    /// Retain terminal observation before it becomes controller/actor input.
    /// Stop, revocation and restart do not discard already-incurred usage. No
    /// budget is refunded, and an exact recorded outcome cannot be replaced.
    pub fn settle_provider_call(
        &self,
        claim: &ProviderCallClaim,
        outcome: &ProviderCallOutcome,
    ) -> Result<(), AuthorityError> {
        self.settle_provider_record(&claim.canonical_journal_id, &claim.record, outcome)
    }

    pub fn provider_settlement_receipt(
        &self,
        call_id: &str,
    ) -> Result<ProviderSettlementReceipt, AuthorityError> {
        let state = self.lock()?;
        let record = state
            .data
            .provider_calls
            .iter()
            .find(|call| call.intent.call_id == call_id)
            .ok_or(AuthorityError::Denied)?
            .clone();
        Ok(ProviderSettlementReceipt {
            canonical_journal_id: state.data.canonical_journal_id.clone(),
            record,
        })
    }

    /// Recovery must supply independently observed terminal evidence; the
    /// retained claim itself proves neither zero token usage nor termination.
    pub fn reconcile_provider_call(
        &self,
        receipt: &ProviderSettlementReceipt,
        outcome: &ProviderCallOutcome,
    ) -> Result<(), AuthorityError> {
        self.settle_provider_record(&receipt.canonical_journal_id, &receipt.record, outcome)
    }

    fn settle_provider_record(
        &self,
        canonical_journal_id: &Option<String>,
        record: &ProviderCallRecord,
        outcome: &ProviderCallOutcome,
    ) -> Result<(), AuthorityError> {
        validate_provider_outcome(outcome)?;
        let mut state = self.lock()?;
        let index = state
            .data
            .provider_calls
            .iter()
            .position(|call| call.intent.call_id == record.intent.call_id)
            .ok_or(AuthorityError::Denied)?;
        let existing = &state.data.provider_calls[index];
        if &state.data.canonical_journal_id != canonical_journal_id
            || !same_provider_claim(existing, record)
        {
            return Err(AuthorityError::Denied);
        }
        if let Some(prior) = &existing.outcome {
            return if prior == outcome {
                Ok(())
            } else {
                Err(AuthorityError::Invalid(
                    "provider terminal observation is immutable",
                ))
            };
        }
        let mut next = state.data.clone();
        next.provider_calls[index].outcome = Some(outcome.clone());
        self.commit(&mut state, next)
    }

    pub fn provider_call(
        &self,
        call_id: &str,
    ) -> Result<Option<ProviderCallRecord>, AuthorityError> {
        Ok(self
            .lock()?
            .data
            .provider_calls
            .iter()
            .find(|call| call.intent.call_id == call_id)
            .cloned())
    }

    /// The largest tokens and cost any provider call of this activation
    /// reserved: what its next call is expected to reserve. `None` before its
    /// first call.
    pub fn largest_provider_reservation(
        &self,
        activation: &ActivationRef,
    ) -> Result<Option<DispatchReservation>, AuthorityError> {
        let state = self.lock()?;
        Ok(state
            .data
            .provider_calls
            .iter()
            .filter(|call| call.activation == *activation)
            .map(|call| &call.intent.reservation)
            .fold(None, |largest: Option<DispatchReservation>, reservation| {
                let (tokens, cost_microunits) =
                    largest.map_or((0, 0), |largest| (largest.tokens, largest.cost_microunits));
                Some(DispatchReservation {
                    tokens: tokens.max(reservation.tokens),
                    cost_microunits: cost_microunits.max(reservation.cost_microunits),
                })
            }))
    }

    /// Exact measured subtotal across this activation's claimed calls. An
    /// unregistered or legacy tool-only activation has no coverage proof and
    /// errors; a provider-gated activation with no claims is known zero.
    pub fn provider_usage(
        &self,
        activation: &ActivationRef,
    ) -> Result<ProviderUsage, AuthorityError> {
        provider_usage_for(&self.lock()?.data, std::slice::from_ref(activation))
    }

    /// Host exact Stop. This acknowledges only the durable closed dispatch gate,
    /// not cancellation settlement or rollback of previously claimed effects.
    pub fn stop_activation(
        &self,
        target: &ActivationRef,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        self.stop_locked(&mut state, target, expected_revision)
    }

    /// A supervisor can stop only exact registered descendants, never itself,
    /// ancestors, or another branch. Revoked/stale generation leases cannot act.
    pub fn stop_descendant(
        &self,
        source: &ActivationLease,
        target: &ActivationRef,
        expected_revision: u64,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        self.validate_lease(&state.data, source, now_ms)?;
        let grant = &state.data.grants[grant_index(&state.data, &source.grant_id)?].policy;
        if source.activation.node_id != grant.holder
            || !grant.permits_operation(DelegatedOperation::StopActivation, &target.node_id)
        {
            return Err(AuthorityError::Denied);
        }
        self.stop_locked(&mut state, target, expected_revision)
    }

    pub fn close_dispatch(&self, expected_revision: u64) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        if state.data.closed && state.data.lifecycle_suspension.is_none() {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        let mut next = state.data.clone();
        next.closed = true;
        next.lifecycle_suspension = None;
        self.commit(&mut state, next)
    }

    /// Fence every dispatch during daemon/Session lifecycle cleanup, preserving
    /// explicit future continuation without restoring any old generation lease.
    pub fn suspend_dispatch(&self, expected_revision: u64) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        if state.data.closed {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        let mut next = state.data.clone();
        next.closed = true;
        next.lifecycle_suspension = Some(self.scope.clone());
        self.commit(&mut state, next)
    }

    /// Reconstruct only an explicitly recorded lifecycle suspension. Historical
    /// closure without this marker is never guessed to have been a shutdown.
    /// This mints no lease and does not replay any provider/tool/condition claim.
    pub fn recover_lifecycle_suspension(
        &self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        let Some(scope) = &state.data.lifecycle_suspension else {
            return Ok(());
        };
        let namespace = self.namespace.as_ref().ok_or(AuthorityError::Denied)?;
        if scope == &self.scope
            || namespace.identity().journal_id() != snapshot.journal_id()
            || namespace.identity().owner() != snapshot.owner()
            || state.data.session_id != snapshot.owner().session_id
            || &state.data.turn_id != snapshot.turn_id()
            || state
                .data
                .activations
                .iter()
                .any(|activation| !activation.stopped)
        {
            return Err(AuthorityError::Denied);
        }
        if snapshot.contract().state() != Some(LogicalTurnState::NeedsAttention)
            || snapshot.contract().stop_requested().is_some()
        {
            return Ok(());
        }
        let mut next = state.data.clone();
        next.closed = false;
        next.lifecycle_suspension = None;
        self.commit(&mut state, next)
    }

    /// Late audit evidence can settle a claimed effect after Stop/turn closure.
    /// Reservations are retained conservatively; settlement never refunds budget
    /// or reopens dispatch. A failed tool still has a recorded external outcome.
    pub fn settle_dispatch(
        &self,
        invocation: &InvocationId,
        audit: &InvocationAudit,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        let mut state = self.lock()?;
        if audit
            .canonical_identity()
            .map_err(|_| AuthorityError::RecoveryRequired)?
            != self
                .namespace
                .as_ref()
                .map(OwnedExecutionNamespace::identity)
        {
            return Err(AuthorityError::Denied);
        }
        let index = state
            .data
            .claims
            .iter()
            .position(|c| &c.invocation == invocation)
            .ok_or(AuthorityError::Denied)?;
        let claim = &state.data.claims[index];
        if audit
            .audit_id()
            .map_err(|_| AuthorityError::RecoveryRequired)?
            != claim.audit_id
        {
            return Err(AuthorityError::Denied);
        }
        let record = audit
            .invocation(invocation)
            .map_err(|_| AuthorityError::RecoveryRequired)?
            .ok_or(AuthorityError::Denied)?;
        if record.intent != claim.intent
            || record.intent.activation != claim.activation
            || record.intent.dispatch_scope != claim.dispatch_scope
            || record.intent.authority.grant_id != claim.grant_id
            || record.intent.authority.grant_revision != claim.grant_revision
            || record.final_evidence.is_none()
        {
            return Err(AuthorityError::Denied);
        }
        if claim.settled {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        let mut next = state.data.clone();
        next.claims[index].settled = true;
        self.commit(&mut state, next)
    }

    pub fn revision(&self) -> Result<u64, AuthorityError> {
        Ok(self.lock()?.data.revision)
    }

    /// Current retained policy for exact host-side input validation. Reading a
    /// policy does not bypass revocation, expiry, or the live dispatch gate.
    pub fn grant_policy(&self, grant_id: &str) -> Result<AuthorityGrant, AuthorityError> {
        let state = self.lock()?;
        let index = grant_index(&state.data, grant_id)?;
        Ok(state.data.grants[index].policy.clone())
    }
    pub fn grant_status(&self, grant_id: &str) -> Result<AuthorityGrantStatus, AuthorityError> {
        let state = self.lock()?;
        let record = &state.data.grants[grant_index(&state.data, grant_id)?];
        Ok(AuthorityGrantStatus {
            policy: record.policy.clone(),
            authority_revision: state.data.revision,
            revoked_at_revision: record.revoked_at_revision,
        })
    }

    /// Read-only admission preflight. This does not reserve budget or mint a
    /// lease; dispatch registration must repeat these checks under its write gate.
    /// Checks the larger ordinary-registration encoding, which also covers a
    /// provider-gated registration, including reserved settlement/revocation space.
    pub fn validate_activation_grant(
        &self,
        activation: &ActivationRef,
        grant_id: &str,
        profile: &ExecutionProfile,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        validate_profile(profile)?;
        let state = self.lock()?;
        let (next, _) = activation_registration_candidate(
            &state.data,
            activation,
            grant_id,
            profile,
            now_ms,
            false,
        )?;
        self.prepare_commit(&state.data, next).map(|_| ())
    }

    /// The exact profile a registered activation was admitted with, read from
    /// its durable record. Enforcement reads this rather than any live copy.
    pub fn activation_profile(
        &self,
        activation: &ActivationRef,
    ) -> Result<ExecutionProfile, AuthorityError> {
        let state = self.lock()?;
        state
            .data
            .activations
            .iter()
            .find(|record| record.activation == *activation)
            .map(|record| record.profile.clone())
            .ok_or(AuthorityError::Denied)
    }

    pub fn usage(&self, grant_id: &str) -> Result<GrantUsage, AuthorityError> {
        let state = self.lock()?;
        Ok(state.data.grants[grant_index(&state.data, grant_id)?]
            .usage
            .clone())
    }

    /// Capture attributable source identity through the same live gate used by
    /// dispatch. This is not authorization for the requested control operation:
    /// the controller must check its exact target/scope again at acceptance and
    /// application, because this evidence can outlive a Stop or grant revocation.
    pub fn attest_control_source(
        &self,
        lease: &ActivationLease,
        now_ms: u64,
    ) -> Result<TrustedCommandSource, AuthorityError> {
        let state = self.lock()?;
        self.validate_lease(&state.data, lease, now_ms)?;
        let grant = &state.data.grants[grant_index(&state.data, &lease.grant_id)?].policy;
        // The serialized policy itself is retained in current/previous policies;
        // issuer_evidence alone identifies its approval, not this exact snapshot.
        let digest = Sha256::digest(serde_json::to_vec(grant)?);
        let evidence = EvidenceRef::new(format!("grant-sha256:{digest:x}"))
            .map_err(|_| AuthorityError::Invalid("grant snapshot identity"))?;
        Ok(TrustedCommandSource::agent(
            lease.activation.clone(),
            lease.grant_id.clone(),
            lease.grant_revision,
            evidence,
            self.scope.clone(),
        ))
    }

    fn stop_locked(
        &self,
        state: &mut LiveState,
        target: &ActivationRef,
        expected_revision: u64,
    ) -> Result<(), AuthorityError> {
        if state.data.activations.iter().any(|a| {
            a.activation.node_id == target.node_id && a.activation.generation > target.generation
        }) {
            return Err(AuthorityError::StaleLease);
        }
        let index = state
            .data
            .activations
            .iter()
            .position(|a| &a.activation == target)
            .ok_or(AuthorityError::StaleLease)?;
        if state.data.activations[index].stopped {
            return Ok(());
        }
        check_revision(&state.data, expected_revision)?;
        let mut next = state.data.clone();
        next.activations[index].stopped = true;
        self.commit(state, next)
    }

    fn validate_lease(
        &self,
        data: &AuthorityData,
        lease: &ActivationLease,
        now_ms: u64,
    ) -> Result<(), AuthorityError> {
        if data.closed
            || lease.scope != self.scope
            || data.provider_calls.iter().any(provider_bound_violation)
        {
            return Err(AuthorityError::StaleLease);
        }
        let activation = data
            .activations
            .iter()
            .find(|a| {
                a.activation == lease.activation
                    && a.grant_id == lease.grant_id
                    && expansion::recorded_revision(
                        data,
                        &lease.grant_id,
                        a.grant_revision,
                        lease.grant_revision,
                    )
                    && !a.stopped
                    && !a.never_dispatched
            })
            .ok_or(AuthorityError::StaleLease)?;
        let grant = &data.grants[grant_index(data, &lease.grant_id)?];
        if (grant.policy.delegation.is_some() && grant.native_delegation.is_none())
            || !delegation::delegated_ancestors_live(data, grant, now_ms)
            || grant.revoked_at_revision.is_some()
            || grant.policy.revision != lease.grant_revision
            || now_ms >= grant.policy.expires_at_ms
            || !node_allowed(&grant.policy, &lease.activation.node_id)
            || !profile_allowed(&activation.profile, &grant.policy)
        {
            return Err(AuthorityError::Denied);
        }
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, LiveState>, AuthorityError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AuthorityError::RecoveryRequired)?;
        if state.poisoned {
            return Err(AuthorityError::RecoveryRequired);
        }
        if let Some(namespace) = &self.namespace {
            namespace.verify_ambient_identity()?;
        }
        self.dir.verify_ambient_identity()?;
        Ok(state)
    }

    fn prepare_commit(
        &self,
        current: &AuthorityData,
        mut next: AuthorityData,
    ) -> Result<(AuthorityData, Vec<u8>), AuthorityError> {
        next.revision = current
            .revision
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        let bytes = serde_json::to_vec(&next)?;
        if reserved_size(&next, bytes.len()) > self.capacity_bytes {
            return Err(AuthorityError::Capacity);
        }
        Ok((next, bytes))
    }

    fn commit(&self, state: &mut LiveState, next: AuthorityData) -> Result<(), AuthorityError> {
        let (next, bytes) = self.prepare_commit(&state.data, next)?;
        if let Err(e) = self.dir.atomic_write(FILE, &bytes) {
            // The rename may have published. Never dispatch or overwrite from
            // the older in-memory state after an uncertain write.
            state.poisoned = true;
            return Err(e.into());
        }
        state.data = next;
        Ok(())
    }
}

fn check_revision(data: &AuthorityData, expected: u64) -> Result<(), AuthorityError> {
    if data.revision != expected {
        return Err(AuthorityError::Invalid("stale authority revision"));
    }
    Ok(())
}

fn reserved_size(data: &AuthorityData, bytes: usize) -> usize {
    // Future revocation must fit even when normal admission exhausts storage.
    // Reserve a maximum-width revision for every grant and the store itself.
    bytes
        // Lifecycle fencing must remain writable even after normal admission.
        .saturating_add(if data.lifecycle_suspension.is_none() {
            128
        } else {
            0
        })
        .saturating_add(
            data.provider_calls
                .iter()
                .filter(|call| call.outcome.is_none())
                .count()
                .saturating_mul(PROVIDER_OUTCOME_RESERVE),
        )
        .saturating_add(
            data.condition_calls
                .iter()
                .filter(|call| call.result.is_none())
                .count()
                .saturating_mul(CONDITION_OUTCOME_RESERVE),
        )
        + 20
        + 20 * data
            .grants
            .iter()
            .filter(|g| g.revoked_at_revision.is_none())
            .count()
}

fn validate_activation_registration(
    data: &AuthorityData,
    activation: &ActivationRef,
    grant_id: &str,
    profile: &ExecutionProfile,
    now_ms: u64,
) -> Result<usize, AuthorityError> {
    if data.closed
        || activation.generation == 0
        || activation.session_id != data.session_id
        || activation.turn_id != data.turn_id
    {
        return Err(AuthorityError::Denied);
    }
    if data.activations.len() >= MAX_ACTIVATIONS {
        return Err(AuthorityError::Capacity);
    }
    if data.activations.iter().any(|a| {
        a.activation.activation_id == activation.activation_id
            || (a.activation.node_id == activation.node_id
                && (!a.stopped || a.activation.generation >= activation.generation))
    }) {
        return Err(AuthorityError::StaleLease);
    }
    if data.claims.iter().any(|c| {
        !c.settled
            && (c.activation.node_id == activation.node_id
                || c.activation.execution_epoch_id != activation.execution_epoch_id)
    }) || data.provider_calls.iter().any(|call| {
        provider_bound_violation(call)
            || (call.outcome.is_none()
                && (call.activation.node_id == activation.node_id
                    || call.activation.execution_epoch_id != activation.execution_epoch_id))
    }) {
        return Err(AuthorityError::Denied);
    }
    let index = grant_index(data, grant_id)?;
    let grant = &data.grants[index];
    // Contract-only policy cannot enter the legacy admission path. The future
    // canonical admission must resolve scope/templates/resources/checks first.
    if (grant.policy.delegation.is_some() && grant.native_delegation.is_none())
        || !delegation::delegated_ancestors_live(data, grant, now_ms)
        || grant.revoked_at_revision.is_some()
        || now_ms >= grant.policy.expires_at_ms
        || !node_allowed(&grant.policy, &activation.node_id)
        || !profile_allowed(profile, &grant.policy)
    {
        return Err(AuthorityError::Denied);
    }
    if grant.usage.activations >= grant.policy.limits.activations {
        return Err(AuthorityError::Capacity);
    }
    Ok(index)
}

fn activation_registration_candidate(
    data: &AuthorityData,
    activation: &ActivationRef,
    grant_id: &str,
    profile: &ExecutionProfile,
    now_ms: u64,
    provider_gated: bool,
) -> Result<(AuthorityData, u64), AuthorityError> {
    let index = validate_activation_registration(data, activation, grant_id, profile, now_ms)?;
    let grant_revision = data.grants[index].policy.revision;
    let mut next = data.clone();
    next.grants[index].usage.activations += 1;
    next.activations.push(ActivationRecord {
        activation: activation.clone(),
        grant_id: grant_id.to_owned(),
        grant_revision,
        profile: profile.clone(),
        stopped: false,
        provider_gated,
        never_dispatched: false,
    });
    Ok((next, grant_revision))
}

fn grant_index(data: &AuthorityData, id: &str) -> Result<usize, AuthorityError> {
    data.grants
        .iter()
        .position(|g| g.policy.id == id)
        .ok_or(AuthorityError::Denied)
}

fn node_allowed(grant: &AuthorityGrant, node: &TurnNodeId) -> bool {
    &grant.holder == node || grant.descendants.contains(node)
}

fn profile_subset(profile: &ExecutionProfile, limit: &ExecutionProfile) -> bool {
    profile.definition == limit.definition
        && profile.provider == limit.provider
        && profile.model == limit.model
        && profile.isolation == limit.isolation
        && profile.tools.iter().all(|t| limit.tools.contains(t))
        && crate::path_scope::write_scope_within(
            profile.write_scope.as_deref(),
            limit.write_scope.as_deref(),
        )
}

fn profile_allowed(profile: &ExecutionProfile, grant: &AuthorityGrant) -> bool {
    grant.profiles.iter().any(|p| profile_subset(profile, p))
}

fn narrower(new: &AuthorityGrant, old: &AuthorityGrant) -> bool {
    old.revision.checked_add(1) == Some(new.revision)
        && new.id == old.id
        && new.holder == old.holder
        && new.issuer_evidence == old.issuer_evidence
        && new.expires_at_ms <= old.expires_at_ms
        && (!new.allow_stop_descendants || old.allow_stop_descendants)
        && delegation_narrower(new.delegation.as_deref(), old.delegation.as_deref())
        && new.descendants.iter().all(|n| old.descendants.contains(n))
        && new.profiles.iter().all(|p| profile_allowed(p, old))
        && new.conditions.iter().all(|permission| {
            old.conditions
                .iter()
                .any(|limit| condition_permission_subset(permission, limit))
        })
        && new.limits.activations <= old.limits.activations
        && new.limits.invocations <= old.limits.invocations
        && new.limits.tokens <= old.limits.tokens
        && new.limits.cost_microunits <= old.limits.cost_microunits
}

fn delegation_narrower(new: Option<&DelegationPolicy>, old: Option<&DelegationPolicy>) -> bool {
    match (new, old) {
        (None, None) => true,
        (Some(new), Some(old)) => {
            new.schema_version == old.schema_version
                && new.scope == old.scope
                && new.resource_policy == old.resource_policy
                && new.replay_policy == old.replay_policy
                && new.graph_limits.max_nodes <= old.graph_limits.max_nodes
                && new.graph_limits.max_edges <= old.graph_limits.max_edges
                && new.operations.iter().all(|p| {
                    old.operations.iter().any(|limit| {
                        p.operation == limit.operation
                            && delegated_scope_narrower(&p.targets, &limit.targets)
                    })
                })
                && new.templates.iter().all(|t| old.templates.contains(t))
                && new
                    .machine_blockers
                    .iter()
                    .all(|b| old.machine_blockers.contains(b))
                && old
                    .required_conditions
                    .iter()
                    .all(|c| new.required_conditions.contains(c))
                && old
                    .completion_criteria
                    .iter()
                    .all(|c| new.completion_criteria.contains(c))
        }
        // Replacing legacy policy is not narrowing. A host must explicitly issue
        // a separate approved grant; old captured snapshots and usage stay intact.
        _ => false,
    }
}

fn delegated_scope_narrower(new: &DelegatedTargetScope, old: &DelegatedTargetScope) -> bool {
    match (new, old) {
        (
            DelegatedTargetScope::Nodes { nodes },
            DelegatedTargetScope::Nodes { nodes: previous },
        ) => nodes.iter().all(|node| previous.contains(node)),
        (
            DelegatedTargetScope::Subtree {
                root,
                include_future_descendants,
            },
            DelegatedTargetScope::Subtree {
                root: previous,
                include_future_descendants: had_future,
            },
        ) => root == previous && (!*include_future_descendants || *had_future),
        // Conversion between scopes needs canonical topology proof; this journal
        // must never guess that an arbitrary node belongs to a subtree.
        _ => false,
    }
}

fn validate_grant_owner(
    grant: &AuthorityGrant,
    data: &AuthorityData,
) -> Result<(), AuthorityError> {
    if grant.delegation.as_ref().is_some_and(|policy| {
        policy.scope.session_id != data.session_id || policy.scope.turn_id != data.turn_id
    }) {
        return Err(AuthorityError::Denied);
    }
    Ok(())
}

fn total_claims(data: &AuthorityData) -> usize {
    data.claims
        .len()
        .saturating_add(data.provider_calls.len())
        .saturating_add(data.condition_calls.len())
}

fn condition_permission_subset(
    permission: &ConditionPermission,
    limit: &ConditionPermission,
) -> bool {
    permission.kind == limit.kind
        && permission.repository == limit.repository
        && permission.isolation == limit.isolation
        && permission
            .nodes
            .iter()
            .all(|node| limit.nodes.contains(node))
        && permission.max_timeout_ms <= limit.max_timeout_ms
        && permission.max_stdout_bytes <= limit.max_stdout_bytes
        && permission.max_stderr_bytes <= limit.max_stderr_bytes
}

fn condition_allowed(record: &ConditionCallRecord, grant: &AuthorityGrant) -> bool {
    // No condition executor may bypass the new resource-policy proof join.
    grant.delegation.is_none()
        && grant.conditions.iter().any(|permission| {
            permission.kind
                == (ConditionKind::RepositoryCheck {
                    definition: record.definition.clone(),
                })
                && permission.repository == record.repository
                && permission.isolation == record.isolation
                && record.run.activations.iter().all(|activation| {
                    node_allowed(grant, &activation.node_id)
                        && permission.nodes.contains(&activation.node_id)
                })
                && record.timeout_ms <= permission.max_timeout_ms
                && record.stdout_bytes <= permission.max_stdout_bytes
                && record.stderr_bytes <= permission.max_stderr_bytes
        })
}

/// A check a standing-work turn ran under its carried permissions. Only a
/// stored claim is judged this way; no new claim can use them.
fn legacy_standing_condition_allowed(
    record: &ConditionCallRecord,
    grant: &GrantRecord,
    policy: &AuthorityGrant,
) -> bool {
    checks::has_shell(policy)
        && grant.legacy_standing.as_ref().is_some_and(|carry| {
            carry
                .conditions
                .iter()
                .any(|permission| checks::permission_covers(permission, record))
        })
}

/// The usage a legacy standing carry adds to its grant, once it is checked
/// against the grant's original approval.
fn validate_legacy_standing(
    data: &AuthorityData,
    grant: &GrantRecord,
) -> Result<GrantUsage, AuthorityError> {
    let Some(carry) = &grant.legacy_standing else {
        return Ok(GrantUsage::default());
    };
    let original = grant.previous_policies.first().unwrap_or(&grant.policy);
    if grant.delegated_from.is_some()
        || data.canonical_journal_id.is_none()
        || carry.consumed_before.activations > original.limits.activations
        || carry.consumed_before.invocations > original.limits.invocations
        || carry.consumed_before.tokens > original.limits.tokens
        || carry.consumed_before.cost_microunits > original.limits.cost_microunits
    {
        return Err(AuthorityError::Invalid(
            "standing grant carry differs from approved allowance",
        ));
    }
    checks::validate_permissions(
        &carry.conditions,
        original,
        "invalid standing condition permission",
    )?;
    Ok(carry.consumed_before.clone())
}

fn same_condition_claim(left: &ConditionCallRecord, right: &ConditionCallRecord) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.result = None;
    right.result = None;
    left == right
}

fn validate_protected_condition(value: &ProtectedArguments) -> Result<(), AuthorityError> {
    if value.sha256.len() != 64
        || !value.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.byte_len == 0
    {
        return Err(AuthorityError::Invalid(
            "invalid protected condition evidence",
        ));
    }
    Ok(())
}

fn validate_condition_record(record: &ConditionCallRecord) -> Result<(), AuthorityError> {
    bounded(&record.isolation, 128)?;
    bounded(&record.dispatch_scope, 128)?;
    validate_protected_condition(&record.arguments)?;
    if let Some(result) = &record.result {
        validate_protected_condition(result)?;
    }
    let mut nodes = HashSet::new();
    if record.run.activations.is_empty()
        || record.run.activations.len() > 128
        || record.timeout_ms == 0
        || record.grant.revision == 0
        || record.arguments.evidence_ref != record.intent
        || record.run.activations.iter().any(|activation| {
            activation.session_id != record.run.session_id
                || activation.turn_id != record.run.turn_id
                || activation.generation == 0
                || !nodes.insert(&activation.node_id)
        })
    {
        return Err(AuthorityError::Invalid("invalid condition claim"));
    }
    Ok(())
}

fn validate_dispatch(
    data: &AuthorityData,
    lease: &ActivationLease,
    invocation: &InvocationId,
    tool: &str,
    reservation: &DispatchReservation,
) -> Result<(), AuthorityError> {
    if total_claims(data) >= MAX_CLAIMS {
        return Err(AuthorityError::Capacity);
    }
    if data.claims.iter().any(|c| &c.invocation == invocation) {
        return Err(AuthorityError::Denied);
    }
    let activation = data
        .activations
        .iter()
        .find(|a| a.activation == lease.activation)
        .ok_or(AuthorityError::StaleLease)?;
    let grant = &data.grants[grant_index(data, &lease.grant_id)?];
    // Delegation admits a helper through the same child-grant reservation as
    // any AddAgent command, so only the holder of a policy that can add an
    // Agent from at least one template may reach the port.
    let delegate_port = tool == DELEGATE_TOOL
        && activation.activation.node_id == grant.policy.holder
        && grant.policy.delegation.as_deref().is_some_and(|policy| {
            !policy.templates.is_empty()
                && policy
                    .operations
                    .iter()
                    .any(|permission| permission.operation == DelegatedOperation::AddAgent)
        });
    // Workspace knowledge only reads the bound workspace or stages a private
    // proposal. Publication is a separate accepted-closure/human operation. It
    // consumes the same live lease and invocation budget as other host ports.
    let knowledge_port = tool == KNOWLEDGE_TOOL
        && (activation.activation.node_id == grant.policy.holder
            || grant
                .policy
                .descendants
                .contains(&activation.activation.node_id));
    let capture_port = tool == REPOSITORY_CAPTURE_PORT
        && activation
            .profile
            .write_scope
            .as_ref()
            .is_some_and(|scope| !scope.is_empty());
    if !delegate_port
        && !knowledge_port
        && !capture_port
        && !activation.profile.tools.iter().any(|t| t == tool)
    {
        return Err(AuthorityError::Denied);
    }
    // A read-only activation may list the file-writing tools in its captured
    // definition, but it can never claim one.
    if matches!(tool, "write_file" | "edit_file")
        && activation
            .profile
            .write_scope
            .as_ref()
            .is_some_and(Vec::is_empty)
    {
        return Err(AuthorityError::Denied);
    }
    if grant.usage.invocations >= grant.policy.limits.invocations
        || grant
            .usage
            .tokens
            .checked_add(reservation.tokens)
            .is_none_or(|n| n > grant.policy.limits.tokens)
        || grant
            .usage
            .cost_microunits
            .checked_add(reservation.cost_microunits)
            .is_none_or(|n| n > grant.policy.limits.cost_microunits)
    {
        return Err(AuthorityError::Capacity);
    }
    Ok(())
}

fn validate_reservation_budget(
    grant: &GrantRecord,
    reservation: &DispatchReservation,
) -> Result<(), AuthorityError> {
    if grant.usage.invocations >= grant.policy.limits.invocations
        || grant
            .usage
            .tokens
            .checked_add(reservation.tokens)
            .is_none_or(|n| n > grant.policy.limits.tokens)
        || grant
            .usage
            .cost_microunits
            .checked_add(reservation.cost_microunits)
            .is_none_or(|n| n > grant.policy.limits.cost_microunits)
    {
        return Err(AuthorityError::Capacity);
    }
    Ok(())
}

fn validate_provider_intent(intent: &ProviderCallIntent) -> Result<(), AuthorityError> {
    bounded(&intent.call_id, 128)?;
    bounded(&intent.provider, 256)?;
    bounded(&intent.model, 256)?;
    if intent.request_sha256.len() != 64
        || !intent
            .request_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || intent.request_bytes == 0
        || intent.request_bytes > MAX_PROVIDER_REQUEST_BYTES
        || intent.max_response_bytes == 0
        || intent.max_response_bytes > MAX_PROVIDER_RESPONSE_BYTES
        || intent.reservation.tokens == 0
    {
        return Err(AuthorityError::Invalid(
            "invalid provider request identity or hard bound",
        ));
    }
    Ok(())
}

fn validate_provider_outcome(outcome: &ProviderCallOutcome) -> Result<(), AuthorityError> {
    if outcome.cost_known && outcome.cost_microunits.is_none() {
        return Err(AuthorityError::Invalid(
            "known provider cost has no observed value",
        ));
    }
    Ok(())
}

fn same_provider_claim(left: &ProviderCallRecord, right: &ProviderCallRecord) -> bool {
    left.intent == right.intent
        && left.activation == right.activation
        && left.grant_id == right.grant_id
        && left.grant_revision == right.grant_revision
        && left.dispatch_scope == right.dispatch_scope
}

fn provider_bound_violation(record: &ProviderCallRecord) -> bool {
    record.outcome.as_ref().is_some_and(|outcome| {
        let usage = &outcome.usage.usage;
        usage
            .input_tokens
            .checked_add(usage.output_tokens)
            .and_then(|total| total.checked_add(usage.reasoning_tokens.unwrap_or(0)))
            .and_then(|total| u64::try_from(total).ok())
            .is_none_or(|total| total > record.intent.reservation.tokens)
            || outcome
                .cost_microunits
                .is_some_and(|cost| cost > record.intent.reservation.cost_microunits)
    })
}

// Shared with the existing-only historical reader. Callers must first verify
// the owned journal/turn and validate_data; an absent authority is never zero.
fn provider_usage_for(
    data: &AuthorityData,
    activations: &[ActivationRef],
) -> Result<ProviderUsage, AuthorityError> {
    let mut seen = HashSet::new();
    if activations.len() > MAX_ACTIVATIONS {
        return Err(AuthorityError::Capacity);
    }
    for activation in activations {
        if !seen.insert(&activation.activation_id)
            || !data
                .activations
                .iter()
                .any(|record| record.activation == *activation && record.provider_gated)
        {
            return Err(AuthorityError::Invalid(
                "provider accounting coverage is absent or duplicated",
            ));
        }
    }
    let mut result = ProviderUsage {
        tokens: MeasuredTokenUsage::known(TokenUsageStats::default()),
        cost_microunits: 0,
        cost_known: true,
        calls: 0,
        unsettled_calls: 0,
    };
    for call in data
        .provider_calls
        .iter()
        .filter(|call| activations.contains(&call.activation))
    {
        result.calls = result
            .calls
            .checked_add(1)
            .ok_or(AuthorityError::Capacity)?;
        let Some(outcome) = &call.outcome else {
            result.unsettled_calls += 1;
            result.tokens.complete = false;
            // A valid enforced zero ceiling proves zero provider inference API
            // charge, not zero computation or a settled execution. Preserve the
            // unresolved call and token uncertainty across process loss.
            result.cost_known &= call.intent.reservation.cost_microunits == 0;
            continue;
        };
        let subtotal = &mut result.tokens.usage;
        let observed = &outcome.usage.usage;
        subtotal.input_tokens = subtotal
            .input_tokens
            .checked_add(observed.input_tokens)
            .ok_or(AuthorityError::Capacity)?;
        subtotal.output_tokens = subtotal
            .output_tokens
            .checked_add(observed.output_tokens)
            .ok_or(AuthorityError::Capacity)?;
        if let Some(reasoning) = observed.reasoning_tokens {
            subtotal.reasoning_tokens = Some(
                subtotal
                    .reasoning_tokens
                    .unwrap_or(0)
                    .checked_add(reasoning)
                    .ok_or(AuthorityError::Capacity)?,
            );
        }
        result.tokens.complete &= outcome.usage.complete;
        result.cost_microunits = result
            .cost_microunits
            .checked_add(outcome.cost_microunits.unwrap_or(0))
            .ok_or(AuthorityError::Capacity)?;
        result.cost_known &= outcome.cost_known
            || (call.intent.reservation.cost_microunits == 0
                && outcome.cost_microunits.unwrap_or(0) == 0);
    }
    Ok(result)
}

fn bounded(value: &str, max: usize) -> Result<(), AuthorityError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(AuthorityError::Invalid("invalid bounded text"));
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn validate_profile(profile: &ExecutionProfile) -> Result<(), AuthorityError> {
    for value in [
        &profile.definition,
        &profile.provider,
        &profile.model,
        &profile.isolation,
    ] {
        bounded(value, 256)?;
    }
    if profile.tools.len() > 64 {
        return Err(AuthorityError::Capacity);
    }
    let mut tools = HashSet::new();
    for tool in &profile.tools {
        bounded(tool, 256)?;
        if !tools.insert(tool) {
            return Err(AuthorityError::Invalid("duplicate tool"));
        }
    }
    if let Some(scope) = &profile.write_scope {
        crate::path_scope::validate_write_scope(scope)
            .map_err(|_| AuthorityError::Invalid("invalid write scope"))?;
    }
    Ok(())
}

fn validate_grant(grant: &AuthorityGrant) -> Result<(), AuthorityError> {
    GrantId::new(grant.id.clone())
        .map_err(|_| AuthorityError::Invalid("invalid grant identity"))?;
    if grant.revision == 0
        || grant.expires_at_ms == 0
        || grant.descendants.len() > 128
        || (grant.revision == 1
            && grant.profiles.is_empty()
            && grant.conditions.is_empty()
            && grant.delegation.is_none())
        || grant.profiles.len() > 32
        || grant.conditions.len() > 128
    {
        return Err(AuthorityError::Invalid("invalid grant bounds"));
    }
    let mut nodes = HashSet::new();
    for node in &grant.descendants {
        if node == &grant.holder || !nodes.insert(node) {
            return Err(AuthorityError::Invalid("invalid descendant set"));
        }
    }
    for profile in &grant.profiles {
        validate_profile(profile)?;
    }
    for (index, permission) in grant.conditions.iter().enumerate() {
        bounded(&permission.isolation, 128)?;
        let mut nodes = HashSet::new();
        if permission.nodes.is_empty()
            || permission.nodes.len() > 128
            || permission.max_timeout_ms == 0
            || permission
                .nodes
                .iter()
                .any(|node| !node_allowed(grant, node) || !nodes.insert(node))
            || grant.conditions[..index].contains(permission)
        {
            return Err(AuthorityError::Invalid("invalid condition permission"));
        }
    }
    if let Some(policy) = &grant.delegation {
        validate_delegation(grant, policy)?;
    }
    if serde_json::to_vec(grant)?.len() > 64 * 1024 {
        return Err(AuthorityError::Capacity);
    }
    Ok(())
}

fn validate_delegation(
    grant: &AuthorityGrant,
    policy: &DelegationPolicy,
) -> Result<(), AuthorityError> {
    if policy.schema_version != 1
        || grant.allow_stop_descendants
        || policy.graph_limits.max_nodes == 0
        || policy.graph_limits.max_nodes as usize > MAX_CONTRACT_NODES
        || policy.graph_limits.max_edges as usize > MAX_GRAPH_EDGES
        || policy.operations.len() > 11
        || policy.templates.len() > 32
        || policy.required_conditions.len() > MAX_COMPLETION_CONDITIONS
        || policy.completion_criteria.len() > MAX_COMPLETION_CONDITIONS
        || policy.machine_blockers.len() > 128
    {
        return Err(AuthorityError::Invalid(
            "invalid delegation bounds or mixed legacy policy",
        ));
    }
    let mut operations = HashSet::new();
    for permission in &policy.operations {
        let valid_scope = match &permission.targets {
            DelegatedTargetScope::Nodes { nodes } => {
                let mut seen = HashSet::new();
                !nodes.is_empty()
                    && nodes.len() <= MAX_CONTRACT_NODES
                    && nodes.iter().all(|node| {
                        node_allowed(grant, node)
                            && seen.insert(node)
                            && (permission.operation != DelegatedOperation::StopActivation
                                || node != &grant.holder)
                    })
            }
            DelegatedTargetScope::Subtree { root, .. } => node_allowed(grant, root),
        };
        if !operations.insert(permission.operation) || !valid_scope {
            return Err(AuthorityError::Invalid("invalid delegated operation scope"));
        }
    }
    let mut templates = HashSet::new();
    if policy.templates.iter().any(|template| {
        !templates.insert(&template.definition_id)
            || !grant
                .profiles
                .iter()
                .any(|profile| profile.definition == template.definition_id.as_str())
    }) {
        return Err(AuthorityError::Invalid(
            "duplicate or unprofiled delegated template",
        ));
    }
    let mut conditions = HashSet::new();
    for condition in &policy.required_conditions {
        let mut nodes = HashSet::new();
        if !conditions.insert(&condition.condition_id)
            || condition.nodes.is_empty()
            || condition
                .nodes
                .iter()
                .any(|node| !node_allowed(grant, node) || !nodes.insert(node))
        {
            return Err(AuthorityError::Invalid(
                "invalid required delegated condition",
            ));
        }
    }
    let mut criteria = HashSet::new();
    let mut blockers = HashSet::new();
    if policy
        .completion_criteria
        .iter()
        .any(|criterion| !criteria.insert(criterion))
        || policy
            .machine_blockers
            .iter()
            .any(|blocker| !blockers.insert(&blocker.blocker_type))
    {
        return Err(AuthorityError::Invalid(
            "duplicate delegated criterion or blocker type",
        ));
    }
    Ok(())
}

fn validate_data(data: &AuthorityData) -> Result<(), AuthorityError> {
    delegation::validate_delegated_records(data)?;
    if let Some(scope) = &data.lifecycle_suspension {
        if !data.closed || uuid::Uuid::parse_str(scope).is_err() {
            return Err(AuthorityError::Invalid("invalid lifecycle suspension"));
        }
    }
    if data.schema_version != 1
        || data.grants.len() > MAX_GRANTS
        || data.activations.len() > MAX_ACTIVATIONS
        || total_claims(data) > MAX_CLAIMS
    {
        return Err(AuthorityError::Invalid("invalid authority schema/bounds"));
    }
    let mut grants = HashSet::new();
    for grant in &data.grants {
        validate_grant(&grant.policy)?;
        validate_grant_owner(&grant.policy, data)?;
        if !grants.insert(&grant.policy.id) || grant.previous_policies.len() >= 64 {
            return Err(AuthorityError::Invalid(
                "duplicate grant or excessive history",
            ));
        }
        if grant
            .revoked_at_revision
            .is_some_and(|revision| revision == 0 || revision > data.revision)
        {
            return Err(AuthorityError::Invalid("invalid revocation revision"));
        }
        let mut expansion_revisions = HashSet::new();
        if grant.expansions.iter().any(|item| {
            item.revision < 2
                || item.revision > grant.policy.revision
                || !expansion_revisions.insert(item.revision)
        }) {
            return Err(AuthorityError::Invalid("invalid grant expansion history"));
        }
        let mut previous: Option<&AuthorityGrant> = None;
        for policy in grant
            .previous_policies
            .iter()
            .chain(std::iter::once(&grant.policy))
        {
            validate_grant(policy)?;
            validate_grant_owner(policy, data)?;
            if let Some(old) = previous {
                if !(narrower(policy, old)
                    || (grant
                        .expansions
                        .iter()
                        .any(|item| item.revision == policy.revision)
                        && expansion::expanded(policy, old)))
                {
                    return Err(AuthorityError::Invalid("invalid grant history"));
                }
            } else if policy.revision != 1 {
                return Err(AuthorityError::Invalid("missing initial grant"));
            }
            previous = Some(policy);
        }
    }
    let mut activations = HashSet::new();
    let mut generations: HashMap<&TurnNodeId, (u32, bool)> = HashMap::new();
    for activation in &data.activations {
        validate_profile(&activation.profile)?;
        if activation.activation.generation == 0
            || activation.activation.session_id != data.session_id
            || activation.activation.turn_id != data.turn_id
            || !activations.insert(&activation.activation.activation_id)
            || !grants.contains(&activation.grant_id)
            || activation.grant_revision == 0
            || (activation.never_dispatched
                && (!activation.stopped
                    || !activation.provider_gated
                    || data.canonical_journal_id.is_none()))
        {
            return Err(AuthorityError::Invalid("invalid activation owner"));
        }
        let grant = &data.grants[grant_index(data, &activation.grant_id)?];
        let policy = grant
            .previous_policies
            .iter()
            .chain(std::iter::once(&grant.policy))
            .find(|p| p.revision == activation.grant_revision)
            .ok_or(AuthorityError::Invalid("missing activation grant revision"))?;
        if !node_allowed(policy, &activation.activation.node_id)
            || !profile_allowed(&activation.profile, policy)
        {
            return Err(AuthorityError::Invalid("invalid recorded activation grant"));
        }
        if generations
            .get(&activation.activation.node_id)
            .is_some_and(|(generation, stopped)| {
                !stopped || *generation >= activation.activation.generation
            })
        {
            return Err(AuthorityError::Invalid(
                "invalid per-node generation history",
            ));
        }
        generations.insert(
            &activation.activation.node_id,
            (activation.activation.generation, activation.stopped),
        );
    }
    let mut invocations = HashSet::new();
    for claim in &data.claims {
        bounded(&claim.dispatch_scope, 128)?;
        bounded(&claim.audit_id, 128)?;
        if claim.intent.invocation_id != claim.invocation
            || claim.intent.activation != claim.activation
            || claim.intent.dispatch_scope != claim.dispatch_scope
            || claim.intent.authority.grant_id != claim.grant_id
            || claim.intent.authority.grant_revision != claim.grant_revision
            || !invocations.insert(&claim.invocation)
            || !data.activations.iter().any(|a| {
                a.activation == claim.activation
                    && !a.never_dispatched
                    && a.grant_id == claim.grant_id
                    && expansion::recorded_revision(
                        data,
                        &claim.grant_id,
                        a.grant_revision,
                        claim.grant_revision,
                    )
            })
        {
            return Err(AuthorityError::Invalid("invalid claim owner"));
        }
    }
    let mut provider_ids = HashSet::new();
    for call in &data.provider_calls {
        validate_provider_intent(&call.intent)?;
        bounded(&call.dispatch_scope, 128)?;
        if !provider_ids.insert(&call.intent.call_id)
            || !data.activations.iter().any(|activation| {
                activation.activation == call.activation
                    && activation.provider_gated
                    && !activation.never_dispatched
                    && activation.grant_id == call.grant_id
                    && expansion::recorded_revision(
                        data,
                        &call.grant_id,
                        activation.grant_revision,
                        call.grant_revision,
                    )
                    && activation.profile.provider == call.intent.provider
                    && activation.profile.model == call.intent.model
            })
        {
            return Err(AuthorityError::Invalid(
                "invalid provider claim ownership or profile",
            ));
        }
        if let Some(outcome) = &call.outcome {
            validate_provider_outcome(outcome)?;
        }
    }
    let mut condition_ids = HashSet::new();
    for call in &data.condition_calls {
        validate_condition_record(call)?;
        if data.canonical_journal_id.is_none()
            || call.run.session_id != data.session_id
            || call.run.turn_id != data.turn_id
            || !condition_ids.insert(&call.run.run_id)
        {
            return Err(AuthorityError::Invalid("invalid condition claim owner"));
        }
        let grant = &data.grants[grant_index(data, call.grant.grant_id.as_str())?];
        let policy = grant
            .previous_policies
            .iter()
            .chain(std::iter::once(&grant.policy))
            .find(|policy| policy.revision == call.grant.revision)
            .ok_or(AuthorityError::Invalid("missing condition grant revision"))?;
        if (!condition_allowed(call, policy)
            && !legacy_standing_condition_allowed(call, grant, policy)
            && !checks::condition_allowed(call, grant, policy))
            || call.claimed_at_ms >= policy.expires_at_ms
        {
            return Err(AuthorityError::Invalid(
                "condition claim exceeds recorded permission",
            ));
        }
    }
    for grant in &data.grants {
        checks::validate_host_checks(data, grant)?;
        let mut expected = validate_legacy_standing(data, grant)?;
        expected.activations = expected
            .activations
            .checked_add(
                data.activations
                    .iter()
                    .filter(|a| a.grant_id == grant.policy.id && !a.never_dispatched)
                    .count() as u32,
            )
            .ok_or(AuthorityError::Capacity)?;
        for claim in data.claims.iter().filter(|c| c.grant_id == grant.policy.id) {
            expected.invocations += 1;
            expected.tokens = expected
                .tokens
                .checked_add(claim.reservation.tokens)
                .ok_or(AuthorityError::Capacity)?;
            expected.cost_microunits = expected
                .cost_microunits
                .checked_add(claim.reservation.cost_microunits)
                .ok_or(AuthorityError::Capacity)?;
        }
        for call in data
            .provider_calls
            .iter()
            .filter(|call| call.grant_id == grant.policy.id)
        {
            expected.invocations = expected
                .invocations
                .checked_add(1)
                .ok_or(AuthorityError::Capacity)?;
            expected.tokens = expected
                .tokens
                .checked_add(call.intent.reservation.tokens)
                .ok_or(AuthorityError::Capacity)?;
            expected.cost_microunits = expected
                .cost_microunits
                .checked_add(call.intent.reservation.cost_microunits)
                .ok_or(AuthorityError::Capacity)?;
        }
        for _ in data
            .condition_calls
            .iter()
            .filter(|call| call.grant.grant_id.as_str() == grant.policy.id)
        {
            expected.invocations = expected
                .invocations
                .checked_add(1)
                .ok_or(AuthorityError::Capacity)?;
        }
        for child in data
            .grants
            .iter()
            .filter_map(|child| child.delegated_from.as_ref())
            .filter(|reservation| reservation.parent_grant_id == grant.policy.id)
        {
            expected = delegation::add_reserved_limits(&expected, &child.limits)?;
        }
        if expected != grant.usage {
            return Err(AuthorityError::Invalid(
                "usage does not match retained reservations",
            ));
        }
    }
    if reserved_size(data, serde_json::to_vec(data)?.len()) > MAX_BYTES {
        return Err(AuthorityError::Capacity);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod provider_tests {
    use super::*;
    use crate::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
    use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use crate::turn_contract::{
        ActivationId, AgentDefinitionId, CommandId, ExecutionEpochId, TurnContractEnvelope,
        TurnContractEvent,
    };
    use std::sync::Arc;

    struct UndispatchedFixture {
        root: tempfile::TempDir,
        ownership: Arc<UpgradedFormatOwnership>,
        canonical: SessionExecutionStore,
        gate: ControlAuthority,
        snapshot: DurableTurnSnapshot,
        profile: ExecutionProfile,
    }

    fn undispatched_fixture() -> UndispatchedFixture {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/partial_finish_is_not_success.json"
        ))
        .unwrap();
        let events: Vec<TurnContractEnvelope> = fixture["steps"].as_array().unwrap()[..2]
            .iter()
            .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
            .collect();
        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let mut canonical = SessionExecutionStore::open(
            ownership.clone(),
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: events[0].session_id.clone(),
            },
        )
        .unwrap();
        for event in &events {
            canonical.append(event.clone()).unwrap();
        }
        let snapshot = canonical.snapshot(&events[0].turn_id).unwrap();
        let input = &snapshot.contract().activations()[0].input;
        let grant = input.grant.as_ref().unwrap();
        let profile = ExecutionProfile {
            definition: input.definition.definition_id.as_str().into(),
            ..profile()
        };
        let gate = ControlAuthority::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ControlAuthority {
                    turn_id: snapshot.turn_id().clone(),
                })
                .unwrap(),
        )
        .unwrap();
        gate.install_grant(
            AuthorityGrant {
                id: grant.grant_id.as_str().into(),
                revision: grant.revision,
                issuer_evidence: EvidenceRef::new("issuer").unwrap(),
                holder: input.activation.node_id.clone(),
                descendants: vec![],
                allow_stop_descendants: false,
                delegation: None,
                profiles: vec![profile.clone()],
                conditions: vec![],
                limits: GrantLimits {
                    activations: 1,
                    invocations: 2,
                    tokens: 1000,
                    cost_microunits: 100,
                },
                expires_at_ms: 1000,
            },
            0,
        )
        .unwrap();
        UndispatchedFixture {
            root,
            ownership,
            canonical,
            gate,
            snapshot,
            profile,
        }
    }

    fn undispatched_event(
        snapshot: &DurableTurnSnapshot,
        id: &str,
        event: TurnContractEvent,
    ) -> TurnContractEnvelope {
        TurnContractEnvelope {
            schema_version: crate::turn_contract::TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(id).unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: snapshot.turn_id().clone(),
            event,
        }
    }

    #[test]
    fn legacy_standing_carry_still_validates_on_reload() {
        let fixture = undispatched_fixture();
        let activation = fixture.snapshot.contract().activations()[0]
            .activation
            .clone();
        let grant_id = fixture.snapshot.contract().activations()[0]
            .input
            .grant
            .as_ref()
            .unwrap()
            .grant_id
            .as_str()
            .to_owned();
        let policy = fixture.gate.grant_policy(&grant_id).unwrap();
        drop(fixture.gate);
        let namespace = || {
            fixture
                .canonical
                .component_namespace(ExecutionComponent::ControlAuthority {
                    turn_id: fixture.snapshot.turn_id().clone(),
                })
                .unwrap()
        };
        // A grant as the removed standing-work inbox left it: usage its
        // shared allowance had already spent was carried into this turn.
        let consumed_before = serde_json::json!({
            "activations": 0, "invocations": 1, "tokens": 950, "cost_microunits": 95,
        });
        let mut stored: serde_json::Value =
            serde_json::from_slice(&namespace().read_limited(FILE, MAX_BYTES).unwrap()).unwrap();
        for grant in stored["grants"].as_array_mut().unwrap() {
            if grant["policy"]["id"] == grant_id.as_str() {
                grant["standing"] = serde_json::json!({
                    "receipt_id": "work-prior",
                    "grant": {
                        "id": policy.id,
                        "revision": policy.revision,
                        "limits": policy.limits,
                        "expires_at_ms": policy.expires_at_ms,
                    },
                    "consumed_before": consumed_before,
                    "conditions": [],
                });
                grant["usage"] = consumed_before.clone();
            }
        }
        let write = |value: &serde_json::Value| {
            namespace()
                .atomic_write(FILE, &serde_json::to_vec(value).unwrap())
                .unwrap()
        };
        write(&stored);
        let reopened = ControlAuthority::open_owned(namespace()).unwrap();
        assert_eq!(reopened.usage(&grant_id).unwrap().tokens, 950);
        // The carried usage still counts against the grant.
        let lease = reopened
            .register_provider_activation(
                activation,
                &grant_id,
                fixture.profile.clone(),
                reopened.revision().unwrap(),
                100,
            )
            .unwrap();
        assert!(matches!(
            reopened.claim_provider_call(&lease, intent("over-remaining"), 100),
            Err(AuthorityError::Capacity)
        ));
        let mut bounded = intent("within-remaining");
        bounded.reservation = DispatchReservation {
            tokens: 40,
            cost_microunits: 4,
        };
        let claim = reopened.claim_provider_call(&lease, bounded, 100).unwrap();
        reopened.settle_provider_call(&claim, &outcome()).unwrap();
        drop(reopened);
        let again = ControlAuthority::open_owned(namespace()).unwrap();
        assert_eq!(again.usage(&grant_id).unwrap().tokens, 990);
        drop(again);
        // Every field it was written with is kept when the store is rewritten.
        let rewritten: serde_json::Value =
            serde_json::from_slice(&namespace().read_limited(FILE, MAX_BYTES).unwrap()).unwrap();
        let carry = rewritten["grants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|grant| grant["policy"]["id"] == grant_id.as_str())
            .unwrap()["standing"]
            .clone();
        assert_eq!(carry["receipt_id"], "work-prior");
        assert_eq!(carry["consumed_before"], consumed_before);
        // Without the carry the same usage no longer balances.
        let mut uncarried = rewritten.clone();
        for grant in uncarried["grants"].as_array_mut().unwrap() {
            grant.as_object_mut().unwrap().remove("standing");
        }
        write(&uncarried);
        assert!(ControlAuthority::open_owned(namespace()).is_err());
    }

    #[test]
    fn lifecycle_recovery_preserves_terminal_revoked_and_expired_fences() {
        for mode in ["suspended", "terminal", "revoked", "expired"] {
            let UndispatchedFixture {
                root: _root,
                ownership,
                canonical,
                gate,
                snapshot,
                profile,
            } = undispatched_fixture();
            let activation = snapshot.contract().activations()[0].activation.clone();
            let grant_id = snapshot.contract().activations()[0]
                .input
                .grant
                .as_ref()
                .unwrap()
                .grant_id
                .as_str()
                .to_owned();
            gate.suspend_dispatch(gate.revision().unwrap()).unwrap();
            if mode == "terminal" {
                gate.close_dispatch(gate.revision().unwrap()).unwrap();
            }
            if mode == "revoked" {
                gate.revoke_grant(&grant_id, gate.revision().unwrap())
                    .unwrap();
            }
            assert!(gate
                .validate_activation_grant(&activation, &grant_id, &profile, 100)
                .is_err());
            assert!(gate.recover_lifecycle_suspension(&snapshot).is_err() || mode == "terminal");
            let usage = gate.usage(&grant_id).unwrap();
            drop(gate);
            drop(canonical);
            let canonical =
                SessionExecutionStore::open_existing(ownership, snapshot.owner().clone()).unwrap();
            let recovered = canonical.snapshot(snapshot.turn_id()).unwrap();
            assert_eq!(
                recovered.contract().state(),
                Some(LogicalTurnState::NeedsAttention)
            );
            let gate = ControlAuthority::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::ControlAuthority {
                        turn_id: snapshot.turn_id().clone(),
                    })
                    .unwrap(),
            )
            .unwrap();
            gate.recover_lifecycle_suspension(&recovered).unwrap();
            assert_eq!(gate.usage(&grant_id).unwrap(), usage);
            let result = gate.validate_activation_grant(
                &activation,
                &grant_id,
                &profile,
                if mode == "expired" { 1000 } else { 100 },
            );
            assert_eq!(result.is_ok(), mode == "suspended", "{mode}: {result:?}");
            assert!(gate.lock().unwrap().data.activations.is_empty());
        }
    }

    #[test]
    fn undispatched_proof_is_durable_zero_without_a_lease_or_budget_charge() {
        let fixture = undispatched_fixture();
        let activation = &fixture.snapshot.contract().activations()[0].activation;
        let grant_id = fixture.snapshot.contract().activations()[0]
            .input
            .grant
            .as_ref()
            .unwrap()
            .grant_id
            .as_str();
        assert!(fixture.gate.provider_usage(activation).is_err());
        let revision = fixture.gate.revision().unwrap();
        fixture
            .gate
            .record_undispatched_activation(
                &fixture.snapshot,
                activation,
                fixture.profile.clone(),
                revision,
            )
            .unwrap();
        let bytes = fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap();
        fixture
            .gate
            .record_undispatched_activation(
                &fixture.snapshot,
                activation,
                fixture.profile.clone(),
                revision,
            )
            .unwrap();
        assert_eq!(fixture.gate.revision().unwrap(), revision + 1);
        assert_eq!(
            fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap(),
            bytes
        );
        assert_eq!(fixture.gate.usage(grant_id).unwrap(), GrantUsage::default());
        let usage = fixture.gate.provider_usage(activation).unwrap();
        assert_eq!(
            usage.tokens,
            MeasuredTokenUsage::known(TokenUsageStats::default())
        );
        assert_eq!(
            (
                usage.calls,
                usage.unsettled_calls,
                usage.cost_microunits,
                usage.cost_known
            ),
            (0, 0, 0, true)
        );
        assert!(fixture
            .gate
            .register_provider_activation(
                activation.clone(),
                grant_id,
                fixture.profile.clone(),
                revision + 1,
                100
            )
            .is_err());
        // Even an internal synthetic ticket cannot execute the inert record.
        let forged = ActivationLease {
            scope: fixture.gate.scope.clone(),
            activation: activation.clone(),
            grant_id: grant_id.into(),
            grant_revision: 1,
        };
        assert!(fixture
            .gate
            .claim_provider_call(&forged, intent("must-not-run"), 100)
            .is_err());
        assert!(fixture
            .gate
            .prepare_dispatch(
                &forged,
                InvocationId::new("must-not-run").unwrap(),
                "tool".into(),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0
                },
                100
            )
            .is_err());
        assert_eq!(
            fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap(),
            bytes
        );
        validate_data(&fixture.gate.lock().unwrap().data).unwrap();
    }

    #[test]
    fn undispatched_ack_before_canonical_failure_survives_restart_and_permits_a_charged_retry() {
        let UndispatchedFixture {
            root,
            ownership,
            canonical,
            gate,
            snapshot,
            profile,
        } = undispatched_fixture();
        let activation = snapshot.contract().activations()[0].activation.clone();
        let grant_id = snapshot.contract().activations()[0]
            .input
            .grant
            .as_ref()
            .unwrap()
            .grant_id
            .as_str()
            .to_owned();
        gate.record_undispatched_activation(
            &snapshot,
            &activation,
            profile.clone(),
            gate.revision().unwrap(),
        )
        .unwrap();
        // Crash cut: the proof is durable, but canonical Fail has not happened.
        drop(gate);
        drop(canonical);
        let mut canonical =
            SessionExecutionStore::open(ownership, snapshot.owner().clone()).unwrap();
        let interrupted = canonical.snapshot(snapshot.turn_id()).unwrap();
        assert_eq!(
            interrupted.contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        let gate = ControlAuthority::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ControlAuthority {
                    turn_id: snapshot.turn_id().clone(),
                })
                .unwrap(),
        )
        .unwrap();
        let revision = gate.revision().unwrap();
        gate.record_undispatched_activation(&interrupted, &activation, profile.clone(), 0)
            .unwrap();
        assert_eq!(gate.revision().unwrap(), revision);
        assert!(gate.provider_usage(&activation).unwrap().tokens.complete);
        let mut retry = interrupted.contract().activations()[0].input.clone();
        retry.activation.generation += 1;
        retry.activation.activation_id = ActivationId::new("retry").unwrap();
        retry.activation.execution_epoch_id = ExecutionEpochId::new("retry-epoch").unwrap();
        retry.manifest_id = crate::turn_contract::InputManifestId::new("retry-input").unwrap();
        canonical
            .append(undispatched_event(
                &interrupted,
                "continue",
                TurnContractEvent::Continue {
                    plan: crate::turn_contract::ContinuationPlan {
                        source_epoch_id: activation.execution_epoch_id.clone(),
                        epoch_id: retry.activation.execution_epoch_id.clone(),
                        selections: vec![crate::turn_contract::ContinuationSelection::Retry {
                            previous: activation.clone(),
                            input: Box::new(retry.clone()),
                        }],
                        condition_runs: vec![],
                    },
                },
            ))
            .unwrap();
        let continued = canonical.snapshot(snapshot.turn_id()).unwrap();
        canonical
            .append(undispatched_event(
                &continued,
                "start-retry",
                TurnContractEvent::StartPreparedActivation {
                    activation: retry.activation.clone(),
                },
            ))
            .unwrap();
        let lease = gate
            .register_provider_activation(
                retry.activation.clone(),
                &grant_id,
                profile.clone(),
                revision,
                100,
            )
            .unwrap();
        let mut call = intent("retry-provider");
        call.model = profile.model;
        call.provider = profile.provider;
        gate.claim_provider_call(&lease, call, 100).unwrap();
        let aggregate =
            provider_usage_for(&gate.lock().unwrap().data, &[activation, retry.activation])
                .unwrap();
        assert_eq!((aggregate.calls, aggregate.unsettled_calls), (1, 1));
        assert!(!aggregate.tokens.complete);
        assert_eq!(gate.usage(&grant_id).unwrap().activations, 1);
        drop(gate);
        drop(canonical);
        drop(root);
    }

    #[test]
    fn undispatched_proof_cannot_convert_registered_or_tool_only_coverage_to_zero() {
        for mode in ["tool-only", "provider", "provider-call"] {
            let fixture = undispatched_fixture();
            let activation = &fixture.snapshot.contract().activations()[0].activation;
            let grant_id = fixture.snapshot.contract().activations()[0]
                .input
                .grant
                .as_ref()
                .unwrap()
                .grant_id
                .as_str();
            let lease = if mode == "tool-only" {
                fixture
                    .gate
                    .register_activation(
                        activation.clone(),
                        grant_id,
                        fixture.profile.clone(),
                        fixture.gate.revision().unwrap(),
                        100,
                    )
                    .unwrap()
            } else {
                fixture
                    .gate
                    .register_provider_activation(
                        activation.clone(),
                        grant_id,
                        fixture.profile.clone(),
                        fixture.gate.revision().unwrap(),
                        100,
                    )
                    .unwrap()
            };
            if mode == "provider-call" {
                fixture
                    .gate
                    .claim_provider_call(&lease, intent("already-dispatched"), 100)
                    .unwrap();
            }
            fixture
                .gate
                .stop_activation(activation, fixture.gate.revision().unwrap())
                .unwrap();
            let before = fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap();
            assert!(
                fixture
                    .gate
                    .record_undispatched_activation(
                        &fixture.snapshot,
                        activation,
                        fixture.profile.clone(),
                        fixture.gate.revision().unwrap()
                    )
                    .is_err(),
                "{mode}"
            );
            assert_eq!(
                fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap(),
                before
            );
            assert_eq!(fixture.gate.usage(grant_id).unwrap().activations, 1);
            if mode == "tool-only" {
                assert!(fixture.gate.provider_usage(activation).is_err());
            }
            if mode == "provider-call" {
                assert_eq!(
                    fixture
                        .gate
                        .provider_usage(activation)
                        .unwrap()
                        .unsettled_calls,
                    1
                );
            }
        }
    }

    #[test]
    fn undispatched_proof_uses_captured_policy_after_narrowing_revocation_and_dispatch_closure() {
        let fixture = undispatched_fixture();
        let activation = &fixture.snapshot.contract().activations()[0].activation;
        let grant_id = fixture.snapshot.contract().activations()[0]
            .input
            .grant
            .as_ref()
            .unwrap()
            .grant_id
            .as_str();
        let mut narrowed = fixture.gate.grant_policy(grant_id).unwrap();
        narrowed.revision += 1;
        narrowed.limits.activations = 0;
        narrowed.expires_at_ms = 1;
        fixture
            .gate
            .narrow_grant(narrowed, fixture.gate.revision().unwrap())
            .unwrap();
        fixture
            .gate
            .revoke_grant(grant_id, fixture.gate.revision().unwrap())
            .unwrap();
        fixture
            .gate
            .close_dispatch(fixture.gate.revision().unwrap())
            .unwrap();
        fixture
            .gate
            .record_undispatched_activation(
                &fixture.snapshot,
                activation,
                fixture.profile.clone(),
                fixture.gate.revision().unwrap(),
            )
            .unwrap();
        assert_eq!(fixture.gate.usage(grant_id).unwrap(), GrantUsage::default());
        assert!(
            fixture
                .gate
                .provider_usage(activation)
                .unwrap()
                .tokens
                .complete
        );
        validate_data(&fixture.gate.lock().unwrap().data).unwrap();
    }

    #[test]
    fn undispatched_proof_refuses_foreign_snapshot_profile_nonrunning_and_unowned_state() {
        let mut fixture = undispatched_fixture();
        let activation = fixture.snapshot.contract().activations()[0]
            .activation
            .clone();
        let other = undispatched_fixture();
        assert_eq!(fixture.snapshot.owner(), other.snapshot.owner());
        assert!(fixture
            .gate
            .record_undispatched_activation(
                &other.snapshot,
                &activation,
                fixture.profile.clone(),
                fixture.gate.revision().unwrap()
            )
            .is_err());
        let mut wrong_profile = fixture.profile.clone();
        wrong_profile.model = "ungranted-model".into();
        assert!(fixture
            .gate
            .record_undispatched_activation(
                &fixture.snapshot,
                &activation,
                wrong_profile,
                fixture.gate.revision().unwrap()
            )
            .is_err());
        let mut wrong_definition = fixture.profile.clone();
        wrong_definition.definition = "foreign-definition".into();
        assert!(fixture
            .gate
            .record_undispatched_activation(
                &fixture.snapshot,
                &activation,
                wrong_definition,
                fixture.gate.revision().unwrap()
            )
            .is_err());
        let unowned_dir = tempfile::tempdir().unwrap();
        let unowned = ControlAuthority::open(
            unowned_dir.path(),
            activation.session_id.clone(),
            activation.turn_id.clone(),
        )
        .unwrap();
        assert!(unowned
            .record_undispatched_activation(
                &fixture.snapshot,
                &activation,
                fixture.profile.clone(),
                0
            )
            .is_err());
        fixture
            .canonical
            .append(undispatched_event(
                &fixture.snapshot,
                "fail-without-proof",
                TurnContractEvent::FailActivation {
                    activation: activation.clone(),
                    evidence: EvidenceRef::new("failure").unwrap(),
                },
            ))
            .unwrap();
        let failed = fixture
            .canonical
            .snapshot(fixture.snapshot.turn_id())
            .unwrap();
        assert!(fixture
            .gate
            .record_undispatched_activation(
                &failed,
                &activation,
                fixture.profile.clone(),
                fixture.gate.revision().unwrap()
            )
            .is_err());
        assert!(fixture.gate.provider_usage(&activation).is_err());
        assert!(fixture.gate.lock().unwrap().data.activations.is_empty());
    }

    #[test]
    fn undispatched_records_are_capacity_bounded_and_loaded_claim_or_flag_conflicts_fail_closed() {
        let mut fixture = undispatched_fixture();
        let activation = &fixture.snapshot.contract().activations()[0].activation;
        let data = fixture.gate.lock().unwrap().data.clone();
        fixture.gate.capacity_bytes =
            reserved_size(&data, serde_json::to_vec(&data).unwrap().len());
        let before = fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap();
        assert!(matches!(
            fixture.gate.record_undispatched_activation(
                &fixture.snapshot,
                activation,
                fixture.profile.clone(),
                fixture.gate.revision().unwrap()
            ),
            Err(AuthorityError::Capacity)
        ));
        assert_eq!(
            fixture.gate.dir.read_limited(FILE, MAX_BYTES).unwrap(),
            before
        );
        fixture.gate.capacity_bytes = MAX_BYTES;
        fixture
            .gate
            .record_undispatched_activation(
                &fixture.snapshot,
                activation,
                fixture.profile.clone(),
                fixture.gate.revision().unwrap(),
            )
            .unwrap();
        let valid = fixture.gate.lock().unwrap().data.clone();
        validate_data(&valid).unwrap();
        for case in ["live", "ungated", "unowned", "charged", "provider-claim"] {
            let mut corrupt = valid.clone();
            match case {
                "live" => corrupt.activations[0].stopped = false,
                "ungated" => corrupt.activations[0].provider_gated = false,
                "unowned" => corrupt.canonical_journal_id = None,
                "charged" => corrupt.grants[0].usage.activations = 1,
                "provider-claim" => corrupt.provider_calls.push(ProviderCallRecord {
                    intent: intent("impossible-call"),
                    activation: activation.clone(),
                    grant_id: corrupt.activations[0].grant_id.clone(),
                    grant_revision: 1,
                    dispatch_scope: "historical".into(),
                    outcome: None,
                }),
                _ => panic!("unknown case"),
            }
            assert!(validate_data(&corrupt).is_err(), "{case}");
        }
        let mut legacy = serde_json::to_value(&valid).unwrap();
        legacy["activations"][0]
            .as_object_mut()
            .unwrap()
            .remove("never_dispatched");
        let legacy: AuthorityData = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.activations[0].never_dispatched);
        // An omitted flag restores ordinary charged-record rules, never inferred zero.
        assert!(validate_data(&legacy).is_err());
    }

    fn activation(node: &str) -> ActivationRef {
        ActivationRef {
            session_id: SessionId::new("session").unwrap(),
            turn_id: LogicalTurnId::new("turn").unwrap(),
            execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            node_id: TurnNodeId::new(node).unwrap(),
            generation: 1,
            activation_id: ActivationId::new(format!("activation-{node}")).unwrap(),
        }
    }
    fn profile() -> ExecutionProfile {
        ExecutionProfile {
            definition: "definition".into(),
            provider: "provider".into(),
            model: "model".into(),
            isolation: "local".into(),
            tools: vec![],
            write_scope: None,
        }
    }
    fn fixture() -> (tempfile::TempDir, ControlAuthority, ActivationLease) {
        let root = tempfile::tempdir().unwrap();
        let gate = ControlAuthority::open(
            root.path(),
            activation("a").session_id,
            activation("a").turn_id,
        )
        .unwrap();
        gate.install_grant(
            AuthorityGrant {
                id: "grant".into(),
                revision: 1,
                issuer_evidence: EvidenceRef::new("issuer").unwrap(),
                holder: TurnNodeId::new("a").unwrap(),
                descendants: vec![TurnNodeId::new("b").unwrap(), TurnNodeId::new("c").unwrap()],
                allow_stop_descendants: true,
                delegation: None,
                profiles: vec![profile()],
                conditions: vec![],
                limits: GrantLimits {
                    activations: 10,
                    invocations: 10,
                    tokens: 1000,
                    cost_microunits: 100,
                },
                expires_at_ms: 1000,
            },
            0,
        )
        .unwrap();
        let lease = gate
            .register_provider_activation(
                activation("a"),
                "grant",
                profile(),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
        (root, gate, lease)
    }
    fn intent(id: &str) -> ProviderCallIntent {
        ProviderCallIntent {
            call_id: id.into(),
            provider: "provider".into(),
            model: "model".into(),
            request_sha256: "a".repeat(64),
            request_bytes: 5,
            reservation: DispatchReservation {
                tokens: 100,
                cost_microunits: 10,
            },
            max_response_bytes: 1024,
        }
    }
    fn outcome() -> ProviderCallOutcome {
        ProviderCallOutcome {
            kind: ProviderCallTerminal::Completed,
            usage: MeasuredTokenUsage::known(TokenUsageStats::new(12, 8)),
            cost_microunits: Some(2),
            cost_known: true,
        }
    }

    fn delegation_fixture(
        operation: DelegatedOperation,
        templates: Vec<DefinitionSnapshotRef>,
    ) -> (
        tempfile::TempDir,
        ControlAuthority,
        ActivationLease,
        ActivationLease,
    ) {
        let root = tempfile::tempdir().unwrap();
        let holder = activation("a");
        let gate = ControlAuthority::open(
            root.path(),
            holder.session_id.clone(),
            holder.turn_id.clone(),
        )
        .unwrap();
        gate.install_grant(
            AuthorityGrant {
                id: "grant".into(),
                revision: 1,
                issuer_evidence: EvidenceRef::new("issuer").unwrap(),
                holder: holder.node_id.clone(),
                descendants: vec![TurnNodeId::new("b").unwrap()],
                allow_stop_descendants: false,
                delegation: Some(Box::new(DelegationPolicy {
                    schema_version: 1,
                    scope: DelegationScope {
                        session_id: holder.session_id.clone(),
                        turn_id: holder.turn_id.clone(),
                        task: EvidenceRef::new("task").unwrap(),
                        approved_graph: EvidenceRef::new("graph").unwrap(),
                    },
                    operations: vec![DelegatedOperationPermission {
                        operation,
                        targets: DelegatedTargetScope::Subtree {
                            root: holder.node_id.clone(),
                            include_future_descendants: true,
                        },
                    }],
                    templates,
                    resource_policy: EvidenceRef::new("resources").unwrap(),
                    graph_limits: DelegatedGraphLimits {
                        max_nodes: 4,
                        max_edges: 4,
                    },
                    required_conditions: vec![],
                    completion_criteria: vec![],
                    machine_blockers: vec![],
                    replay_policy: DelegatedReplayPolicy::RequireProvedEffectSafety,
                })),
                profiles: vec![profile()],
                conditions: vec![],
                limits: GrantLimits {
                    activations: 10,
                    invocations: 10,
                    tokens: 1000,
                    cost_microunits: 100,
                },
                expires_at_ms: 1000,
            },
            0,
        )
        .unwrap();
        // Stand in for acknowledge_native_delegation, which needs a running
        // canonical turn; the gate only checks that the journal ids match.
        {
            let mut state = gate.lock().unwrap();
            state.data.canonical_journal_id = Some("journal".into());
            state.data.grants[0].native_delegation = Some("journal".into());
        }
        let holder_lease = gate
            .register_provider_activation(holder, "grant", profile(), gate.revision().unwrap(), 100)
            .unwrap();
        let descendant_lease = gate
            .register_provider_activation(
                activation("b"),
                "grant",
                profile(),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
        (root, gate, holder_lease, descendant_lease)
    }

    #[test]
    fn delegate_port_opens_only_for_a_holder_that_may_add_agents_from_templates() {
        let dispatch = |gate: &ControlAuthority, lease: &ActivationLease, id: &str| {
            gate.prepare_dispatch(
                lease,
                InvocationId::new(id).unwrap(),
                DELEGATE_TOOL.into(),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0,
                },
                100,
            )
        };
        let template = || DefinitionSnapshotRef {
            definition_id: AgentDefinitionId::new("definition").unwrap(),
            snapshot: EvidenceRef::new("template").unwrap(),
        };

        let (_root, gate, lease) = fixture();
        assert!(
            matches!(
                dispatch(&gate, &lease, "no-policy"),
                Err(AuthorityError::Denied)
            ),
            "a grant without delegation never exposes the port"
        );

        let (_root, gate, holder, descendant) =
            delegation_fixture(DelegatedOperation::AddAgent, vec![template()]);
        dispatch(&gate, &holder, "holder").unwrap();
        assert!(
            matches!(
                dispatch(&gate, &descendant, "descendant"),
                Err(AuthorityError::Denied)
            ),
            "only the delegation holder may delegate"
        );
        assert!(
            matches!(
                gate.prepare_dispatch(
                    &holder,
                    InvocationId::new("removed-control-port").unwrap(),
                    "coordination_control".into(),
                    DispatchReservation {
                        tokens: 0,
                        cost_microunits: 0,
                    },
                    100,
                ),
                Err(AuthorityError::Denied)
            ),
            "the removed model control port stays closed for a delegation holder"
        );

        let (_root, gate, holder, _) =
            delegation_fixture(DelegatedOperation::Inspect, vec![template()]);
        assert!(
            matches!(
                dispatch(&gate, &holder, "inspect-only"),
                Err(AuthorityError::Denied)
            ),
            "a policy that cannot add an Agent keeps the port closed"
        );

        let (_root, gate, holder, _) = delegation_fixture(DelegatedOperation::AddAgent, vec![]);
        assert!(
            matches!(
                dispatch(&gate, &holder, "no-templates"),
                Err(AuthorityError::Denied)
            ),
            "a policy without helper templates keeps the port closed"
        );
    }

    #[test]
    fn activation_grant_preview_uses_exact_commit_capacity_and_preserves_pending_settlement_space()
    {
        let (root, mut gate, lease) = fixture();
        let claim = gate
            .claim_provider_call(&lease, intent("pending"), 100)
            .unwrap();
        let proposed = activation("b");
        let data = gate.lock().unwrap().data.clone();
        let (candidate, _) =
            activation_registration_candidate(&data, &proposed, "grant", &profile(), 100, false)
                .unwrap();
        let (candidate, bytes) = gate.prepare_commit(&data, candidate).unwrap();
        let exact = reserved_size(&candidate, bytes.len());
        assert!(exact > bytes.len() + PROVIDER_OUTCOME_RESERVE);
        let original = std::fs::read(root.path().join(FILE)).unwrap();
        let revision = gate.revision().unwrap();
        let usage = gate.usage("grant").unwrap();
        gate.capacity_bytes = exact - 1;
        assert!(matches!(
            gate.validate_activation_grant(&proposed, "grant", &profile(), 100),
            Err(AuthorityError::Capacity)
        ));
        assert!(matches!(
            gate.register_activation(proposed.clone(), "grant", profile(), revision, 100),
            Err(AuthorityError::Capacity)
        ));
        assert_eq!(gate.revision().unwrap(), revision);
        assert_eq!(gate.usage("grant").unwrap(), usage);
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), original);
        assert!(!gate.lock().unwrap().poisoned);
        gate.capacity_bytes = exact;
        gate.validate_activation_grant(&proposed, "grant", &profile(), 100)
            .unwrap();
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), original);
        gate.register_activation(proposed, "grant", profile(), revision, 100)
            .unwrap();
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), bytes);
        gate.settle_provider_call(&claim, &outcome()).unwrap();
        assert_eq!(
            gate.provider_call("pending").unwrap().unwrap().outcome,
            Some(outcome())
        );
        assert!(std::fs::read(root.path().join(FILE)).unwrap().len() <= exact);
    }

    #[test]
    fn activation_grant_preview_does_not_register_or_charge_a_valid_proposal() {
        let (root, gate, _) = fixture();
        let proposed = activation("b");
        let revision = gate.revision().unwrap();
        let usage = gate.usage("grant").unwrap();
        let bytes = std::fs::read(root.path().join(FILE)).unwrap();
        for _ in 0..2 {
            gate.validate_activation_grant(&proposed, "grant", &profile(), 100)
                .unwrap();
        }
        assert_eq!(gate.revision().unwrap(), revision);
        assert_eq!(gate.usage("grant").unwrap(), usage);
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), bytes);
        assert_eq!(gate.lock().unwrap().data.activations.len(), 1);
        let lease = gate
            .register_provider_activation(proposed.clone(), "grant", profile(), revision, 100)
            .unwrap();
        assert_eq!(lease.activation, proposed);
        assert_eq!(gate.revision().unwrap(), revision + 1);
        assert_eq!(
            gate.usage("grant").unwrap().activations,
            usage.activations + 1
        );
        assert_eq!(gate.usage("grant").unwrap().tokens, usage.tokens);
        assert_eq!(
            gate.usage("grant").unwrap().cost_microunits,
            usage.cost_microunits
        );
        assert!(matches!(
            gate.validate_activation_grant(&proposed, "grant", &profile(), 100),
            Err(AuthorityError::StaleLease)
        ));
    }

    #[test]
    fn activation_grant_preview_refuses_current_authority_and_generation_conflicts_without_writes()
    {
        for case in [
            "expired",
            "revoked",
            "session",
            "turn",
            "node",
            "profile",
            "closed",
            "zero_generation",
            "active_node",
            "duplicate_id",
            "stopped_generation",
            "capacity",
        ] {
            let (root, gate, _) = fixture();
            let mut proposed = activation("b");
            let mut proposed_profile = profile();
            let mut now_ms = 100;
            match case {
                "expired" => now_ms = 1000,
                "revoked" => gate
                    .revoke_grant("grant", gate.revision().unwrap())
                    .unwrap(),
                "session" => proposed.session_id = SessionId::new("foreign-session").unwrap(),
                "turn" => proposed.turn_id = LogicalTurnId::new("foreign-turn").unwrap(),
                "node" => proposed.node_id = TurnNodeId::new("outside-grant").unwrap(),
                "profile" => proposed_profile.model = "outside-grant".into(),
                "closed" => gate.close_dispatch(gate.revision().unwrap()).unwrap(),
                "zero_generation" => proposed.generation = 0,
                "active_node" => {
                    proposed.node_id = activation("a").node_id;
                    proposed.generation = 2;
                }
                "duplicate_id" => proposed.activation_id = activation("a").activation_id,
                "stopped_generation" => {
                    gate.stop_activation(&activation("a"), gate.revision().unwrap())
                        .unwrap();
                    proposed.node_id = activation("a").node_id;
                }
                "capacity" => {
                    let mut grant = gate.grant_policy("grant").unwrap();
                    grant.revision += 1;
                    grant.limits.activations = 1;
                    gate.narrow_grant(grant, gate.revision().unwrap()).unwrap();
                }
                _ => panic!("unknown case"),
            }
            let revision = gate.revision().unwrap();
            let usage = gate.usage("grant").unwrap();
            let bytes = std::fs::read(root.path().join(FILE)).unwrap();
            assert!(
                gate.validate_activation_grant(&proposed, "grant", &proposed_profile, now_ms)
                    .is_err(),
                "{case}"
            );
            assert!(
                gate.register_activation(proposed, "grant", proposed_profile, revision, now_ms)
                    .is_err(),
                "{case}"
            );
            assert_eq!(gate.revision().unwrap(), revision, "{case}");
            assert_eq!(gate.usage("grant").unwrap(), usage, "{case}");
            assert_eq!(
                std::fs::read(root.path().join(FILE)).unwrap(),
                bytes,
                "{case}"
            );
        }
    }

    #[test]
    fn successful_activation_grant_preview_does_not_survive_a_later_revocation() {
        let (root, gate, _) = fixture();
        let proposed = activation("b");
        gate.validate_activation_grant(&proposed, "grant", &profile(), 100)
            .unwrap();
        gate.revoke_grant("grant", gate.revision().unwrap())
            .unwrap();
        let revision = gate.revision().unwrap();
        let usage = gate.usage("grant").unwrap();
        let bytes = std::fs::read(root.path().join(FILE)).unwrap();
        assert!(matches!(
            gate.register_provider_activation(proposed, "grant", profile(), revision, 100),
            Err(AuthorityError::Denied)
        ));
        assert_eq!(gate.revision().unwrap(), revision);
        assert_eq!(gate.usage("grant").unwrap(), usage);
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), bytes);
    }

    #[test]
    fn provider_terminal_space_survives_unrelated_admission_at_exact_capacity() {
        let (root, mut gate, lease) = fixture();
        let claim = gate
            .claim_provider_call(&lease, intent("first"), 100)
            .unwrap();
        let data = gate.lock().unwrap().data.clone();
        gate.capacity_bytes = reserved_size(&data, serde_json::to_vec(&data).unwrap().len());
        let before = std::fs::read(root.path().join(FILE)).unwrap();
        assert!(matches!(
            gate.register_provider_activation(
                activation("b"),
                "grant",
                profile(),
                gate.revision().unwrap(),
                100
            ),
            Err(AuthorityError::Capacity)
        ));
        assert_eq!(std::fs::read(root.path().join(FILE)).unwrap(), before);
        gate.stop_activation(claim.activation(), gate.revision().unwrap())
            .unwrap();
        let maximal = ProviderCallOutcome {
            kind: ProviderCallTerminal::Interrupted,
            usage: MeasuredTokenUsage::lower_bound(
                TokenUsageStats::new(usize::MAX, usize::MAX).with_reasoning(usize::MAX),
            ),
            cost_microunits: Some(u64::MAX),
            cost_known: false,
        };
        assert!(serde_json::to_vec(&maximal).unwrap().len() < PROVIDER_OUTCOME_RESERVE);
        gate.settle_provider_call(&claim, &maximal).unwrap();
        assert!(std::fs::read(root.path().join(FILE)).unwrap().len() <= gate.capacity_bytes);
        drop(gate);
        let gate = ControlAuthority::open(
            root.path(),
            activation("a").session_id,
            activation("a").turn_id,
        )
        .unwrap();
        assert_eq!(
            gate.provider_call("first").unwrap().unwrap().outcome,
            Some(maximal)
        );
        assert_eq!(gate.usage("grant").unwrap().tokens, 100);
    }

    #[test]
    fn exact_settlement_retry_after_lost_ack_does_not_double_count_or_erase_unknown_spend() {
        let (root, gate, lease) = fixture();
        let claim = gate
            .claim_provider_call(&lease, intent("first"), 100)
            .unwrap();
        gate.settle_provider_call(&claim, &outcome()).unwrap();
        let second = gate
            .claim_provider_call(&lease, intent("second"), 100)
            .unwrap();
        let mut partial = outcome();
        partial.kind = ProviderCallTerminal::Interrupted;
        partial.usage = MeasuredTokenUsage::lower_bound(TokenUsageStats::new(3, 1));
        partial.cost_microunits = None;
        partial.cost_known = false;
        // The observation was published; its caller may lose the return value.
        gate.settle_provider_call(&second, &partial).unwrap();
        drop(gate);
        let gate = ControlAuthority::open(
            root.path(),
            activation("a").session_id,
            activation("a").turn_id,
        )
        .unwrap();
        let revision = gate.revision().unwrap();
        gate.settle_provider_call(&second, &partial).unwrap();
        assert_eq!(gate.revision().unwrap(), revision);
        let usage = gate.provider_usage(&activation("a")).unwrap();
        assert_eq!(
            usage.tokens,
            MeasuredTokenUsage::lower_bound(TokenUsageStats::new(15, 9))
        );
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.cost_microunits, 2);
        assert!(!usage.cost_known);
        assert_eq!(gate.usage("grant").unwrap().tokens, 200);
    }

    #[test]
    fn aggregate_requires_exact_gated_coverage_and_counts_each_activation_once() {
        let (_root, gate, lease) = fixture();
        let first = gate
            .claim_provider_call(&lease, intent("first"), 100)
            .unwrap();
        gate.settle_provider_call(&first, &outcome()).unwrap();
        let second_lease = gate
            .register_provider_activation(
                activation("b"),
                "grant",
                profile(),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
        gate.claim_provider_call(&second_lease, intent("second"), 100)
            .unwrap();
        gate.register_activation(
            activation("c"),
            "grant",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
        let data = gate.lock().unwrap().data.clone();
        let usage = provider_usage_for(&data, &[activation("a"), activation("b")]).unwrap();
        assert_eq!(
            usage.tokens,
            MeasuredTokenUsage::lower_bound(TokenUsageStats::new(12, 8))
        );
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.unsettled_calls, 1);
        assert!(!usage.cost_known);
        for requested in [
            vec![activation("a"), activation("a")],
            vec![activation("c")],
            vec![activation("missing")],
        ] {
            assert!(provider_usage_for(&data, &requested).is_err());
        }
    }

    #[test]
    fn zero_api_charge_survives_unknown_execution_without_settling_or_refunding_it() {
        let (root, gate, lease) = fixture();
        let mut free = intent("zero-api-charge");
        free.reservation.cost_microunits = 0;
        gate.claim_provider_call(&lease, free.clone(), 100).unwrap();
        drop(gate);
        let gate = ControlAuthority::open(
            root.path(),
            activation("a").session_id,
            activation("a").turn_id,
        )
        .unwrap();
        let usage = gate.provider_usage(&activation("a")).unwrap();
        assert!(usage.cost_known);
        assert_eq!(usage.cost_microunits, 0);
        assert_eq!(usage.unsettled_calls, 1);
        assert_eq!(
            usage.tokens,
            MeasuredTokenUsage::lower_bound(TokenUsageStats::default())
        );
        assert!(gate.claim_provider_call(&lease, free, 100).is_err());
        let mut next = activation("a");
        next.generation = 2;
        next.activation_id = ActivationId::new("a-next").unwrap();
        assert!(gate
            .register_provider_activation(next, "grant", profile(), gate.revision().unwrap(), 100,)
            .is_err());
        let receipt = gate.provider_settlement_receipt("zero-api-charge").unwrap();
        let partial = ProviderCallOutcome {
            kind: ProviderCallTerminal::Interrupted,
            usage: MeasuredTokenUsage::lower_bound(TokenUsageStats::new(5, 0)),
            // Old settlement writers lacked the knowledge derivation. The
            // immutable bound is still valid evidence of zero API charges.
            cost_microunits: None,
            cost_known: false,
        };
        gate.reconcile_provider_call(&receipt, &partial).unwrap();
        let usage = gate.provider_usage(&activation("a")).unwrap();
        assert!(usage.cost_known);
        assert_eq!(usage.unsettled_calls, 0);
        assert_eq!(usage.tokens, partial.usage);
        assert_eq!(gate.usage("grant").unwrap().tokens, 100);
        assert_eq!(gate.usage("grant").unwrap().invocations, 1);
        let paid = gate
            .register_provider_activation(
                activation("b"),
                "grant",
                profile(),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
        gate.claim_provider_call(&paid, intent("unknown-paid"), 100)
            .unwrap();
        let data = gate.lock().unwrap().data.clone();
        let aggregate = provider_usage_for(&data, &[activation("a"), activation("b")]).unwrap();
        assert!(!aggregate.cost_known);
        assert_eq!(aggregate.unsettled_calls, 1);
        assert!(!aggregate.tokens.complete);
    }

    fn scoped_profile(write_scope: Option<&[&str]>) -> ExecutionProfile {
        ExecutionProfile {
            tools: ["read_file", "write_file", "edit_file", "bash"]
                .into_iter()
                .map(String::from)
                .collect(),
            write_scope: write_scope
                .map(|scope| scope.iter().map(|pattern| (*pattern).to_owned()).collect()),
            ..profile()
        }
    }

    fn scoped_gate(limit: ExecutionProfile) -> (tempfile::TempDir, ControlAuthority) {
        let root = tempfile::tempdir().unwrap();
        let gate = ControlAuthority::open(
            root.path(),
            activation("a").session_id,
            activation("a").turn_id,
        )
        .unwrap();
        gate.install_grant(
            AuthorityGrant {
                id: "grant".into(),
                revision: 1,
                issuer_evidence: EvidenceRef::new("issuer").unwrap(),
                holder: TurnNodeId::new("a").unwrap(),
                descendants: vec![TurnNodeId::new("b").unwrap(), TurnNodeId::new("c").unwrap()],
                allow_stop_descendants: false,
                delegation: None,
                profiles: vec![limit],
                conditions: vec![],
                limits: GrantLimits {
                    activations: 10,
                    invocations: 10,
                    tokens: 1000,
                    cost_microunits: 100,
                },
                expires_at_ms: 1000,
            },
            0,
        )
        .unwrap();
        (root, gate)
    }

    #[test]
    fn profile_without_write_scope_keeps_its_exact_bytes() {
        assert_eq!(
            serde_json::to_string(&profile()).unwrap(),
            r#"{"definition":"definition","provider":"provider","model":"model","isolation":"local","tools":[]}"#
        );
        let decoded: ExecutionProfile = serde_json::from_str(
            r#"{"definition":"definition","provider":"provider","model":"model","isolation":"local","tools":[]}"#,
        )
        .unwrap();
        assert_eq!(decoded, profile());
        // A whole durable authority file written without a scope reopens and
        // rewrites to the same bytes.
        let (root, gate, _) = fixture();
        let stored = std::fs::read(root.path().join(FILE)).unwrap();
        assert!(!String::from_utf8_lossy(&stored).contains("write_scope"));
        let reloaded: AuthorityData = serde_json::from_slice(&stored).unwrap();
        validate_data(&reloaded).unwrap();
        assert_eq!(serde_json::to_vec(&reloaded).unwrap(), stored);
        drop(gate);
        let reopened = ControlAuthority::open(
            root.path(),
            activation("a").session_id,
            activation("a").turn_id,
        )
        .unwrap();
        assert_eq!(
            reopened.activation_profile(&activation("a")).unwrap(),
            profile()
        );
        // A read-only scope is recorded explicitly, never as absence.
        let read_only = serde_json::to_value(scoped_profile(Some(&[]))).unwrap();
        assert_eq!(read_only["write_scope"], serde_json::json!([]));
    }

    #[test]
    fn activation_cannot_register_a_wider_write_scope_than_its_grant() {
        let (_root, gate) = scoped_gate(scoped_profile(Some(&["lib/", "docs/"])));
        for (node, wider) in [
            ("a", scoped_profile(None)),
            ("a", scoped_profile(Some(&["src/"]))),
            ("a", scoped_profile(Some(&["lib/", "src/"]))),
        ] {
            assert!(matches!(
                gate.validate_activation_grant(&activation(node), "grant", &wider, 100),
                Err(AuthorityError::Denied)
            ));
            assert!(matches!(
                gate.register_activation(
                    activation(node),
                    "grant",
                    wider,
                    gate.revision().unwrap(),
                    100
                ),
                Err(AuthorityError::Denied)
            ));
        }
        for (node, narrower) in [
            ("a", scoped_profile(Some(&["lib/"]))),
            ("b", scoped_profile(Some(&[]))),
            ("c", scoped_profile(Some(&["docs/", "lib/"]))),
        ] {
            gate.register_activation(
                activation(node),
                "grant",
                narrower.clone(),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
            assert_eq!(
                gate.activation_profile(&activation(node)).unwrap(),
                narrower
            );
        }
        assert!(matches!(
            gate.activation_profile(&activation("unregistered")),
            Err(AuthorityError::Denied)
        ));
        // A grant can be narrowed to a smaller scope, never widened or opened.
        let mut policy = gate.grant_policy("grant").unwrap();
        policy.revision = 2;
        policy.profiles = vec![scoped_profile(None)];
        assert!(matches!(
            gate.narrow_grant(policy.clone(), gate.revision().unwrap()),
            Err(AuthorityError::Denied)
        ));
        policy.profiles = vec![scoped_profile(Some(&["lib/"]))];
        gate.narrow_grant(policy, gate.revision().unwrap()).unwrap();
        // An invalid pattern never becomes durable authority.
        let (_root, open) = scoped_gate(scoped_profile(None));
        assert!(matches!(
            open.register_activation(
                activation("a"),
                "grant",
                scoped_profile(Some(&["../outside"])),
                open.revision().unwrap(),
                100
            ),
            Err(AuthorityError::Invalid(_))
        ));
        let mut stored = open.lock().unwrap().data.clone();
        stored.grants[0].policy.profiles[0].write_scope = Some(vec!["lib/".into(), "lib/".into()]);
        assert!(validate_data(&stored).is_err());
    }

    #[test]
    fn read_only_profile_cannot_claim_write_tools() {
        let (_root, gate) = scoped_gate(scoped_profile(None));
        let read_only = gate
            .register_activation(
                activation("a"),
                "grant",
                scoped_profile(Some(&[])),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
        let scoped = gate
            .register_activation(
                activation("b"),
                "grant",
                scoped_profile(Some(&["lib/"])),
                gate.revision().unwrap(),
                100,
            )
            .unwrap();
        let prepare = |lease: &ActivationLease, id: &str, tool: &str| {
            gate.prepare_dispatch(
                lease,
                InvocationId::new(id).unwrap(),
                tool.into(),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0,
                },
                100,
            )
        };
        for tool in ["write_file", "edit_file"] {
            assert!(matches!(
                prepare(&read_only, &format!("read-only-{tool}"), tool),
                Err(AuthorityError::Denied)
            ));
            // A scoped writer may claim the tool; its paths are checked at execution.
            prepare(&scoped, &format!("scoped-{tool}"), tool).unwrap();
        }
        for tool in ["read_file", "bash"] {
            prepare(&read_only, &format!("read-only-{tool}"), tool).unwrap();
        }
    }

    /// Only an activation limited to named paths may be charged for the
    /// host's capture port, whatever tools its profile lists.
    #[test]
    fn only_a_path_scoped_activation_may_claim_the_capture_port() {
        for (scope, allowed) in [
            (None, false),
            (Some(&[][..]), false),
            (Some(&["lib/"][..]), true),
        ] {
            let (_root, gate) = scoped_gate(scoped_profile(None));
            let lease = gate
                .register_activation(
                    activation("a"),
                    "grant",
                    scoped_profile(scope),
                    gate.revision().unwrap(),
                    100,
                )
                .unwrap();
            let prepared = gate.prepare_dispatch(
                &lease,
                InvocationId::new("capture").unwrap(),
                REPOSITORY_CAPTURE_PORT.into(),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0,
                },
                100,
            );
            assert_eq!(prepared.is_ok(), allowed, "{scope:?}");
            if !allowed {
                assert!(matches!(prepared, Err(AuthorityError::Denied)));
            }
        }
    }
}

#[cfg(all(test, unix))]
mod condition_tests {
    use super::*;
    use crate::execution_content::{
        ActivationOutputContent, ActivationOutputLimits, ConditionOutputCapture,
        ConditionProcessStatus, ExecutionUsage, OutputKind, RepositoryCheckDefinition,
    };
    use crate::execution_ownership::LegacyFormatOwnership;
    use crate::execution_store::ExecutionStoreOwner;
    use crate::turn_contract::{
        CheckpointId, CheckpointRef, CheckpointSource, CommandId, CompletionCondition, ConditionId,
        TurnContractEnvelope, TurnContractEvent,
    };
    use std::sync::Arc;

    struct Fixture {
        _root: tempfile::TempDir,
        canonical: SessionExecutionStore,
        content: ExecutionContentStore,
        gate: ControlAuthority,
        run: ConditionRunRef,
        arguments: DurableConditionArguments,
        grant: GrantSnapshotRef,
        policy: AuthorityGrant,
    }

    fn event(
        canonical: &SessionExecutionStore,
        turn: &LogicalTurnId,
        kind: TurnContractEvent,
    ) -> TurnContractEnvelope {
        let snapshot = canonical.snapshot(turn).unwrap();
        TurnContractEnvelope {
            schema_version: crate::turn_contract::TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new(format!(
                "condition-test-{}",
                snapshot.contract().revision()
            ))
            .unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: turn.clone(),
            event: kind,
        }
    }

    // Uses real owned journals and retained output, but executes no process.
    // Physical checkpoint restoration belongs to the controller/Memory seam.
    fn condition_fixture(with_intent: bool) -> Fixture {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/partial_finish_is_not_success.json"
        ))
        .unwrap();
        let mut begin: TurnContractEnvelope =
            serde_json::from_value(source["steps"][0]["envelope"].clone()).unwrap();
        let start: TurnContractEnvelope =
            serde_json::from_value(source["steps"][1]["envelope"].clone()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut canonical = SessionExecutionStore::open(
            Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            ),
            ExecutionStoreOwner {
                workspace_id: "condition-workspace".into(),
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
        let definition = content
            .retain_repository_check_definition(RepositoryCheckDefinition {
                argv: vec!["check-fixture-only".into(), "--exact-argument".into()],
                timeout_ms: 100,
                stdout_bytes: 128,
                stderr_bytes: 64,
            })
            .unwrap();
        let repository = content
            .retain_activation_evidence(ActivationEvidenceContent::Repository {
                description: "owned test repository resource".into(),
                revision: Some("exact-revision".into()),
            })
            .unwrap();
        let TurnContractEvent::StartActivation { input } = &start.event else {
            panic!("missing fixture input")
        };
        let activation = input.activation.clone();
        let conversation = input.conversation_id.clone();
        let condition_id = ConditionId::new("repository-check").unwrap();
        let kind = ConditionKind::RepositoryCheck {
            definition: definition.reference().clone(),
        };
        let TurnContractEvent::Begin { graph, .. } = &mut begin.event else {
            panic!("missing fixture graph")
        };
        graph.conditions.push(CompletionCondition {
            condition_id: condition_id.clone(),
            kind: kind.clone(),
            nodes: vec![activation.node_id.clone()],
        });
        canonical.append(begin).unwrap();
        canonical.append(start).unwrap();
        let snapshot = canonical.snapshot(&activation.turn_id).unwrap();
        let reservation = content
            .reserve_activation_output(
                &snapshot,
                &activation,
                ActivationOutputLimits {
                    partial_records: 0,
                    partial_bytes: 0,
                    settlement_bytes: 1024,
                },
            )
            .unwrap();
        let output = content
            .settle_activation_output(
                &reservation,
                ActivationOutputContent {
                    activation: activation.clone(),
                    recorded_at_unix_ms: 1,
                    text: "accepted fixture output".into(),
                    usage: ExecutionUsage::Measured {
                        usage: TokenUsageStats::default(),
                    },
                    kind: OutputKind::Final,
                },
            )
            .unwrap();
        canonical
            .append(event(
                &canonical,
                &activation.turn_id,
                TurnContractEvent::AcceptActivation {
                    activation: activation.clone(),
                    checkpoint: Box::new(CheckpointRef {
                        checkpoint_id: CheckpointId::new("accepted-condition-fixture").unwrap(),
                        session_id: activation.session_id.clone(),
                        conversation_id: conversation,
                        source: CheckpointSource::Accepted {
                            activation: activation.clone(),
                        },
                    }),
                    output: output.reference().clone(),
                },
            ))
            .unwrap();
        let run = ConditionRunRef {
            session_id: activation.session_id.clone(),
            turn_id: activation.turn_id.clone(),
            epoch_id: activation.execution_epoch_id.clone(),
            condition_id,
            run_id: ConditionRunId::new("condition-run-one").unwrap(),
            activations: vec![activation.clone()],
        };
        let arguments = content
            .reserve_condition_arguments(
                &canonical.snapshot(&run.turn_id).unwrap(),
                &run,
                repository.reference(),
            )
            .unwrap();
        if with_intent {
            canonical
                .append(event(
                    &canonical,
                    &run.turn_id,
                    TurnContractEvent::RecordConditionIntent {
                        run: run.clone(),
                        intent: arguments.reference().clone(),
                    },
                ))
                .unwrap();
        }
        let policy = AuthorityGrant {
            id: "condition-grant".into(),
            revision: 1,
            issuer_evidence: EvidenceRef::new("condition-human-approval").unwrap(),
            holder: activation.node_id.clone(),
            descendants: vec![],
            allow_stop_descendants: false,
            delegation: None,
            profiles: vec![],
            conditions: vec![ConditionPermission {
                kind,
                nodes: vec![activation.node_id.clone()],
                repository: repository.reference().clone(),
                isolation: "owned-local-check".into(),
                max_timeout_ms: 100,
                max_stdout_bytes: 128,
                max_stderr_bytes: 64,
            }],
            limits: GrantLimits {
                activations: 0,
                invocations: 4,
                tokens: 0,
                cost_microunits: 0,
            },
            expires_at_ms: 1000,
        };
        let retained = content
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: policy.clone(),
            })
            .unwrap();
        let grant = GrantSnapshotRef {
            grant_id: GrantId::new(policy.id.clone()).unwrap(),
            revision: 1,
            evidence: retained.reference().clone(),
        };
        let gate = ControlAuthority::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ControlAuthority {
                    turn_id: run.turn_id.clone(),
                })
                .unwrap(),
        )
        .unwrap();
        gate.install_grant(policy.clone(), 0).unwrap();
        Fixture {
            _root: root,
            canonical,
            content,
            gate,
            run,
            arguments,
            grant,
            policy,
        }
    }

    fn claim(fixture: &Fixture) -> Result<ConditionCallClaim, AuthorityError> {
        fixture.gate.claim_condition_run(
            &fixture.canonical,
            &fixture.content,
            &fixture.arguments,
            &fixture.grant,
            "owned-local-check",
            100,
        )
    }

    fn result(fixture: &mut Fixture) -> DurableConditionResult {
        let mut stdout = ConditionOutputCapture::new(128).unwrap();
        stdout.observe(b"actual captured fixture stdout").unwrap();
        fixture
            .content
            .record_condition_result(
                &fixture.arguments,
                ConditionProcessStatus::Exited { code: 0 },
                stdout.finish(true),
                ConditionOutputCapture::new(64).unwrap().finish(true),
                101,
            )
            .unwrap()
    }

    #[test]
    fn condition_claim_requires_owned_intent_exact_content_permission_and_current_inputs() {
        let fixture = condition_fixture(true);
        let foreign = condition_fixture(true);
        assert!(fixture
            .gate
            .claim_condition_run(
                &foreign.canonical,
                &fixture.content,
                &fixture.arguments,
                &fixture.grant,
                "owned-local-check",
                100
            )
            .is_err());
        assert!(fixture
            .gate
            .claim_condition_run(
                &fixture.canonical,
                &foreign.content,
                &fixture.arguments,
                &fixture.grant,
                "owned-local-check",
                100
            )
            .is_err());
        assert!(fixture
            .gate
            .claim_condition_run(
                &fixture.canonical,
                &fixture.content,
                &foreign.arguments,
                &fixture.grant,
                "owned-local-check",
                100
            )
            .is_err());
        let mut forged_grant = fixture.grant.clone();
        forged_grant.evidence = fixture.arguments.reference().clone();
        assert!(fixture
            .gate
            .claim_condition_run(
                &fixture.canonical,
                &fixture.content,
                &fixture.arguments,
                &forged_grant,
                "owned-local-check",
                100
            )
            .is_err());
        assert!(fixture
            .gate
            .claim_condition_run(
                &fixture.canonical,
                &fixture.content,
                &fixture.arguments,
                &fixture.grant,
                "ungranted-isolation",
                100
            )
            .is_err());
        assert!(fixture
            .gate
            .claim_condition_run(
                &fixture.canonical,
                &fixture.content,
                &fixture.arguments,
                &fixture.grant,
                "owned-local-check",
                1000
            )
            .is_err());
        assert_eq!(
            fixture.gate.usage(&fixture.policy.id).unwrap(),
            GrantUsage::default()
        );
        let claimed = claim(&fixture).unwrap();
        fixture
            .gate
            .validate_condition_claim(&fixture.canonical, &claimed, 100)
            .unwrap();
        assert!(claim(&fixture).is_err());
        assert_eq!(
            fixture.gate.usage(&fixture.policy.id).unwrap(),
            GrantUsage {
                invocations: 1,
                ..Default::default()
            }
        );
        assert!(
            fixture.gate.lock().unwrap().data.activations.is_empty(),
            "a check is not an Agent lease"
        );
        let missing = condition_fixture(false);
        assert!(claim(&missing).is_err());
    }

    #[test]
    fn condition_claim_is_never_reissued_after_restart_and_late_result_survives_stop() {
        let mut fixture = condition_fixture(true);
        let claimed = claim(&fixture).unwrap();
        drop(fixture.gate);
        let namespace = fixture
            .canonical
            .component_namespace(ExecutionComponent::ControlAuthority {
                turn_id: fixture.run.turn_id.clone(),
            })
            .unwrap();
        fixture.gate = ControlAuthority::open_owned(namespace).unwrap();
        assert!(claim(&fixture).is_err());
        assert!(fixture
            .gate
            .validate_condition_claim(&fixture.canonical, &claimed, 100)
            .is_err());
        fixture
            .gate
            .close_dispatch(fixture.gate.revision().unwrap())
            .unwrap();
        let result = result(&mut fixture);
        let evidence_only = fixture
            .gate
            .condition_settlement_receipt(&fixture.run.run_id)
            .unwrap();
        fixture
            .gate
            .reconcile_condition_run(&evidence_only, &result)
            .unwrap();
        let revision = fixture.gate.revision().unwrap();
        fixture
            .gate
            .settle_condition_run(&claimed, &result)
            .unwrap();
        assert_eq!(fixture.gate.revision().unwrap(), revision);
        assert_eq!(
            fixture
                .gate
                .condition_call(&fixture.run.run_id)
                .unwrap()
                .unwrap()
                .result
                .as_ref(),
            Some(result.protected_result())
        );
        assert_eq!(
            fixture.gate.usage(&fixture.policy.id).unwrap().invocations,
            1
        );
        assert!(
            fixture
                .canonical
                .snapshot(&fixture.run.turn_id)
                .unwrap()
                .contract()
                .condition_run(&fixture.run.run_id)
                .unwrap()
                .resolution
                .is_none(),
            "authority settlement does not fabricate a canonical check outcome"
        );
    }

    #[test]
    fn condition_permission_narrowing_cannot_expand_scope_or_resource_and_invalidates_live_claim() {
        let mut fixture = condition_fixture(true);
        let claimed = claim(&fixture).unwrap();
        for change in [
            "definition",
            "resource",
            "isolation",
            "timeout",
            "stdout",
            "stderr",
            "nodes",
        ] {
            let mut policy = fixture.policy.clone();
            policy.revision = 2;
            let permission = &mut policy.conditions[0];
            match change {
                "definition" => {
                    permission.kind = ConditionKind::Review {
                        criterion: fixture.arguments.definition_ref().clone(),
                    }
                }
                "resource" => permission.repository = EvidenceRef::new("foreign-resource").unwrap(),
                "isolation" => permission.isolation = "unrestricted".into(),
                "timeout" => permission.max_timeout_ms += 1,
                "stdout" => permission.max_stdout_bytes += 1,
                "stderr" => permission.max_stderr_bytes += 1,
                "nodes" => permission
                    .nodes
                    .push(TurnNodeId::new("foreign-node").unwrap()),
                _ => unreachable!(),
            }
            assert!(
                fixture
                    .gate
                    .narrow_grant(policy, fixture.gate.revision().unwrap())
                    .is_err(),
                "{change}"
            );
        }
        let mut narrowed = fixture.policy.clone();
        narrowed.revision = 2;
        narrowed.conditions[0].max_timeout_ms = 50;
        fixture
            .gate
            .narrow_grant(narrowed, fixture.gate.revision().unwrap())
            .unwrap();
        assert!(fixture
            .gate
            .validate_condition_claim(&fixture.canonical, &claimed, 100)
            .is_err());
        fixture
            .gate
            .revoke_grant(&fixture.policy.id, fixture.gate.revision().unwrap())
            .unwrap();
        let result = result(&mut fixture);
        fixture
            .gate
            .settle_condition_run(&claimed, &result)
            .unwrap();
        assert_eq!(
            fixture.gate.usage(&fixture.policy.id).unwrap().invocations,
            1
        );
        validate_data(&fixture.gate.lock().unwrap().data).unwrap();
    }

    #[test]
    fn condition_terminal_capacity_and_conservative_call_charge_survive_unrelated_writes() {
        let mut fixture = condition_fixture(true);
        let original = fixture.gate.lock().unwrap().data.clone();
        fixture.gate.capacity_bytes =
            reserved_size(&original, serde_json::to_vec(&original).unwrap().len());
        assert!(matches!(claim(&fixture), Err(AuthorityError::Capacity)));
        assert_eq!(
            fixture.gate.usage(&fixture.policy.id).unwrap().invocations,
            0
        );
        fixture.gate.capacity_bytes = MAX_BYTES;
        let claimed = claim(&fixture).unwrap();
        let data = fixture.gate.lock().unwrap().data.clone();
        fixture.gate.capacity_bytes =
            reserved_size(&data, serde_json::to_vec(&data).unwrap().len());
        fixture
            .gate
            .revoke_grant(&fixture.policy.id, fixture.gate.revision().unwrap())
            .unwrap();
        fixture
            .gate
            .close_dispatch(fixture.gate.revision().unwrap())
            .unwrap();
        let result = result(&mut fixture);
        fixture
            .gate
            .settle_condition_run(&claimed, &result)
            .unwrap();
        let data = fixture.gate.lock().unwrap().data.clone();
        assert!(serde_json::to_vec(&data).unwrap().len() <= fixture.gate.capacity_bytes);
        let mut corrupt = data.clone();
        corrupt.grants[0].usage.invocations = 0;
        assert!(validate_data(&corrupt).is_err());
        let mut corrupt = data.clone();
        corrupt.condition_calls[0].run.turn_id = LogicalTurnId::new("foreign-turn").unwrap();
        assert!(validate_data(&corrupt).is_err());
        let mut corrupt = data;
        corrupt.condition_calls[0].grant.revision = 99;
        assert!(validate_data(&corrupt).is_err());
    }

    #[test]
    fn condition_write_failure_poisons_claim_admission_without_returning_an_executable_capability()
    {
        let fixture = condition_fixture(true);
        let file = fixture.gate.dir.path().join(FILE);
        let original = fixture.gate.dir.path().join("saved-authority");
        std::fs::rename(&file, &original).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(matches!(claim(&fixture), Err(AuthorityError::Io(_))));
        assert!(matches!(
            claim(&fixture),
            Err(AuthorityError::RecoveryRequired)
        ));
        assert!(fixture.gate.state.lock().unwrap().poisoned);
        std::fs::remove_dir(&file).unwrap();
        std::fs::rename(&original, &file).unwrap();
    }

    #[test]
    fn condition_invocation_limit_remains_charged_after_result_and_canonical_resolution() {
        let mut fixture = condition_fixture(true);
        let mut limited = fixture.policy.clone();
        limited.revision = 2;
        limited.limits.invocations = 1;
        fixture
            .gate
            .narrow_grant(limited.clone(), fixture.gate.revision().unwrap())
            .unwrap();
        let evidence = fixture
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Grant { policy: limited })
            .unwrap();
        fixture.grant.revision = 2;
        fixture.grant.evidence = evidence.reference().clone();
        let claimed = claim(&fixture).unwrap();
        let result = result(&mut fixture);
        fixture
            .gate
            .settle_condition_run(&claimed, &result)
            .unwrap();
        fixture
            .canonical
            .append(event(
                &fixture.canonical,
                &fixture.run.turn_id,
                TurnContractEvent::ResolveConditionIntent {
                    run_id: fixture.run.run_id.clone(),
                    resolution: crate::turn_contract::ConditionEffectResolution::OutcomeRecorded {
                        evidence: result.reference().clone(),
                    },
                },
            ))
            .unwrap();
        fixture.run.run_id = ConditionRunId::new("condition-run-two").unwrap();
        fixture.arguments = fixture
            .content
            .reserve_condition_arguments(
                &fixture.canonical.snapshot(&fixture.run.turn_id).unwrap(),
                &fixture.run,
                fixture.arguments.repository_ref(),
            )
            .unwrap();
        fixture
            .canonical
            .append(event(
                &fixture.canonical,
                &fixture.run.turn_id,
                TurnContractEvent::RecordConditionIntent {
                    run: fixture.run.clone(),
                    intent: fixture.arguments.reference().clone(),
                },
            ))
            .unwrap();
        assert!(matches!(claim(&fixture), Err(AuthorityError::Capacity)));
        assert_eq!(
            fixture.gate.usage(&fixture.policy.id).unwrap(),
            GrantUsage {
                invocations: 1,
                ..Default::default()
            }
        );
        assert!(fixture
            .gate
            .condition_call(&fixture.run.run_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn legacy_grants_authorize_no_conditions_and_last_check_permission_can_be_removed() {
        let mut fixture = condition_fixture(true);
        let mut legacy = fixture.policy.clone();
        legacy.id = "legacy-no-checks".into();
        legacy.conditions.clear();
        legacy.profiles = vec![ExecutionProfile {
            definition: "shared-coder".into(),
            provider: "local".into(),
            model: "fixture".into(),
            isolation: "local".into(),
            tools: vec![],
            write_scope: None,
        }];
        let encoded = serde_json::to_value(&legacy).unwrap();
        assert!(encoded.get("conditions").is_none());
        let decoded: AuthorityGrant = serde_json::from_value(encoded).unwrap();
        assert!(decoded.conditions.is_empty());
        fixture
            .gate
            .install_grant(decoded.clone(), fixture.gate.revision().unwrap())
            .unwrap();
        let retained = fixture
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Grant { policy: decoded })
            .unwrap();
        let old_grant = GrantSnapshotRef {
            grant_id: GrantId::new(legacy.id).unwrap(),
            revision: 1,
            evidence: retained.reference().clone(),
        };
        assert!(fixture
            .gate
            .claim_condition_run(
                &fixture.canonical,
                &fixture.content,
                &fixture.arguments,
                &old_grant,
                "owned-local-check",
                100
            )
            .is_err());
        let mut narrowed = fixture.policy.clone();
        narrowed.revision = 2;
        narrowed.conditions.clear();
        fixture
            .gate
            .narrow_grant(narrowed, fixture.gate.revision().unwrap())
            .unwrap();
        assert!(claim(&fixture).is_err());
        validate_data(&fixture.gate.lock().unwrap().data).unwrap();
    }
}

#[path = "control_authority_expansion.rs"]
mod expansion;
