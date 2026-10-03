//! Durable invocation evidence, independent of logical-turn lifecycle.
//!
//! Events are append-only: the bounded journal is atomically republished without
//! changing prior records. Late authoritative evidence may settle an invocation
//! after its turn closes; this store never changes turn state or accepted output.
//!
//! A durable intent receipt proves storage acknowledgement, not permission to
//! dispatch. The host must protect/resolve evidence, authenticate observations,
//! and revalidate the exact generation, grant, environment, and live dispatch
//! scope through its control arbiter. Reopening or repeating an intent never
//! authorizes replay. Adapter policy and evidence references are recorded claims,
//! not independently verified idempotency, reconciliation, or approval proofs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_store::DurableSessionIdentity;
use crate::turn_contract::{
    ActivationRef, CommandId, EffectDisposition, EvidenceRef, InvocationId, InvocationOutcome,
    SessionId,
};

const SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "invocation-audit.v1.json";
const MAX_INVOCATIONS: usize = 256;
const MAX_RECORDS: usize = MAX_INVOCATIONS * 2;
const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_RECORD_BYTES: usize = 17 * 1024;
const MAX_PROTECTED_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationAuditOwner {
    pub workspace_id: String,
    pub session_id: SessionId,
}

/// A protected, immutable blob already retained by the host. This store checks
/// metadata form and size; the host must verify the bytes and access boundary.
/// Neither this reference nor a redacted preview is executable argument data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedArguments {
    pub evidence_ref: EvidenceRef,
    pub sha256: String,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationAuthority {
    pub grant_id: String,
    pub grant_revision: u64,
    pub approval_ref: Option<EvidenceRef>,
}

