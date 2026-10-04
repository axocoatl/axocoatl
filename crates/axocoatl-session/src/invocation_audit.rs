//! Durable invocation evidence, independent of logical-turn lifecycle.
//!
//! Events are append-only: each is one synced line of a segment log whose
//! sealed segments are hash-chained (see `segment_log`), so a Session can
//! record any number of invocations over its life. Late authoritative evidence
//! may settle an invocation after its turn closes; this store never changes
//! turn state or accepted output.
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
use crate::segment_log::{
    KeyFilter, SegmentCache, SegmentError, SegmentLog, SegmentSpec, SegmentsMarker,
};
use crate::turn_contract::{
    ActivationRef, CommandId, EffectDisposition, EvidenceRef, InvocationId, InvocationOutcome,
    SessionId,
};

const SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "invocation-audit.v1.json";
/// Bounds of the single-file layout written before segmentation, which such
/// a file still meets when it is read and migrated.
const LEGACY_MAX_RECORDS: usize = 512;
const LEGACY_MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_RECORD_BYTES: usize = 17 * 1024;
const MAX_PROTECTED_BYTES: u64 = 16 * 1024 * 1024;
/// The records live in a segment log beside the head file; a Session can
/// record any number of invocations over its life.
const SPEC: SegmentSpec = SegmentSpec {
    name: "invocation-audit",
    kind: "invocation-audit",
    segment_bytes: 1024 * 1024,
    // Unit tests seal often so that every path crosses segments.
    segment_records: if cfg!(test) { 8 } else { 1024 },
    // A record line wraps the record in a short frame.
    record_bytes: MAX_RECORD_BYTES + 64,
};
const CACHED_SEGMENTS: usize = 4;

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
    #[error("invocation audit segments: {0}")]
    Segment(String),
}

impl From<SegmentError> for InvocationAuditError {
    fn from(error: SegmentError) -> Self {
        match error {
            SegmentError::Io(error) => Self::Io(error),
            SegmentError::Json(error) => Self::Json(error),
            SegmentError::RecoveryRequired => Self::RecoveryRequired,
            SegmentError::RecordTooLarge => Self::Capacity,
            error => Self::Segment(error.to_string()),
        }
    }
}

/// The single-file layout written before segmentation. A store still in it
/// is validated with its old bounds and migrated on open.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAuditData {
    schema_version: u32,
    audit_id: String,
    owner: InvocationAuditOwner,
    records: Vec<InvocationAuditRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical_journal_id: Option<String>,
}

/// The primary file of a segmented audit: its identity only. The records
/// are in the segment log beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditHead {
    schema_version: u32,
    audit_id: String,
    owner: InvocationAuditOwner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical_journal_id: Option<String>,
    segments: SegmentsMarker,
}

impl AuditHead {
    fn meta(&self) -> serde_json::Value {
        serde_json::json!({
            "audit_id": self.audit_id,
            "owner": self.owner,
            "canonical_journal_id": self.canonical_journal_id,
        })
    }
}

enum StoredAudit {
    Head(AuditHead),
    Legacy(LegacyAuditData),
}

fn parse_stored(bytes: &[u8]) -> Result<StoredAudit, InvocationAuditError> {
    #[derive(Deserialize)]
    struct Header {
        schema_version: u32,
        #[serde(default)]
        segments: Option<serde::de::IgnoredAny>,
    }
    let header: Header = serde_json::from_slice(bytes)?;
    if header.schema_version != SCHEMA_VERSION {
        return Err(InvocationAuditError::UnsupportedVersion(
            header.schema_version,
        ));
    }
    Ok(if header.segments.is_some() {
        let head: AuditHead = serde_json::from_slice(bytes)?;
        if !head.segments.matches(&SPEC) {
            return Err(InvocationAuditError::Invalid("unsupported audit segments"));
        }
        StoredAudit::Head(head)
    } else {
        StoredAudit::Legacy(serde_json::from_slice(bytes)?)
    })
}

/// The keys a record is found by in a sealed segment.
fn record_keys(record: &InvocationAuditRecord) -> Vec<String> {
    match &record.command {
        InvocationAuditCommand::Intent(command) => vec![
            format!("cmd:{}", command.command_id.as_str()),
            format!("intent:{}", command.intent.invocation_id.as_str()),
            format!("inv:{}", command.intent.invocation_id.as_str()),
            format!("turn:{}", command.intent.activation.turn_id.as_str()),
        ],
        InvocationAuditCommand::Evidence(command) => vec![
            format!("cmd:{}", command.command_id.as_str()),
            format!("inv:{}", command.invocation_id.as_str()),
            format!("turn:{}", command.activation.turn_id.as_str()),
        ],
    }
}

