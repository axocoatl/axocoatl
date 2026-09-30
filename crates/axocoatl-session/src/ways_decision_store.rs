//! Session-owned Ways evidence, pinned before any disposable candidate cleanup.
//!
//! The record and complete protected patch bytes share one atomic publication.
//! This store does not apply patches or authorize cleanup. The existing Keep
//! transaction supplies its actual result, then records exact cleanup receipts.
use std::collections::HashSet;
use std::io::{self, Write};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use crate::turn_contract::EvidenceRef;
use crate::ways_decision::*;

pub const WAYS_ARCHIVE_FILE: &str = "ways-decisions.v1.json";
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WaysArchiveError {
    #[error("Ways evidence storage: {0}")]
    Io(#[from] io::Error),
    #[error("Ways evidence encoding: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Contract(#[from] WaysDecisionError),
    #[error("invalid Ways evidence archive: {0}")]
    Invalid(&'static str),
    #[error("Ways evidence write is uncertain; reopen before cleanup or acknowledgement")]
    RecoveryRequired,
}
type Result<T> = std::result::Result<T, WaysArchiveError>;

/// Opaque artifact identity is derived from the exact protected patch bytes.
/// Original Git object identities remain in the candidate record; their patch
/// body is independent of disposable clones and refs after publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedWaysPatch {
    pub reference: EvidenceRef,
    pub sha256: String,
    pub byte_len: u64,
    pub base64: String,
}
impl PinnedWaysPatch {
    pub fn capture(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(WaysDecisionError::Capacity.into());
        }
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let reference = EvidenceRef::new(format!("ways-patch-{sha256}"))
            .map_err(|_| WaysArchiveError::Invalid("patch identity"))?;
        Ok(Self {
            reference,
            sha256,
            byte_len: bytes.len() as u64,
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }
    pub fn bytes(&self) -> Result<Vec<u8>> {
        if self.base64.len() > MAX_ARCHIVE_BYTES {
            return Err(WaysDecisionError::Capacity.into());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.base64)
            .map_err(|_| WaysArchiveError::Invalid("protected patch encoding"))?;
        if Self::capture(&bytes)? != *self {
            return Err(WaysArchiveError::Invalid(
                "protected patch digest or identity changed",
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletedWaysDecision {
    pub decision_id: DecisionId,
    pub set_id: WaysSetId,
    pub deleted_at_unix_ms: u64,
    pub ownership_receipt: EvidenceRef,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    schema_version: u32,
    journal_id: String,
    owner: ExecutionStoreOwner,
    limits: WaysRetentionLimits,
    records: Vec<WaysDecisionRecord>,
    patches: Vec<PinnedWaysPatch>,
    deleted: Vec<DeletedWaysDecision>,
}

pub struct WaysDecisionStore {
    namespace: OwnedExecutionNamespace,
    archive: Archive,
    poisoned: bool,
}
impl WaysDecisionStore {
    pub fn open_configured(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
    ) -> Result<Option<Self>> {
        namespace.require_root(&ExecutionComponent::WaysDecisions)?;
        let bytes = match namespace.read_limited(WAYS_ARCHIVE_FILE, MAX_ARCHIVE_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                namespace.check_journal_creation(WAYS_ARCHIVE_FILE)?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let archive: Archive = serde_json::from_slice(&bytes)?;
        Self::open_owned(namespace, canonical, archive.limits).map(Some)
    }
    pub fn limits(&self) -> WaysRetentionLimits {
        self.archive.limits
    }
    pub fn configure_limits(&mut self, limits: WaysRetentionLimits) -> Result<()> {
        self.healthy()?;
        let mut next = self.archive.clone();
        next.limits = limits;
        // The same validation covers every existing record, artifact and
        // unfinished transaction reservation. Reconfiguration never evicts.
        self.publish(next)
    }
    /// Limits are explicit host configuration. Reopening never silently reduces
    /// them or evicts evidence to accommodate a newer configuration.
    pub fn open_owned(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        limits: WaysRetentionLimits,
    ) -> Result<Self> {
        namespace.require_root(&ExecutionComponent::WaysDecisions)?;
        if canonical
            .identity()
            .map_err(|_| WaysArchiveError::Invalid("canonical owner unavailable"))?
            != *namespace.identity()
        {
            return Err(WaysArchiveError::Invalid("foreign canonical owner"));
        }
        validate_ways_retention(&[], limits)?;
        let archive = match namespace.read_limited(WAYS_ARCHIVE_FILE, MAX_ARCHIVE_BYTES) {
            Ok(bytes) => serde_json::from_slice::<Archive>(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                namespace.check_journal_creation(WAYS_ARCHIVE_FILE)?;
                Archive {
                    schema_version: 1,
                    journal_id: namespace.identity().journal_id().into(),
                    owner: namespace.identity().owner().clone(),
                    limits,
                    records: vec![],
                    patches: vec![],
                    deleted: vec![],
                }
            }
            Err(error) => return Err(error.into()),
        };
        if archive.schema_version != 1
            || archive.owner != *namespace.identity().owner()
            || archive.journal_id != namespace.identity().journal_id()
            || archive.limits != limits
        {
            return Err(WaysArchiveError::Invalid(
                "archive identity, version or configured limits changed",
            ));
        }
        let encoded = validate_archive(&archive)?;
        namespace.mark_journal_initialized(WAYS_ARCHIVE_FILE)?;
        namespace.atomic_write(WAYS_ARCHIVE_FILE, &encoded)?;
        Ok(Self {
            namespace,
            archive,
            poisoned: false,
        })
    }
    pub fn records(&self) -> Result<&[WaysDecisionRecord]> {
        self.healthy()?;
        Ok(&self.archive.records)
    }
    pub fn deleted(&self) -> Result<&[DeletedWaysDecision]> {
        self.healthy()?;
        Ok(&self.archive.deleted)
    }
    pub fn get(&self, id: &DecisionId) -> Result<Option<&WaysDecisionRecord>> {
        self.healthy()?;
        Ok(self
            .archive
            .records
            .iter()
            .find(|record| record.decision_id == *id))
    }
    pub fn patch(&self, reference: &EvidenceRef) -> Result<Option<Vec<u8>>> {
        self.healthy()?;
        self.archive
            .patches
            .iter()
            .find(|patch| patch.reference == *reference)
            .map(PinnedWaysPatch::bytes)
            .transpose()
    }
    /// All candidate evidence and patch bodies are durable together before the
    /// caller can begin applying a selected candidate or disposing any clone.
    pub fn freeze(
        &mut self,
        record: WaysDecisionRecord,
        patches: Vec<PinnedWaysPatch>,
    ) -> Result<()> {
        self.healthy()?;
        if let Some(saved) = self.get(&record.decision_id)? {
            for patch in &patches {
                patch.bytes()?;
                if self
                    .archive
                    .patches
                    .iter()
                    .find(|stored| stored.reference == patch.reference)
                    != Some(patch)
                {
                    return Err(WaysArchiveError::Invalid(
                        "retry supplied different protected patch bytes",
                    ));
                }
            }
            return if frozen_record(saved) == frozen_record(&record) {
                Ok(())
            } else {
                Err(WaysArchiveError::Invalid(
                    "decision identity was reused with different evidence",
                ))
            };
        }
        if self
            .archive
            .deleted
            .iter()
            .any(|item| item.decision_id == record.decision_id || item.set_id == record.set_id)
        {
            return Err(WaysArchiveError::Invalid(
                "a deleted decision cannot be recreated",
            ));
        }
        if !matches!(record.application, WaysApplicationOutcome::NotStarted)
            || record.selected_session_turn.is_some()
            || record.cleanup.completed_at_unix_ms.is_some()
            || record
                .cleanup
                .targets
                .iter()
                .any(|target| !matches!(target.outcome, WaysCleanupOutcome::Pending))
        {
            return Err(WaysArchiveError::Invalid(
                "initial evidence already claims application or cleanup",
            ));
        }
        require_completion_capacity(&record, self.archive.limits.record_bytes)?;
        let mut next = self.archive.clone();
        next.records.push(record);
        for patch in patches {
            patch.bytes()?;
            match next
                .patches
                .iter()
                .find(|item| item.reference == patch.reference)
            {
                Some(saved) if *saved != patch => {
                    return Err(WaysArchiveError::Invalid("conflicting protected patch"))
                }
                Some(_) => (),
                None => next.patches.push(patch),
            }
        }
        self.publish(next)
    }
    /// Only monotonic execution/cleanup evidence can change. Candidate bytes,
    /// user choice and targets remain the exact previously pinned decision.
    pub fn record_progress(&mut self, record: WaysDecisionRecord) -> Result<()> {
        self.healthy()?;
        let index = self
            .archive
            .records
            .iter()
            .position(|item| item.decision_id == record.decision_id)
            .ok_or(WaysArchiveError::Invalid(
                "decision evidence was not frozen",
            ))?;
        let saved = &self.archive.records[index];
        if frozen_record(saved) != frozen_record(&record) {
            return Err(WaysArchiveError::Invalid("frozen decision changed"));
        }
        validate_progress(saved, &record)?;
        let mut next = self.archive.clone();
        next.records[index] = record;
        self.publish(next)
    }
    /// The host must verify all external reference ownership before invoking
    /// this explicit deletion. The tombstone survives patch release so History
    /// and reference resolvers can distinguish deletion from missing storage.
    pub fn delete_verified<F>(&mut self, id: &DecisionId, at: u64, verify: F) -> Result<()>
    where
        F: FnOnce(&WaysDecisionRecord) -> Result<EvidenceRef>,
    {
        self.healthy()?;
        let index = self
            .archive
            .records
            .iter()
            .position(|item| item.decision_id == *id)
            .ok_or(WaysArchiveError::Invalid("decision not found"))?;
        let record = &self.archive.records[index];
        if record.cleanup.completed_at_unix_ms.is_none() {
            return Err(WaysArchiveError::Invalid(
                "decision still owns disposable resources",
            ));
        }
        let receipt = verify(record)?;
        let mut next = self.archive.clone();
        let removed = next.records.remove(index);
        next.deleted.push(DeletedWaysDecision {
            decision_id: removed.decision_id,
            set_id: removed.set_id,
            deleted_at_unix_ms: at,
            ownership_receipt: receipt,
        });
        let pinned: HashSet<_> = next
            .records
            .iter()
            .flat_map(record_patches)
            .map(|patch| patch.protected_artifact_ref.clone())
            .collect();
        next.patches
            .retain(|patch| pinned.contains(&patch.reference));
        self.publish(next)
    }
    fn healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(WaysArchiveError::RecoveryRequired);
        }
        self.namespace.verify_ambient_identity()?;
        Ok(())
    }
    fn publish(&mut self, next: Archive) -> Result<()> {
        let encoded = validate_archive(&next)?;
        self.poisoned = true;
        self.namespace.atomic_write(WAYS_ARCHIVE_FILE, &encoded)?;
        self.archive = next;
        self.poisoned = false;
        Ok(())
    }
}

fn record_patches(record: &WaysDecisionRecord) -> impl Iterator<Item = &ProtectedWaysPatch> {
    record
        .candidates
        .iter()
        .filter_map(|candidate| match &candidate.patch {
            Recorded::Available { value } => Some(value),
            _ => None,
        })
}
/// Check the largest successful completion metadata before permitting any
/// apply/cleanup. This temporary shape is never persisted as execution evidence.
fn require_completion_capacity(record: &WaysDecisionRecord, limit: usize) -> Result<()> {
    let mut completed = record.clone();
    let reference = EvidenceRef::new("r".repeat(128))
        .map_err(|_| WaysArchiveError::Invalid("completion reserve identity"))?;
    completed.application = match &record.human_decision.choice {
        WaysHumanChoice::NoKeep => WaysApplicationOutcome::NoKeepRecorded {
            receipt_ref: reference.clone(),
            recorded_at_unix_ms: u64::MAX,
        },
        WaysHumanChoice::Keep { patch } => {
            completed.selected_session_turn = Some(WaysSelectedSessionTurn {
                session_id: record.session_id.clone(),
                turn_id: crate::turn_contract::LogicalTurnId::new("t".repeat(128))
                    .map_err(|_| WaysArchiveError::Invalid("completion reserve turn"))?,
                transcript_receipt_ref: reference.clone(),
            });
            WaysApplicationOutcome::Applied {
                identity: WaysApplicationIdentity {
                    operation_id: reference.clone(),
                    patch: patch.clone(),
                    preimage_tree_oid: "f".repeat(40),
                    postimage_tree_oid: "f".repeat(40),
                },
                receipt_ref: reference.clone(),
                applied_at_unix_ms: u64::MAX,
            }
        }
    };
    for target in &mut completed.cleanup.targets {
        target.outcome = WaysCleanupOutcome::Completed {
            receipt_ref: reference.clone(),
            completed_at_unix_ms: u64::MAX,
        };
    }
    completed.cleanup.completed_at_unix_ms = Some(u64::MAX);
    if serde_json::to_vec(&completed)?.len() > limit {
        return Err(WaysDecisionError::Capacity.into());
    }
    Ok(())
}

fn frozen_record(record: &WaysDecisionRecord) -> WaysDecisionRecord {
    let mut frozen = record.clone();
    frozen.application = WaysApplicationOutcome::NotStarted;
    frozen.selected_session_turn = None;
    frozen.cleanup.completed_at_unix_ms = None;
    for target in &mut frozen.cleanup.targets {
        target.outcome = WaysCleanupOutcome::Pending;
    }
    frozen
}
fn validate_archive(archive: &Archive) -> Result<Vec<u8>> {
    validate_ways_retention(&archive.records, archive.limits)?;
    if archive.records.len().saturating_add(archive.deleted.len()) > archive.limits.records {
        return Err(WaysDecisionError::Capacity.into());
    }
    let maximum = archive.limits.aggregate_bytes.min(MAX_ARCHIVE_BYTES);
    let patch_bytes = archive
        .patches
        .iter()
        .try_fold(0usize, |total, patch| total.checked_add(patch.base64.len()))
        .ok_or(WaysDecisionError::Capacity)?;
    if patch_bytes > maximum {
        return Err(WaysDecisionError::Capacity.into());
    }
    let mut ids = HashSet::new();
    let mut sets = HashSet::new();
    let mut references = HashSet::new();
    for patch in &archive.patches {
        if !references.insert(&patch.reference) {
            return Err(WaysArchiveError::Invalid("duplicate patch"));
        }
        patch.bytes()?;
    }
    let mut used = HashSet::new();
    for record in &archive.records {
        if record.session_id != archive.owner.session_id {
            return Err(WaysArchiveError::Invalid("foreign Session decision"));
        }
        ids.insert(&record.decision_id);
        sets.insert(&record.set_id);
        for expected in record_patches(record) {
            let patch = archive
                .patches
                .iter()
                .find(|patch| patch.reference == expected.protected_artifact_ref)
                .ok_or(WaysArchiveError::Invalid(
                    "required protected patch missing",
                ))?;
            if patch.sha256 != expected.patch_sha256 || patch.byte_len != expected.patch_bytes {
                return Err(WaysArchiveError::Invalid(
                    "candidate patch identity mismatch",
                ));
            }
            used.insert(&patch.reference);
        }
    }
    if used != references {
        return Err(WaysArchiveError::Invalid("unowned protected artifact"));
    }
    for deleted in &archive.deleted {
        if !ids.insert(&deleted.decision_id) || !sets.insert(&deleted.set_id) {
            return Err(WaysArchiveError::Invalid("duplicate deleted decision"));
        }
    }
    let mut encoded = BoundedBytes {
        bytes: Vec::new(),
        maximum,
    };
    serde_json::to_writer(&mut encoded, archive).map_err(|error| {
        if error.is_io() {
            WaysArchiveError::Contract(WaysDecisionError::Capacity)
        } else {
            error.into()
        }
    })?;
    // Reserve each unfinished record's complete configured envelope before
    // applying any patch. Later application/link/cleanup receipts can grow up
    // to that bound without competing with newly frozen decisions.
    let reserved = archive
        .records
        .iter()
        .filter(|record| record.cleanup.completed_at_unix_ms.is_none())
        .try_fold(0usize, |sum, record| {
            let used = record.validate(archive.limits)?;
            sum.checked_add(archive.limits.record_bytes.saturating_sub(used))
                .ok_or(WaysDecisionError::Capacity)
        })?;
    if encoded
        .bytes
        .len()
        .checked_add(reserved)
        .is_none_or(|size| size > maximum)
    {
        return Err(WaysDecisionError::Capacity.into());
    }
    Ok(encoded.bytes)
}
struct BoundedBytes {
    bytes: Vec<u8>,
    maximum: usize,
}
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > self.maximum)
        {
            return Err(io::Error::other("Ways archive capacity"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn validate_progress(saved: &WaysDecisionRecord, next: &WaysDecisionRecord) -> Result<()> {
    use WaysApplicationOutcome::*;
    let identity = |state: &WaysApplicationOutcome| match state {
        Pending { identity }
        | Failed { identity, .. }
        | ReconciliationRequired { identity, .. }
        | Applied { identity, .. } => Some(identity.clone()),
        _ => None,
    };
    let valid = match (&saved.application, &next.application) {
        (NotStarted, NotStarted | Pending { .. } | NoKeepRecorded { .. }) => true,
        (
            Pending { .. } | Failed { .. } | ReconciliationRequired { .. },
            Pending { .. } | Failed { .. } | ReconciliationRequired { .. } | Applied { .. },
        ) => identity(&saved.application) == identity(&next.application),
        (Applied { .. }, Applied { .. }) | (NoKeepRecorded { .. }, NoKeepRecorded { .. }) => {
            saved.application == next.application
        }
        _ => false,
    };
    if !valid
        || (saved.selected_session_turn.is_some()
            && saved.selected_session_turn != next.selected_session_turn)
        || (saved.cleanup.completed_at_unix_ms.is_some() && saved.cleanup != next.cleanup)
    {
        return Err(WaysArchiveError::Invalid(
            "application or cleanup moved backwards",
        ));
    }
    for (before, after) in saved.cleanup.targets.iter().zip(&next.cleanup.targets) {
        if matches!(before.outcome, WaysCleanupOutcome::Completed { .. })
            && before.outcome != after.outcome
        {
            return Err(WaysArchiveError::Invalid("completed cleanup changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "ways_decision_store_tests.rs"]
mod tests;