/// Records the adapter's policy; it cannot authorize automatic replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationReplayPolicy {
    ManualOnly,
    ProviderIdempotency {
        policy_ref: EvidenceRef,
        key_ref: EvidenceRef,
    },
    ReconcileBeforeReplay {
        policy_ref: EvidenceRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderReplayIdentity {
    pub adapter_id: String,
    pub adapter_version: String,
    pub provider_run_ref: Option<EvidenceRef>,
    pub native_call_id: Option<String>,
    pub response_group_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationIntent {
    pub invocation_id: InvocationId,
    pub activation: ActivationRef,
    /// Fresh live-arbiter identity; a reconstructed arbiter must use a new scope.
    pub dispatch_scope: String,
    pub tool_name: String,
    /// Identity of the actual executable arguments after hooks and approval.
    pub arguments: ProtectedArguments,
    /// Display-only text, never substituted for protected executable arguments.
    pub redacted_preview: String,
    pub authority: InvocationAuthority,
    pub replay_policy: InvocationReplayPolicy,
    pub provider_replay: ProviderReplayIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationIntentCommand {
    pub command_id: CommandId,
    /// Per-invocation revision; a new intent requires zero.
    pub expected_revision: u64,
    pub intent: InvocationIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOutcomeSource {
    Executor,
    Reconciliation,
    HumanVerified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvocationFinalEvidence {
    Outcome {
        outcome: InvocationOutcome,
        result: ProtectedArguments,
        redacted_preview: String,
        source: InvocationOutcomeSource,
        /// Host-verified provenance of this observation, distinct from its result.
        authority_ref: EvidenceRef,
    },
    NotDispatched {
        /// Positive proof from the dispatch authority, never inferred from error.
        evidence: EvidenceRef,
        authority_ref: EvidenceRef,
    },
}

impl InvocationFinalEvidence {
    pub fn disposition(&self) -> EffectDisposition {
        match self {
            Self::Outcome { .. } => EffectDisposition::OutcomeRecorded,
            Self::NotDispatched { .. } => EffectDisposition::NotDispatched,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationEvidenceCommand {
    pub command_id: CommandId,
    pub expected_revision: u64,
    pub invocation_id: InvocationId,
    pub activation: ActivationRef,
    /// Captured dispatch authority, even if its grant has since been revoked.
    /// Revocation prevents new work; it must not prevent recording late effects.
    pub authority: InvocationAuthority,
    pub evidence: InvocationFinalEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "command",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InvocationAuditCommand {
    Intent(InvocationIntentCommand),
    Evidence(InvocationEvidenceCommand),
}

impl InvocationAuditCommand {
    fn command_id(&self) -> &CommandId {
        match self {
            Self::Intent(command) => &command.command_id,
            Self::Evidence(command) => &command.command_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationAuditRecord {
    pub sequence: u64,
    pub invocation_revision: u64,
    pub command: InvocationAuditCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditedInvocation {
    pub intent: InvocationIntent,
    pub revision: u64,
    pub final_evidence: Option<InvocationFinalEvidence>,
}

impl AuditedInvocation {
    pub fn disposition(&self) -> EffectDisposition {
        self.final_evidence.as_ref().map_or(
            EffectDisposition::OutcomeUnknown,
            InvocationFinalEvidence::disposition,
        )
    }
}

/// Opaque, non-serializable proof that this exact intent crossed a successful
/// persistence boundary. It is not a dispatch permit. Use the live arbiter's
/// scope/ticket and exact generation gate; never replay from this receipt alone.
#[derive(Debug)]
pub struct DurableIntentReceipt {
    audit_id: String,
    command: InvocationIntentCommand,
}

impl DurableIntentReceipt {
    pub fn audit_id(&self) -> &str {
        &self.audit_id
    }
    pub fn invocation_id(&self) -> &InvocationId {
        &self.command.intent.invocation_id
    }
    pub fn activation(&self) -> &ActivationRef {
        &self.command.intent.activation
    }
    pub fn intent(&self) -> &InvocationIntent {
        &self.command.intent
    }
    pub fn revision(&self) -> u64 {
        1
    }
}

/// Acknowledgement of retained authoritative evidence; does not change or accept
/// a logical turn's output. Consumers must read that turn's own authority.
#[derive(Debug)]
pub struct DurableEvidenceReceipt {
    invocation_id: InvocationId,
    revision: u64,
}

impl DurableEvidenceReceipt {
    pub fn invocation_id(&self) -> &InvocationId {
        &self.invocation_id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InvocationAuditError {
    #[error("invocation audit storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("invocation audit JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported invocation audit schema {0}")]
    UnsupportedVersion(u32),
    #[error("invalid invocation audit: {0}")]
    Invalid(&'static str),
    #[error("invocation audit belongs to another Workspace or Session")]
    OwnerConflict,
    #[error("command id was already used for different content")]
    CommandConflict,
    #[error("invocation already has a different immutable intent")]
    IntentConflict,
    #[error("invocation identity does not exist")]
    NotFound,
    #[error("invocation target or captured authority does not match its intent")]
    TargetConflict,
    #[error("stale invocation revision: expected {expected}, actual {actual}")]
    StaleRevision { expected: u64, actual: u64 },
    #[error("invocation already has authoritative final evidence")]
    EvidenceConflict,
    #[error("invocation audit capacity exhausted; no record was acknowledged")]
    Capacity,
    #[error("ambiguous write; reopen and reconcile before acknowledging or dispatching")]
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditData {
    schema_version: u32,
    audit_id: String,
    owner: InvocationAuditOwner,
    records: Vec<InvocationAuditRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical_journal_id: Option<String>,
}

#[derive(Clone, Default)]
struct AuditProjection {
    invocations: HashMap<InvocationId, AuditedInvocation>,
    commands: HashMap<CommandId, InvocationAuditCommand>,
}

/// One bounded, single-writer audit for an explicitly owned Session. The host
/// must supply an existing, durably provisioned private control-plane directory
/// outside the repository mount. No resolved or unresolved history is evicted.
pub struct InvocationAudit {
    dir: SecureDir,
    namespace: Option<OwnedExecutionNamespace>,
    data: AuditData,
    projection: AuditProjection,
    recovery_required: bool,
}

impl InvocationAudit {
    /// Read existing, identity-checked audit evidence without opening a writer,
    /// acknowledging an uncertain write, or minting a dispatch receipt.
    pub fn read_retained_views(
        canonical: &crate::execution_store::SessionExecutionStore,
        turn_id: &crate::turn_contract::LogicalTurnId,
    ) -> Result<Vec<AuditedInvocation>, InvocationAuditError> {
        let snapshot = canonical
            .snapshot(turn_id)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let bytes = canonical
            .read_existing_component(
                &ExecutionComponent::InvocationAudit,
                Path::new(FILE_NAME),
                MAX_STORE_BYTES,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let data: AuditData = serde_json::from_slice(&bytes)?;
        if data.owner.workspace_id != snapshot.owner().workspace_id
            || data.owner.session_id != snapshot.owner().session_id
            || data.canonical_journal_id.as_deref() != Some(snapshot.journal_id())
        {
            return Err(InvocationAuditError::OwnerConflict);
        }
        let projection = rebuild(&data)?;
        data.records
            .iter()
            .filter_map(|record| match &record.command {
                InvocationAuditCommand::Intent(command)
                    if &command.intent.activation.turn_id == turn_id =>
                {
                    Some(
                        projection
                            .invocations
                            .get(&command.intent.invocation_id)
                            .cloned()
                            .ok_or(InvocationAuditError::Invalid(
                                "retained intent has no audit projection",
                            )),
                    )
                }
                _ => None,
            })
            .collect()
    }

    pub fn open(
        path: impl AsRef<Path>,
        owner: InvocationAuditOwner,
    ) -> Result<Self, InvocationAuditError> {
        validate_owner(&owner)?;
        let dir = SecureDir::open(path)?;
        Self::open_in(dir, owner, None)
    }

    pub fn open_owned(namespace: OwnedExecutionNamespace) -> Result<Self, InvocationAuditError> {
        namespace.require_root(&ExecutionComponent::InvocationAudit)?;
        let owner = InvocationAuditOwner {
            workspace_id: namespace.identity().owner().workspace_id.clone(),
            session_id: namespace.identity().owner().session_id.clone(),
        };
        let dir = namespace.secure_dir()?;
        Self::open_in(dir, owner, Some(namespace))
    }

    fn open_in(
        dir: SecureDir,
        owner: InvocationAuditOwner,
        namespace: Option<OwnedExecutionNamespace>,
    ) -> Result<Self, InvocationAuditError> {
        validate_owner(&owner)?;
        dir.restrict_owner_only()?;
        #[cfg(unix)]
        dir.lock_exclusive_waiting(axocoatl_core::LOCK_INHERITANCE_GRACE)?;
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "invocation audit requires a supported single-writer directory lock",
        )
        .into());

        let canonical_journal_id = namespace
            .as_ref()
            .map(|ns| ns.identity().journal_id().to_owned());
        let data = match dir.read_limited(FILE_NAME, MAX_STORE_BYTES) {
            Ok(bytes) => {
                #[derive(Deserialize)]
                struct Header {
                    schema_version: u32,
                }
                let header: Header = serde_json::from_slice(&bytes)?;
                if header.schema_version != SCHEMA_VERSION {
                    return Err(InvocationAuditError::UnsupportedVersion(
                        header.schema_version,
                    ));
                }
                serde_json::from_slice::<AuditData>(&bytes)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(namespace) = &namespace {
                    namespace.check_journal_creation(FILE_NAME)?;
                }
                AuditData {
                    schema_version: SCHEMA_VERSION,
                    audit_id: uuid::Uuid::new_v4().to_string(),
                    owner: owner.clone(),
                    records: Vec::new(),
                    canonical_journal_id: canonical_journal_id.clone(),
                }
            }
            Err(error) => return Err(error.into()),
        };
        if data.owner != owner || data.canonical_journal_id != canonical_journal_id {
            return Err(InvocationAuditError::OwnerConflict);
        }
        let projection = rebuild(&data)?;
        if let Some(namespace) = &namespace {
            namespace.mark_journal_initialized(FILE_NAME)?;
        }
        // Reading cannot repair a prior rename whose directory fsync failed.
        // A successful atomic rewrite is required before exposing any receipt.
        dir.verify_ambient_identity()?;
        dir.atomic_write(FILE_NAME, &serde_json::to_vec(&data)?)?;
        Ok(Self {
            dir,
            namespace,
            data,
            projection,
            recovery_required: false,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(FILE_NAME)
    }

    pub fn audit_id(&self) -> Result<&str, InvocationAuditError> {
        self.ensure_usable()?;
        Ok(&self.data.audit_id)
    }

    /// Return only after intent is durable. Exact repeats return its original
    /// acknowledgement, including after an outcome; that does not revive work.
    pub fn record_intent(
        &mut self,
        command: InvocationIntentCommand,
    ) -> Result<DurableIntentReceipt, InvocationAuditError> {
        self.append(InvocationAuditCommand::Intent(command.clone()))?;
        Ok(DurableIntentReceipt {
            audit_id: self.data.audit_id.clone(),
            command,
        })
    }

    /// Record exact authoritative evidence even after logical turn closure.
    /// This module cannot authenticate the caller's proof; host verification
    /// must happen before entry. A tool error never proves it made no effect.
    pub fn record_evidence(
        &mut self,
        command: InvocationEvidenceCommand,
    ) -> Result<DurableEvidenceReceipt, InvocationAuditError> {
        self.append(InvocationAuditCommand::Evidence(command.clone()))?;
        Ok(DurableEvidenceReceipt {
            invocation_id: command.invocation_id,
            revision: 2,
        })
    }

    /// Check acknowledgement provenance and latest audit evidence. The live
    /// control arbiter must still atomically order dispatch against Stop and
    /// revocation, and require the intent's fresh dispatch scope. This method
    /// does not claim work or hold an audit lock across execution.
    pub fn is_dispatchable_receipt(
        &self,
        receipt: &DurableIntentReceipt,
    ) -> Result<bool, InvocationAuditError> {
        self.ensure_usable()?;
        Ok(receipt.audit_id == self.data.audit_id
            && self.projection.commands.get(&receipt.command.command_id)
                == Some(&InvocationAuditCommand::Intent(receipt.command.clone()))
            && self
                .projection
                .invocations
                .get(receipt.invocation_id())
                .is_some_and(|item| item.revision == 1 && item.final_evidence.is_none()))
    }

    /// Latest durable evidence, independent of closure-time turn projections.
    /// Absence means unrecorded here; it never proves an older or external
    /// executor performed no effect and never authorizes a replay.
    pub fn invocation(
        &self,
        invocation_id: &InvocationId,
    ) -> Result<Option<&AuditedInvocation>, InvocationAuditError> {
        self.ensure_usable()?;
        Ok(self.projection.invocations.get(invocation_id))
    }

    pub fn records(&self) -> Result<&[InvocationAuditRecord], InvocationAuditError> {
        self.ensure_usable()?;
        Ok(&self.data.records)
    }

    pub(crate) fn canonical_identity(
        &self,
    ) -> Result<Option<&DurableSessionIdentity>, InvocationAuditError> {
        self.ensure_usable()?;
        Ok(self
            .namespace
            .as_ref()
            .map(OwnedExecutionNamespace::identity))
    }

    fn ensure_usable(&self) -> Result<(), InvocationAuditError> {
        if self.recovery_required {
            return Err(InvocationAuditError::RecoveryRequired);
        }
        if let Some(namespace) = &self.namespace {
            namespace.verify_ambient_identity()?;
        }
        self.dir.verify_ambient_identity()?;
        Ok(())
    }

    fn append(&mut self, command: InvocationAuditCommand) -> Result<(), InvocationAuditError> {
        self.append_with(command, |dir, bytes| dir.atomic_write(FILE_NAME, bytes))
    }

    fn append_with(
        &mut self,
        command: InvocationAuditCommand,
        write: impl FnOnce(&SecureDir, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), InvocationAuditError> {
        self.ensure_usable()?;
        let mut projection = self.projection.clone();
        let Some(invocation_revision) = apply_command(&self.data.owner, &mut projection, &command)?
        else {
            return Ok(());
        };
        let record = InvocationAuditRecord {
            sequence: self.data.records.len() as u64 + 1,
            invocation_revision,
            command,
        };
        if serde_json::to_vec(&record)?.len() > MAX_RECORD_BYTES {
            return Err(InvocationAuditError::Capacity);
        }
        let mut next = self.data.clone();
        next.records.push(record);
        let bytes = serde_json::to_vec(&next)?;
        validate_capacity(&next, &projection, bytes.len())?;
        if let Err(error) = write(&self.dir, &bytes) {
            // Publication may have succeeded. No acknowledgement, stale read,
            // or retry may escape until reopen has made observed state durable.
            self.recovery_required = true;
            return Err(error.into());
        }
        self.data = next;
        self.projection = projection;
        Ok(())
    }
}

fn apply_command(
    owner: &InvocationAuditOwner,
    projection: &mut AuditProjection,
    command: &InvocationAuditCommand,
) -> Result<Option<u64>, InvocationAuditError> {
    validate_command(owner, command)?;
    if let Some(previous) = projection.commands.get(command.command_id()) {
        return if previous == command {
            Ok(None)
        } else {
            Err(InvocationAuditError::CommandConflict)
        };
    }
    let revision = match command {
        InvocationAuditCommand::Intent(command) => {
            if projection
                .invocations
                .contains_key(&command.intent.invocation_id)
            {
                return Err(InvocationAuditError::IntentConflict);
            }
            if command.expected_revision != 0 {
                return Err(InvocationAuditError::StaleRevision {
                    expected: command.expected_revision,
                    actual: 0,
                });
            }
            projection.invocations.insert(
                command.intent.invocation_id.clone(),
                AuditedInvocation {
                    intent: command.intent.clone(),
                    revision: 1,
                    final_evidence: None,
                },
            );
            1
        }
        InvocationAuditCommand::Evidence(command) => {
            let invocation = projection
                .invocations
                .get_mut(&command.invocation_id)
                .ok_or(InvocationAuditError::NotFound)?;
            if command.activation != invocation.intent.activation
                || command.authority != invocation.intent.authority
            {
                return Err(InvocationAuditError::TargetConflict);
            }
            if command.expected_revision != invocation.revision {
                return Err(InvocationAuditError::StaleRevision {
                    expected: command.expected_revision,
                    actual: invocation.revision,
                });
            }
            if invocation.final_evidence.is_some() {
                return Err(InvocationAuditError::EvidenceConflict);
            }
            invocation.final_evidence = Some(command.evidence.clone());
            invocation.revision = 2;
            2
        }
    };
    projection
        .commands
        .insert(command.command_id().clone(), command.clone());
    Ok(Some(revision))
}

fn rebuild(data: &AuditData) -> Result<AuditProjection, InvocationAuditError> {
    if data.schema_version != SCHEMA_VERSION {
        return Err(InvocationAuditError::UnsupportedVersion(
            data.schema_version,
        ));
    }
    validate_owner(&data.owner)?;
    uuid::Uuid::parse_str(&data.audit_id)
        .map_err(|_| InvocationAuditError::Invalid("invalid audit identity"))?;
    if data.records.len() > MAX_RECORDS {
        return Err(InvocationAuditError::Capacity);
    }
    let mut projection = AuditProjection::default();
    for (index, record) in data.records.iter().enumerate() {
        if record.sequence != index as u64 + 1
            || serde_json::to_vec(record)?.len() > MAX_RECORD_BYTES
        {
            return Err(InvocationAuditError::Invalid(
                "invalid record sequence or size",
            ));
        }
        let revision = apply_command(&data.owner, &mut projection, &record.command)?;
        if revision != Some(record.invocation_revision) {
            return Err(InvocationAuditError::Invalid(
                "duplicate record or revision mismatch",
            ));
        }
    }
    validate_capacity(data, &projection, serde_json::to_vec(data)?.len())?;
    Ok(projection)
}

fn validate_capacity(
    data: &AuditData,
    projection: &AuditProjection,
    serialized_size: usize,
) -> Result<(), InvocationAuditError> {
    let unresolved = projection
        .invocations
        .values()
        .filter(|item| item.final_evidence.is_none())
        .count();
    // Every admitted intent reserves a whole bounded terminal record, including
    // its comma. Capacity cannot prevent late evidence for acknowledged work.
    if projection.invocations.len() > MAX_INVOCATIONS
        || data.records.len() + unresolved > MAX_RECORDS
        || serialized_size + unresolved * (MAX_RECORD_BYTES + 1) > MAX_STORE_BYTES
    {
        return Err(InvocationAuditError::Capacity);
    }
    Ok(())
}

fn validate_owner(owner: &InvocationAuditOwner) -> Result<(), InvocationAuditError> {
    bounded_id(&owner.workspace_id)
}

fn validate_command(
    owner: &InvocationAuditOwner,
    command: &InvocationAuditCommand,
) -> Result<(), InvocationAuditError> {
    let (activation, authority) = match command {
        InvocationAuditCommand::Intent(command) => {
            let intent = &command.intent;
            bounded_id(&intent.dispatch_scope)?;
            bounded_text(&intent.tool_name, 256)?;
            validate_blob(&intent.arguments)?;
            validate_preview(&intent.redacted_preview)?;
            bounded_id(&intent.provider_replay.adapter_id)?;
            bounded_text(&intent.provider_replay.adapter_version, 128)?;
            for value in [
                &intent.provider_replay.native_call_id,
                &intent.provider_replay.response_group_id,
            ]
            .into_iter()
            .flatten()
            {
                bounded_text(value, 512)?;
            }
            (&intent.activation, &intent.authority)
        }
        InvocationAuditCommand::Evidence(command) => {
            if let InvocationFinalEvidence::Outcome {
                result,
                redacted_preview,
                ..
            } = &command.evidence
            {
                validate_blob(result)?;
                validate_preview(redacted_preview)?;
            }
            (&command.activation, &command.authority)
        }
    };
    if activation.session_id != owner.session_id {
        return Err(InvocationAuditError::OwnerConflict);
    }
    if activation.generation == 0 {
        return Err(InvocationAuditError::Invalid("zero activation generation"));
    }
    bounded_id(&authority.grant_id)?;
    if authority.grant_revision == 0 {
        return Err(InvocationAuditError::Invalid("zero grant revision"));
    }
    if serde_json::to_vec(command)?.len() > MAX_COMMAND_BYTES {
        return Err(InvocationAuditError::Invalid("command exceeds byte bound"));
    }
    Ok(())
}

fn validate_blob(blob: &ProtectedArguments) -> Result<(), InvocationAuditError> {
    if blob.byte_len > MAX_PROTECTED_BYTES
        || blob.sha256.len() != 64
        || !blob
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(InvocationAuditError::Invalid(
            "invalid protected evidence metadata",
        ));
    }
    Ok(())
}

fn validate_preview(value: &str) -> Result<(), InvocationAuditError> {
    if value.len() > 2048 {
        return Err(InvocationAuditError::Invalid("preview exceeds byte bound"));
    }
    Ok(())
}

fn bounded_text(value: &str, max: usize) -> Result<(), InvocationAuditError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(InvocationAuditError::Invalid("invalid bounded text"));
    }
    Ok(())
}

fn bounded_id(value: &str) -> Result<(), InvocationAuditError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(InvocationAuditError::Invalid("invalid authority identity"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> InvocationAuditOwner {
        InvocationAuditOwner {
            workspace_id: "workspace-a".into(),
            session_id: SessionId::new("session-a").unwrap(),
        }
    }

    fn intent() -> InvocationIntentCommand {
        serde_json::from_str(include_str!(
            "../tests/fixtures/invocation-audit-intent.v1.json"
        ))
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn failure_after_publication_requires_durable_reopen_before_receipt() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let command = intent();
        let error = audit.append_with(
            InvocationAuditCommand::Intent(command.clone()),
            |dir, bytes| {
                dir.atomic_write(FILE_NAME, bytes)?;
                Err(std::io::Error::other(
                    "injected uncertain return after publication",
                ))
            },
        );
        assert!(matches!(error, Err(InvocationAuditError::Io(_))));
        assert!(matches!(
            audit.record_intent(command.clone()),
            Err(InvocationAuditError::RecoveryRequired)
        ));
        assert!(matches!(
            audit.records(),
            Err(InvocationAuditError::RecoveryRequired)
        ));
        let prior_inode = std::fs::metadata(audit.path()).unwrap().ino();
        drop(audit);
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        assert_ne!(std::fs::metadata(audit.path()).unwrap().ino(), prior_inode);
        let receipt = audit.record_intent(command).unwrap();
        assert!(audit.is_dispatchable_receipt(&receipt).unwrap());
        assert_eq!(audit.records().unwrap().len(), 1);
    }

    #[test]
    fn byte_capacity_reserves_late_evidence_and_does_not_evict_unresolved_intents() {
        let dir = tempfile::tempdir().unwrap();
        let audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let mut data = audit.data.clone();
        let path = audit.path();
        drop(audit);
        let mut large = intent();
        large.intent.redacted_preview = "\u{1}".repeat(2048);
        large.intent.tool_name = "t".repeat(256);
        large.intent.dispatch_scope = "d".repeat(128);
        large.intent.arguments.evidence_ref = EvidenceRef::new("a".repeat(128)).unwrap();
        large.intent.authority.grant_id = "g".repeat(128);
        large.intent.authority.approval_ref = Some(EvidenceRef::new("p".repeat(128)).unwrap());
        large.intent.provider_replay.adapter_id = "i".repeat(128);
        large.intent.provider_replay.adapter_version = "v".repeat(128);
        large.intent.provider_replay.provider_run_ref =
            Some(EvidenceRef::new("r".repeat(128)).unwrap());
        large.intent.provider_replay.native_call_id = Some("n".repeat(512));
        large.intent.provider_replay.response_group_id = Some("s".repeat(512));
        large.intent.replay_policy = InvocationReplayPolicy::ProviderIdempotency {
            policy_ref: EvidenceRef::new("q".repeat(128)).unwrap(),
            key_ref: EvidenceRef::new("k".repeat(128)).unwrap(),
        };
        let mut projection = AuditProjection::default();
        for i in 0..MAX_INVOCATIONS {
            let mut next = data.clone();
            let mut candidate = large.clone();
            candidate.command_id = CommandId::new(format!("command-{i}")).unwrap();
            candidate.intent.invocation_id = InvocationId::new(format!("invocation-{i}")).unwrap();
            let command = InvocationAuditCommand::Intent(candidate);
            let mut next_projection = projection.clone();
            apply_command(&data.owner, &mut next_projection, &command).unwrap();
            next.records.push(InvocationAuditRecord {
                sequence: i as u64 + 1,
                invocation_revision: 1,
                command,
            });
            if validate_capacity(
                &next,
                &next_projection,
                serde_json::to_vec(&next).unwrap().len(),
            )
            .is_err()
            {
                break;
            }
            data = next;
            projection = next_projection;
        }
        assert!(data.records.len() > 1 && data.records.len() < MAX_INVOCATIONS);
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let before = audit.records().unwrap().to_vec();
        assert!(matches!(
            audit.record_intent(large.clone()),
            Err(InvocationAuditError::Capacity)
        ));
        assert_eq!(audit.records().unwrap(), before);
        let InvocationAuditCommand::Intent(first) = &before[0].command else {
            panic!("expected intent")
        };
        audit
            .record_evidence(InvocationEvidenceCommand {
                command_id: CommandId::new("late-outcome").unwrap(),
                expected_revision: 1,
                invocation_id: first.intent.invocation_id.clone(),
                activation: first.intent.activation.clone(),
                authority: first.intent.authority.clone(),
                evidence: InvocationFinalEvidence::Outcome {
                    outcome: InvocationOutcome::Failed,
                    result: ProtectedArguments {
                        evidence_ref: EvidenceRef::new("z".repeat(128)).unwrap(),
                        sha256: "f".repeat(64),
                        byte_len: MAX_PROTECTED_BYTES,
                    },
                    redacted_preview: "\u{1}".repeat(2048),
                    source: InvocationOutcomeSource::Executor,
                    authority_ref: EvidenceRef::new("o".repeat(128)).unwrap(),
                },
            })
            .unwrap();
        assert_eq!(&audit.records().unwrap()[..before.len()], before);
        drop(audit);
        let audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        assert_eq!(audit.records().unwrap().len(), before.len() + 1);
    }
}