/// Keys that one record may hold only once in the whole history.
fn unique_keys(record: &InvocationAuditRecord) -> impl Iterator<Item = String> {
    record_keys(record)
        .into_iter()
        .filter(|key| key.starts_with("cmd:") || key.starts_with("intent:"))
}

/// One single-writer audit for an explicitly owned Session. The host must
/// supply an existing, durably provisioned private control-plane directory
/// outside the repository mount. No resolved or unresolved history is
/// evicted: records are kept in a segment log, and memory holds only the
/// active segment, unresolved intents and one key filter per sealed segment.
pub struct InvocationAudit {
    dir: SecureDir,
    namespace: Option<OwnedExecutionNamespace>,
    head: AuditHead,
    log: SegmentLog,
    /// Records of the active segment, in order.
    active: Vec<InvocationAuditRecord>,
    /// One filter per sealed segment, in order.
    filters: Vec<KeyFilter>,
    cache: SegmentCache<InvocationAuditRecord>,
    /// Every intent without final evidence, wherever it was recorded.
    unresolved: HashMap<InvocationId, InvocationIntentCommand>,
    recovery_required: bool,
    #[cfg(test)]
    fail_next_append: bool,
}

impl InvocationAudit {
    /// Read existing, identity-checked audit evidence for one turn without
    /// opening a writer, acknowledging an uncertain write, or minting a
    /// dispatch receipt.
    pub fn read_retained_views(
        canonical: &crate::execution_store::SessionExecutionStore,
        turn_id: &crate::turn_contract::LogicalTurnId,
    ) -> Result<Vec<AuditedInvocation>, InvocationAuditError> {
        let snapshot = canonical
            .snapshot(turn_id)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let root = canonical
            .existing_component_root(&ExecutionComponent::InvocationAudit, Path::new(FILE_NAME))
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let bytes = root.read_limited(FILE_NAME, LEGACY_MAX_STORE_BYTES)?;
        let (owner, canonical_journal_id) = (
            &snapshot.owner().workspace_id,
            Some(snapshot.journal_id().to_owned()),
        );
        let check = |audit_owner: &InvocationAuditOwner, journal: &Option<String>| {
            if &audit_owner.workspace_id != owner
                || audit_owner.session_id != snapshot.owner().session_id
                || journal != &canonical_journal_id
            {
                return Err(InvocationAuditError::OwnerConflict);
            }
            Ok(())
        };
        let mut views = Vec::new();
        match parse_stored(&bytes)? {
            StoredAudit::Legacy(data) => {
                check(&data.owner, &data.canonical_journal_id)?;
                let mut turn = TurnInvocations::default();
                validate_legacy(&data)?;
                for record in data.records {
                    turn.add(record, turn_id);
                }
                views.extend(turn.finish()?);
            }
            StoredAudit::Head(head) => {
                check(&head.owner, &head.canonical_journal_id)?;
                let turn = std::cell::RefCell::new(TurnInvocations::default());
                SegmentLog::read(
                    root,
                    SPEC,
                    head.meta(),
                    || *turn.borrow_mut() = TurnInvocations::default(),
                    |sequence, record: InvocationAuditRecord| {
                        if record.sequence != sequence {
                            return Err(InvocationAuditError::Invalid("record sequence mismatch"));
                        }
                        turn.borrow_mut().add(record, turn_id);
                        Ok(())
                    },
                )?;
                views.extend(turn.into_inner().finish()?);
            }
        }
        Ok(views)
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
        let head = match dir.read_limited(FILE_NAME, LEGACY_MAX_STORE_BYTES) {
            Ok(bytes) => match parse_stored(&bytes)? {
                StoredAudit::Head(head) => head,
                StoredAudit::Legacy(data) => {
                    if data.owner != owner || data.canonical_journal_id != canonical_journal_id {
                        return Err(InvocationAuditError::OwnerConflict);
                    }
                    validate_legacy(&data)?;
                    migrate_legacy(&dir, namespace.as_ref(), data)?
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(namespace) = &namespace {
                    namespace.check_journal_creation(FILE_NAME)?;
                } else if SegmentLog::exists(&dir, &SPEC)? {
                    return Err(InvocationAuditError::Invalid(
                        "audit segments exist without their head",
                    ));
                }
                let head = AuditHead {
                    schema_version: SCHEMA_VERSION,
                    audit_id: uuid::Uuid::new_v4().to_string(),
                    owner: owner.clone(),
                    canonical_journal_id: canonical_journal_id.clone(),
                    segments: SegmentsMarker::of(&SPEC),
                };
                if let Some(namespace) = &namespace {
                    namespace.mark_journal_initialized(FILE_NAME)?;
                }
                SegmentLog::open(
                    dir.clone(),
                    SPEC,
                    head.meta(),
                    true,
                    |_, _: InvocationAuditRecord| Ok::<(), InvocationAuditError>(()),
                )?;
                dir.verify_ambient_identity()?;
                dir.atomic_write(FILE_NAME, &serde_json::to_vec(&head)?)?;
                head
            }
            Err(error) => return Err(error.into()),
        };
        if head.owner != owner || head.canonical_journal_id != canonical_journal_id {
            return Err(InvocationAuditError::OwnerConflict);
        }
        uuid::Uuid::parse_str(&head.audit_id)
            .map_err(|_| InvocationAuditError::Invalid("invalid audit identity"))?;
        if let Some(namespace) = &namespace {
            namespace.mark_journal_initialized(FILE_NAME)?;
        }
        let mut loader = Loader::default();
        let log = SegmentLog::open_indexed(
            dir.clone(),
            SPEC,
            head.meta(),
            false,
            |segment, sequence, record: InvocationAuditRecord| {
                loader.visit(&head.owner, segment, sequence, record)
            },
        )?;
        let (active, filters, unresolved, suspects) = loader.finish(&log);
        let audit = Self {
            dir,
            namespace,
            head,
            log,
            active,
            filters,
            cache: SegmentCache::new(CACHED_SEGMENTS),
            unresolved,
            recovery_required: false,
            #[cfg(test)]
            fail_next_append: false,
        };
        // A key a later segment repeats may be a filter's false positive;
        // confirm against the earlier segment itself.
        for (key, segment) in suspects {
            if audit.segment_has_key(segment, &key)? {
                return Err(InvocationAuditError::Invalid(
                    "duplicate command or intent across segments",
                ));
            }
        }
        Ok(audit)
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(FILE_NAME)
    }

    pub fn audit_id(&self) -> Result<&str, InvocationAuditError> {
        self.ensure_usable()?;
        Ok(&self.head.audit_id)
    }

    /// Return only after intent is durable. Exact repeats return its original
    /// acknowledgement, including after an outcome; that does not revive work.
    pub fn record_intent(
        &mut self,
        command: InvocationIntentCommand,
    ) -> Result<DurableIntentReceipt, InvocationAuditError> {
        self.append(InvocationAuditCommand::Intent(command.clone()))?;
        Ok(DurableIntentReceipt {
            audit_id: self.head.audit_id.clone(),
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
        Ok(receipt.audit_id == self.head.audit_id
            && self.unresolved.get(receipt.invocation_id()) == Some(&receipt.command))
    }

    /// Latest durable evidence, independent of closure-time turn projections.
    /// Absence means unrecorded here; it never proves an older or external
    /// executor performed no effect and never authorizes a replay.
    pub fn invocation(
        &self,
        invocation_id: &InvocationId,
    ) -> Result<Option<AuditedInvocation>, InvocationAuditError> {
        self.ensure_usable()?;
        if let Some(command) = self.unresolved.get(invocation_id) {
            return Ok(Some(AuditedInvocation {
                intent: command.intent.clone(),
                revision: 1,
                final_evidence: None,
            }));
        }
        let mut found: Option<AuditedInvocation> = None;
        for record in self.records_with(&format!("inv:{}", invocation_id.as_str()))? {
            match &record.command {
                InvocationAuditCommand::Intent(command)
                    if &command.intent.invocation_id == invocation_id =>
                {
                    found = Some(AuditedInvocation {
                        intent: command.intent.clone(),
                        revision: 1,
                        final_evidence: None,
                    })
                }
                InvocationAuditCommand::Evidence(command)
                    if &command.invocation_id == invocation_id =>
                {
                    if let Some(found) = &mut found {
                        found.final_evidence = Some(command.evidence.clone());
                        found.revision = 2;
                    }
                }
                _ => {}
            }
        }
        Ok(found)
    }

    /// Every invocation whose intent belongs to `turn_id`, in intent order,
    /// with its latest evidence.
    pub fn turn_invocations(
        &self,
        turn_id: &crate::turn_contract::LogicalTurnId,
    ) -> Result<Vec<AuditedInvocation>, InvocationAuditError> {
        self.ensure_usable()?;
        let mut turn = TurnInvocations::default();
        for record in self.records_with(&format!("turn:{}", turn_id.as_str()))? {
            turn.add(record, turn_id);
        }
        turn.finish()
    }

    /// Every invocation still without final evidence, in any turn.
    pub fn unresolved(&self) -> Result<Vec<AuditedInvocation>, InvocationAuditError> {
        self.ensure_usable()?;
        let mut unresolved: Vec<_> = self
            .unresolved
            .values()
            .map(|command| AuditedInvocation {
                intent: command.intent.clone(),
                revision: 1,
                final_evidence: None,
            })
            .collect();
        unresolved.sort_by(|a, b| {
            a.intent
                .invocation_id
                .as_str()
                .cmp(b.intent.invocation_id.as_str())
        });
        Ok(unresolved)
    }

    /// The whole history, read back from every segment. Memory grows with
    /// it, so this is for export and tests, not for live operation.
    pub fn records(&self) -> Result<Vec<InvocationAuditRecord>, InvocationAuditError> {
        self.ensure_usable()?;
        let mut records = Vec::new();
        for segment in self.log.sealed() {
            records.extend(self.log.read_sealed::<InvocationAuditRecord>(segment)?);
        }
        records.extend(self.active.iter().cloned());
        Ok(records)
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

    /// Sealed segments and the bytes of memory their key filters hold.
    pub fn sealed_segments(&self) -> (usize, usize) {
        (
            self.filters.len(),
            self.filters.iter().map(KeyFilter::bytes).sum(),
        )
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

    /// Records holding `key`, oldest first: from the sealed segments its
    /// filters admit, then the active segment.
    fn records_with(&self, key: &str) -> Result<Vec<InvocationAuditRecord>, InvocationAuditError> {
        let mut records = Vec::new();
        for (segment, filter) in self.log.sealed().iter().zip(&self.filters) {
            if !filter.may_contain(key) {
                continue;
            }
            for record in self.cache.get(&self.log, segment)?.iter() {
                if record_keys(record).iter().any(|held| held == key) {
                    records.push(record.as_ref().clone());
                }
            }
        }
        records.extend(
            self.active
                .iter()
                .filter(|record| record_keys(record).iter().any(|held| held == key))
                .cloned(),
        );
        Ok(records)
    }

    fn segment_has_key(&self, index: u64, key: &str) -> Result<bool, InvocationAuditError> {
        let segment = &self.log.sealed()[index as usize];
        Ok(self
            .cache
            .get(&self.log, segment)?
            .iter()
            .any(|record| record_keys(record).iter().any(|held| held == key)))
    }

    /// The command already recorded under `id`, anywhere in the history.
    fn command(
        &self,
        id: &CommandId,
    ) -> Result<Option<InvocationAuditCommand>, InvocationAuditError> {
        Ok(self
            .records_with(&format!("cmd:{}", id.as_str()))?
            .into_iter()
            .map(|record| record.command)
            .find(|command| command.command_id() == id))
    }

    /// The revision `command` would create, `None` for an exact repeat, or
    /// why it is refused. Nothing is written.
    fn admit(&self, command: &InvocationAuditCommand) -> Result<Option<u64>, InvocationAuditError> {
        validate_command(&self.head.owner, command)?;
        if let Some(previous) = self.command(command.command_id())? {
            return if &previous == command {
                Ok(None)
            } else {
                Err(InvocationAuditError::CommandConflict)
            };
        }
        match command {
            InvocationAuditCommand::Intent(command) => {
                let id = &command.intent.invocation_id;
                if self.unresolved.contains_key(id)
                    || !self
                        .records_with(&format!("intent:{}", id.as_str()))?
                        .is_empty()
                {
                    return Err(InvocationAuditError::IntentConflict);
                }
                if command.expected_revision != 0 {
                    return Err(InvocationAuditError::StaleRevision {
                        expected: command.expected_revision,
                        actual: 0,
                    });
                }
                Ok(Some(1))
            }
            InvocationAuditCommand::Evidence(command) => {
                let invocation = self
                    .invocation(&command.invocation_id)?
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
                Ok(Some(2))
            }
        }
    }

    fn append(&mut self, command: InvocationAuditCommand) -> Result<(), InvocationAuditError> {
        self.ensure_usable()?;
        let Some(invocation_revision) = self.admit(&command)? else {
            return Ok(());
        };
        let record = InvocationAuditRecord {
            sequence: self.log.next_sequence(),
            invocation_revision,
            command,
        };
        if serde_json::to_vec(&record)?.len() > MAX_RECORD_BYTES {
            return Err(InvocationAuditError::Capacity);
        }
        let line = self
            .log
            .encode_record(&record)
            .map_err(|error| match error {
                SegmentError::RecordTooLarge => InvocationAuditError::Capacity,
                error => error.into(),
            })?;
        if let Err(error) = self.log.append_line(&line) {
            // Publication may have succeeded. No acknowledgement, stale read,
            // or retry may escape until reopen has made observed state durable.
            self.recovery_required = true;
            return Err(error.into());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_append) {
            self.recovery_required = true;
            return Err(
                std::io::Error::other("injected uncertain return after publication").into(),
            );
        }
        match &record.command {
            InvocationAuditCommand::Intent(command) => {
                self.unresolved
                    .insert(command.intent.invocation_id.clone(), command.clone());
            }
            InvocationAuditCommand::Evidence(command) => {
                self.unresolved.remove(&command.invocation_id);
            }
        }
        self.active.push(record);
        if self.log.should_seal() {
            let filter = active_filter(&self.active);
            if self.log.seal().is_err() {
                // The record is durable; the seal is completed on reopen.
                self.recovery_required = true;
                return Ok(());
            }
            self.filters.push(filter);
            self.active.clear();
        }
        Ok(())
    }
}

fn active_filter(records: &[InvocationAuditRecord]) -> KeyFilter {
    let keys: Vec<String> = records.iter().flat_map(record_keys).collect();
    KeyFilter::new(keys.iter().map(String::as_str))
}

/// Builds the in-memory state while a segmented audit is opened, one
/// segment at a time.
#[derive(Default)]
struct Loader {
    segment: u64,
    records: Vec<InvocationAuditRecord>,
    seen: std::collections::HashSet<String>,
    filters: Vec<KeyFilter>,
    unresolved: HashMap<InvocationId, InvocationIntentCommand>,
    suspects: Vec<(String, u64)>,
}

impl Loader {
    fn visit(
        &mut self,
        owner: &InvocationAuditOwner,
        segment: u64,
        sequence: u64,
        record: InvocationAuditRecord,
    ) -> Result<(), InvocationAuditError> {
        if segment != self.segment {
            self.close_segment();
            self.segment = segment;
        }
        validate_command(owner, &record.command)?;
        if record.sequence != sequence || serde_json::to_vec(&record)?.len() > MAX_RECORD_BYTES {
            return Err(InvocationAuditError::Invalid(
                "invalid record sequence or size",
            ));
        }
        for key in unique_keys(&record) {
            if !self.seen.insert(key.clone()) {
                return Err(InvocationAuditError::Invalid(
                    "duplicate command or intent in a segment",
                ));
            }
            for (earlier, filter) in self.filters.iter().enumerate() {
                if filter.may_contain(&key) {
                    self.suspects.push((key.clone(), earlier as u64));
                }
            }
        }
        let revision = match &record.command {
            InvocationAuditCommand::Intent(command) => {
                if command.expected_revision != 0 {
                    return Err(InvocationAuditError::Invalid("intent revision mismatch"));
                }
                self.unresolved
                    .insert(command.intent.invocation_id.clone(), command.clone());
                1
            }
            InvocationAuditCommand::Evidence(command) => {
                let intent = self.unresolved.remove(&command.invocation_id).ok_or(
                    InvocationAuditError::Invalid("evidence for an unknown or settled invocation"),
                )?;
                if command.activation != intent.intent.activation
                    || command.authority != intent.intent.authority
                    || command.expected_revision != 1
                {
                    return Err(InvocationAuditError::Invalid(
                        "evidence differs from its intent",
                    ));
                }
                2
            }
        };
        if record.invocation_revision != revision {
            return Err(InvocationAuditError::Invalid(
                "duplicate record or revision mismatch",
            ));
        }
        self.records.push(record);
        Ok(())
    }

    fn close_segment(&mut self) {
        let records = std::mem::take(&mut self.records);
        self.filters.push(active_filter(&records));
        self.seen.clear();
    }

    /// The active segment's records, every sealed segment's filter, the
    /// unresolved intents, and keys to confirm against earlier segments.
    #[allow(clippy::type_complexity)]
    fn finish(
        mut self,
        log: &SegmentLog,
    ) -> (
        Vec<InvocationAuditRecord>,
        Vec<KeyFilter>,
        HashMap<InvocationId, InvocationIntentCommand>,
        Vec<(String, u64)>,
    ) {
        // Records seen last belong to the active segment unless it is empty.
        let active = if self.segment == log.active_index() {
            std::mem::take(&mut self.records)
        } else {
            if !self.records.is_empty() {
                self.close_segment();
            }
            vec![]
        };
        // A sealed segment always holds records, so every one was visited.
        debug_assert_eq!(self.filters.len(), log.sealed().len());
        (active, self.filters, self.unresolved, self.suspects)
    }
}

/// Collects one turn's invocations from records read in order.
#[derive(Default)]
struct TurnInvocations {
    invocations: Vec<AuditedInvocation>,
}

impl TurnInvocations {
    fn add(
        &mut self,
        record: InvocationAuditRecord,
        turn_id: &crate::turn_contract::LogicalTurnId,
    ) {
        match record.command {
            InvocationAuditCommand::Intent(command)
                if &command.intent.activation.turn_id == turn_id =>
            {
                self.invocations.push(AuditedInvocation {
                    intent: command.intent,
                    revision: 1,
                    final_evidence: None,
                });
            }
            InvocationAuditCommand::Evidence(command) => {
                if let Some(invocation) = self
                    .invocations
                    .iter_mut()
                    .find(|item| item.intent.invocation_id == command.invocation_id)
                {
                    invocation.final_evidence = Some(command.evidence);
                    invocation.revision = 2;
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Result<Vec<AuditedInvocation>, InvocationAuditError> {
        Ok(self.invocations)
    }
}

/// Validate a single-file audit with the rules it was written under.
fn validate_legacy(data: &LegacyAuditData) -> Result<(), InvocationAuditError> {
    if data.schema_version != SCHEMA_VERSION {
        return Err(InvocationAuditError::UnsupportedVersion(
            data.schema_version,
        ));
    }
    validate_owner(&data.owner)?;
    uuid::Uuid::parse_str(&data.audit_id)
        .map_err(|_| InvocationAuditError::Invalid("invalid audit identity"))?;
    if data.records.len() > LEGACY_MAX_RECORDS {
        return Err(InvocationAuditError::Capacity);
    }
    let mut loader = Loader::default();
    for (index, record) in data.records.iter().enumerate() {
        loader.visit(&data.owner, 0, index as u64 + 1, record.clone())?;
    }
    Ok(())
}

/// Move a single-file audit's records into a segment log and replace its
/// file with the head. A crash before the head is written leaves the old
/// file in place, and the next open converts it again from the start.
fn migrate_legacy(
    dir: &SecureDir,
    namespace: Option<&OwnedExecutionNamespace>,
    data: LegacyAuditData,
) -> Result<AuditHead, InvocationAuditError> {
    let head = AuditHead {
        schema_version: SCHEMA_VERSION,
        audit_id: data.audit_id,
        owner: data.owner,
        canonical_journal_id: data.canonical_journal_id,
        segments: SegmentsMarker::of(&SPEC),
    };
    SegmentLog::remove(dir, &SPEC)?;
    let mut log = SegmentLog::open(
        dir.clone(),
        SPEC,
        head.meta(),
        true,
        |_, _: InvocationAuditRecord| Ok::<(), InvocationAuditError>(()),
    )?;
    for record in &data.records {
        log.append_line(&log.encode_record(record)?)?;
        if log.should_seal() {
            log.seal()?;
        }
    }
    drop(log);
    if let Some(namespace) = namespace {
        namespace.mark_journal_initialized(FILE_NAME)?;
    }
    dir.verify_ambient_identity()?;
    dir.atomic_write(FILE_NAME, &serde_json::to_vec(&head)?)?;
    Ok(head)
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

    fn numbered(n: usize) -> InvocationIntentCommand {
        let mut command = intent();
        command.command_id = CommandId::new(format!("intent-{n}")).unwrap();
        command.intent.invocation_id = InvocationId::new(format!("invocation-{n}")).unwrap();
        command
    }

    fn outcome(command: &InvocationIntentCommand, n: usize) -> InvocationEvidenceCommand {
        InvocationEvidenceCommand {
            command_id: CommandId::new(format!("outcome-{n}")).unwrap(),
            expected_revision: 1,
            invocation_id: command.intent.invocation_id.clone(),
            activation: command.intent.activation.clone(),
            authority: command.intent.authority.clone(),
            evidence: InvocationFinalEvidence::Outcome {
                outcome: InvocationOutcome::Succeeded,
                result: ProtectedArguments {
                    evidence_ref: EvidenceRef::new(format!("result-{n}")).unwrap(),
                    sha256: "f".repeat(64),
                    byte_len: 2,
                },
                redacted_preview: "ok".into(),
                source: InvocationOutcomeSource::Executor,
                authority_ref: EvidenceRef::new("authority").unwrap(),
            },
        }
    }

    #[test]
    fn an_uncertain_append_requires_reopening_and_its_record_is_then_durable() {
        let dir = tempfile::tempdir().unwrap();
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let command = intent();
        // The line reached storage, but the write reported an error.
        audit.fail_next_append = true;
        assert!(matches!(
            audit.record_intent(command.clone()),
            Err(InvocationAuditError::Io(_))
        ));
        assert!(matches!(
            audit.record_intent(command.clone()),
            Err(InvocationAuditError::RecoveryRequired)
        ));
        drop(audit);
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let receipt = audit.record_intent(command).unwrap();
        assert!(audit.is_dispatchable_receipt(&receipt).unwrap());
        assert_eq!(audit.records().unwrap().len(), 1);
    }

    /// More invocations than the old single-file bound of 256, across many
    /// sealed segments: each is found again, by id and by turn, before and
    /// after reopening, and memory holds only the active segment.
    #[test]
    fn invocations_beyond_the_old_bound_are_found_across_sealed_segments() {
        let dir = tempfile::tempdir().unwrap();
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let count = 300;
        for n in 0..count {
            let command = numbered(n);
            audit.record_intent(command.clone()).unwrap();
            if n % 3 != 0 {
                audit.record_evidence(outcome(&command, n)).unwrap();
            }
        }
        let check = |audit: &InvocationAudit| {
            let (sealed, filter_bytes) = audit.sealed_segments();
            assert!(sealed > 60, "{sealed}");
            assert!(audit.active.len() < SPEC.segment_records as usize);
            assert!(filter_bytes < sealed * 64);
            for n in [0, 1, 2, 150, count - 1] {
                let found = audit
                    .invocation(&numbered(n).intent.invocation_id)
                    .unwrap()
                    .unwrap();
                assert_eq!(found.final_evidence.is_some(), n % 3 != 0, "{n}");
            }
            assert!(audit
                .invocation(&InvocationId::new("absent").unwrap())
                .unwrap()
                .is_none());
            let turn = audit
                .turn_invocations(&intent().intent.activation.turn_id)
                .unwrap();
            assert_eq!(turn.len(), count);
            assert_eq!(audit.unresolved().unwrap().len(), count.div_ceil(3));
            assert_eq!(audit.records().unwrap().len(), count + count * 2 / 3);
        };
        check(&audit);
        // Exact repeats are acknowledged without a write; changed ones are
        // refused, however old the original.
        let first = numbered(0);
        let before = audit.records().unwrap().len();
        audit.record_intent(first.clone()).unwrap();
        let mut changed = first.clone();
        changed.intent.tool_name = "other".into();
        assert!(matches!(
            audit.record_intent(changed),
            Err(InvocationAuditError::CommandConflict)
        ));
        let mut reused = numbered(1);
        reused.command_id = CommandId::new("fresh-command").unwrap();
        assert!(matches!(
            audit.record_intent(reused),
            Err(InvocationAuditError::IntentConflict)
        ));
        assert!(matches!(
            audit.record_evidence(outcome(&numbered(1), 9999)),
            Err(InvocationAuditError::StaleRevision {
                expected: 1,
                actual: 2
            })
        ));
        let mut second = outcome(&numbered(1), 9999);
        second.expected_revision = 2;
        assert!(matches!(
            audit.record_evidence(second),
            Err(InvocationAuditError::EvidenceConflict)
        ));
        assert_eq!(audit.records().unwrap().len(), before);
        // Late evidence settles an intent sealed long ago.
        audit.record_evidence(outcome(&first, 0)).unwrap();
        drop(audit);
        let audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        assert_eq!(audit.unresolved().unwrap().len(), count.div_ceil(3) - 1);
        assert!(audit
            .invocation(&first.intent.invocation_id)
            .unwrap()
            .unwrap()
            .final_evidence
            .is_some());
    }

    /// A store written by the single-file layout opens with its history
    /// intact, as a segmented store, and an interrupted conversion is redone.
    #[test]
    fn a_single_file_audit_is_migrated_into_segments() {
        let dir = tempfile::tempdir().unwrap();
        let mut records = vec![];
        for n in 0..20 {
            let command = numbered(n);
            records.push(InvocationAuditRecord {
                sequence: records.len() as u64 + 1,
                invocation_revision: 1,
                command: InvocationAuditCommand::Intent(command.clone()),
            });
            if n % 2 == 0 {
                records.push(InvocationAuditRecord {
                    sequence: records.len() as u64 + 1,
                    invocation_revision: 2,
                    command: InvocationAuditCommand::Evidence(outcome(&command, n)),
                });
            }
        }
        let legacy = LegacyAuditData {
            schema_version: SCHEMA_VERSION,
            audit_id: uuid::Uuid::new_v4().to_string(),
            owner: owner(),
            records: records.clone(),
            canonical_journal_id: None,
        };
        let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, &legacy_bytes).unwrap();
        // A conversion that stopped before its head was written left a
        // partial log behind; it is discarded and the conversion redone.
        {
            let partial = SegmentLog::open(
                SecureDir::open(dir.path()).unwrap(),
                SPEC,
                serde_json::json!({"partial": true}),
                true,
                |_, _: InvocationAuditRecord| Ok::<(), SegmentError>(()),
            )
            .unwrap();
            drop(partial);
        }
        let audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        assert_eq!(audit.records().unwrap(), records);
        assert_eq!(audit.audit_id().unwrap(), legacy.audit_id);
        assert_eq!(audit.unresolved().unwrap().len(), 10);
        drop(audit);
        let head: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(head.get("records").is_none());
        assert_eq!(head["segments"]["kind"], "invocation-audit");
        // An older daemon, which knows only the single-file layout, refuses
        // the head instead of reading part of the history.
        assert!(serde_json::from_slice::<LegacyAuditData>(&std::fs::read(&path).unwrap()).is_err());
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        assert_eq!(audit.records().unwrap(), records);
        audit.record_intent(numbered(100)).unwrap();
        assert_eq!(audit.records().unwrap().len(), records.len() + 1);
    }

    /// History that repeats a command or an intent across sealed segments
    /// is refused on open, though each segment alone is well formed.
    #[test]
    fn a_duplicate_across_segments_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        let meta = audit.head.meta();
        drop(audit);
        let mut log = SegmentLog::open(
            SecureDir::open(dir.path()).unwrap(),
            SPEC,
            meta,
            false,
            |_, _: InvocationAuditRecord| Ok::<(), SegmentError>(()),
        )
        .unwrap();
        for (sequence, n) in (1..).zip((0..10).chain([3])) {
            let record = InvocationAuditRecord {
                sequence,
                invocation_revision: 1,
                command: InvocationAuditCommand::Intent(numbered(n)),
            };
            log.append_line(&log.encode_record(&record).unwrap())
                .unwrap();
            if log.should_seal() {
                log.seal().unwrap();
            }
        }
        drop(log);
        assert!(matches!(
            InvocationAudit::open(dir.path(), owner()),
            Err(InvocationAuditError::Invalid(_))
        ));
    }

    /// A line cut short by a crash was never acknowledged: reopening drops
    /// it and the audit continues.
    #[test]
    fn a_torn_append_is_dropped_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        for n in 0..5 {
            audit.record_intent(numbered(n)).unwrap();
        }
        drop(audit);
        let active = dir.path().join(SPEC.active_name());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(active)
            .unwrap();
        std::io::Write::write_all(&mut file, br#"{"record":{"sequence":6,"#).unwrap();
        drop(file);
        let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
        assert_eq!(audit.records().unwrap().len(), 5);
        audit.record_intent(numbered(5)).unwrap();
        assert_eq!(audit.records().unwrap()[5].sequence, 6);
    }
}
