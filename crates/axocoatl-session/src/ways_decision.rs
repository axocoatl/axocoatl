//! Retained Ways decision schema. No persistence, Keep, cleanup,
//! purge, execution, or UI authority is conferred by validating these bytes.
//! The owning host must freeze candidates, verify and pin every protected
//! artifact, durably reserve capacity, and use the existing Keep transaction.
//! An unavailable source is explicit; a storage failure is a hard error and
//! must never be converted into unavailable evidence to permit cleanup.

use crate::execution_content::{ExecutionModelRef, ExecutionUsage};
use crate::turn_contract::{
    AgentDefinitionId, EvidenceRef, InvocationId, LogicalTurnId, SessionId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Write;

pub const WAYS_DECISION_SCHEMA_VERSION: u32 = 1;
pub const WAYS_RETENTION_LIMITS_VERSION: u32 = 1;

/// Caller-configured versioned admission limits. There is no default retention
/// budget. The host supplies the complete aggregate ownership scope and must
/// retain every previously promised record when configuration changes.
///
/// `records` and `aggregate_bytes` bound what is retained at once: the
/// retained decisions, the protected patches they pin, and the room each
/// unfinished decision reserves for its final receipts. Deleting a decision
/// releases its share; its deletion tombstone is kept and not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysRetentionLimits {
    pub version: u32,
    pub field_bytes: usize,
    pub record_bytes: usize,
    pub aggregate_bytes: usize,
    pub records: usize,
    pub candidates: usize,
    pub items_per_field: usize,
}
// Matches the existing execution_namespace MAX_FILE_BYTES representation bound.
// This is a serialized-object bound, not an aggregate retention budget. A store
// must also reserve its framing/index and separately retained artifact bytes.
const MAX_SERIALIZED_DECISION_BYTES: usize = 64 * 1024 * 1024;
// Preserve the current Ways launch/roster maximum; no smaller product limit.
const MAX_WAYS_CANDIDATES: usize = 100;
impl WaysRetentionLimits {
    fn validate(self) -> Result<(), WaysDecisionError> {
        if self.version != WAYS_RETENTION_LIMITS_VERSION {
            return Err(WaysDecisionError::Version);
        }
        if [
            self.field_bytes,
            self.record_bytes,
            self.aggregate_bytes,
            self.records,
            self.candidates,
            self.items_per_field,
        ]
        .contains(&0)
            || self.field_bytes > self.record_bytes
            || self.record_bytes > self.aggregate_bytes
            || self.record_bytes > MAX_SERIALIZED_DECISION_BYTES
            || self.candidates > MAX_WAYS_CANDIDATES
        {
            return Err(WaysDecisionError::Capacity);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WaysDecisionError {
    #[error("unsupported Ways decision or retention schema")]
    Version,
    #[error(
        "Ways decision retention capacity exceeded; retain existing evidence and block cleanup"
    )]
    Capacity,
    #[error("invalid Ways decision: {0}")]
    Invalid(&'static str),
    #[error("invalid Ways decision JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecisionId(pub EvidenceRef);
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WaysSetId(pub EvidenceRef);
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysCandidateId {
    pub set_id: WaysSetId,
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    NotProduced,
    NotRecorded,
    UnsupportedRepresentation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recorded<T> {
    Available { value: T },
    Unavailable { reason: UnavailableReason },
}

/// Retained bytes are always inline. Original digest/length never stand in for
/// the retained body. A truncated fragment records its exact original offset;
/// thus both a diff prefix and the existing Check output tail are representable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewText {
    Complete {
        text: String,
        sha256: String,
    },
    Truncated {
        text: String,
        retained_sha256: String,
        original_sha256: String,
        original_bytes: u64,
        offset_bytes: u64,
    },
    Unavailable {
        reason: UnavailableReason,
        detail: String,
    },
}
impl ReviewText {
    pub fn complete(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::Complete {
            sha256: sha256(text.as_bytes()),
            text,
        }
    }
    fn validate(&self, limit: usize) -> Result<(), WaysDecisionError> {
        match self {
            Self::Complete {
                text,
                sha256: expected,
            } => {
                bounded(text, limit)?;
                if &sha256(text.as_bytes()) != expected {
                    return invalid("retained text digest mismatch");
                }
            }
            Self::Truncated {
                text,
                retained_sha256,
                original_sha256,
                original_bytes,
                offset_bytes,
            } => {
                bounded(text, limit)?;
                let retained =
                    u64::try_from(text.len()).map_err(|_| WaysDecisionError::Capacity)?;
                if text.is_empty()
                    || !digest(original_sha256)
                    || sha256(text.as_bytes()) != *retained_sha256
                    || *original_bytes <= retained
                    || offset_bytes
                        .checked_add(retained)
                        .is_none_or(|end| end > *original_bytes)
                {
                    return invalid("invalid visible truncation");
                }
            }
            Self::Unavailable { detail, .. } => {
                name(detail, limit)?;
            }
        }
        Ok(())
    }
}

/// Lists never silently omit entries. A partial capture retains the original
/// count; it cannot be presented as the full path/tool inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedItems<T> {
    pub items: Vec<T>,
    pub original_count: u64,
}
impl<T> RetainedItems<T> {
    fn validate(&self, maximum: usize) -> Result<(), WaysDecisionError> {
        if self.items.len() > maximum {
            return Err(WaysDecisionError::Capacity);
        }
        if self.original_count < self.items.len() as u64 {
            return invalid("invalid retained item count");
        }
        Ok(())
    }
    pub fn is_truncated(&self) -> bool {
        self.original_count > self.items.len() as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysRepositoryIdentity {
    pub workspace_id: EvidenceRef,
    /// Immutable host-retained repository ownership binding, never a mutable path lookup.
    pub repository_ref: EvidenceRef,
    pub commit_oid: String,
    pub tree_oid: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedWaysPatch {
    pub candidate: WaysCandidateId,
    pub base_commit_oid: String,
    pub base_tree_oid: String,
    pub candidate_commit_oid: String,
    pub candidate_tree_oid: String,
    pub patch_sha256: String,
    pub patch_bytes: u64,
    /// Host must resolve and pin this exact protected object set before cleanup.
    pub protected_artifact_ref: EvidenceRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaysTerminalState {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysUsage {
    pub measurement_id: EvidenceRef,
    pub tokens: ExecutionUsage,
    /// Existing dollar accounting is retained without inventing missing usage.
    pub cost_usd_known_subtotal: f64,
    pub cost_complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysToolReference {
    pub invocation_id: InvocationId,
    pub event_ref: EvidenceRef,
    pub detail: ReviewText,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WaysCheckOutcome {
    Passed,
    Failed,
    /// The host rejected verification independently of the command's exit.
    /// For example, an exit-zero check can produce an unsupported Git delta.
    VerificationRejected {
        reason: ReviewText,
    },
    Interrupted,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysCheckEvidence {
    pub check_id: EvidenceRef,
    /// The exact command is either retained in full or explicitly unavailable.
    pub command: Recorded<String>,
    pub outcome: WaysCheckOutcome,
    pub exit_code: Recorded<i32>,
    pub output: ReviewText,
    pub duration_ms: Recorded<u64>,
    /// Capture can fail before a protected patch exists. Missing evidence must
    /// remain explicit and can never qualify this Check as Passed.
    pub checked_patch: Recorded<ProtectedWaysPatch>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysCandidateEvidence {
    pub id: WaysCandidateId,
    pub agent: AgentDefinitionId,
    pub model: Recorded<ExecutionModelRef>,
    pub isolation: Recorded<String>,
    pub terminal: WaysTerminalState,
    pub failure_or_no_change_reason: Recorded<ReviewText>,
    pub outcome: ReviewText,
    pub route: ReviewText,
    pub tools: RetainedItems<WaysToolReference>,
    pub changed_paths: RetainedItems<String>,
    pub patch: Recorded<ProtectedWaysPatch>,
    pub reviewable_diff: ReviewText,
    pub checks: Vec<WaysCheckEvidence>,
    pub usage: WaysUsage,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysJudgeEvidence {
    pub judgment_id: EvidenceRef,
    pub criteria: ReviewText,
    pub model: ExecutionModelRef,
    /// Exact candidates that were judged; no later mutable candidate lookup.
    pub candidate_patches: Vec<ProtectedWaysPatch>,
    pub result: ReviewText,
    pub recommended: Option<WaysCandidateId>,
    pub usage: WaysUsage,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaysHumanChoice {
    Keep { patch: ProtectedWaysPatch },
    NoKeep,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysHumanDecision {
    pub decision_intent_id: EvidenceRef,
    pub decided_at_unix_ms: u64,
    pub choice: WaysHumanChoice,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysApplicationIdentity {
    pub operation_id: EvidenceRef,
    pub patch: ProtectedWaysPatch,
    pub preimage_tree_oid: String,
    pub postimage_tree_oid: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaysApplicationOutcome {
    NotStarted,
    Pending {
        identity: WaysApplicationIdentity,
    },
    ReconciliationRequired {
        identity: WaysApplicationIdentity,
        detail: ReviewText,
    },
    Failed {
        identity: WaysApplicationIdentity,
        detail: ReviewText,
    },
    Applied {
        identity: WaysApplicationIdentity,
        receipt_ref: EvidenceRef,
        applied_at_unix_ms: u64,
    },
    NoKeepRecorded {
        receipt_ref: EvidenceRef,
        recorded_at_unix_ms: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysSelectedSessionTurn {
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub transcript_receipt_ref: EvidenceRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaysResourceKind {
    Sandbox,
    DependencyVolume,
    Clone,
    /// Only disposable candidate refs after retained objects are independently
    /// pinned. Distinct reference strings do not prove disjoint Git object sets;
    /// the cleanup host must establish that proof. Purge is a separate authority.
    DisposableCandidateGitRefs,
    Memory,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaysCleanupOutcome {
    Pending,
    Failed {
        detail: ReviewText,
    },
    Completed {
        receipt_ref: EvidenceRef,
        completed_at_unix_ms: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysCleanupTarget {
    pub candidate: WaysCandidateId,
    pub kind: WaysResourceKind,
    pub backend: String,
    pub resource_id: String,
    pub ownership_ref: EvidenceRef,
    pub outcome: WaysCleanupOutcome,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysCleanupEvidence {
    pub inventory_complete: bool,
    pub targets: Vec<WaysCleanupTarget>,
    pub completed_at_unix_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaysDecisionRecord {
    pub schema_version: u32,
    pub retention_limits_version: u32,
    pub decision_id: DecisionId,
    pub session_id: SessionId,
    pub source_turn_id: LogicalTurnId,
    pub set_id: WaysSetId,
    pub task: ReviewText,
    pub starting_repository: WaysRepositoryIdentity,
    pub candidates: Vec<WaysCandidateEvidence>,
    pub judge: Option<WaysJudgeEvidence>,
    /// Shared preparation/probe/plan usage is never copied into each candidate.
    pub shared_usage: Vec<WaysUsage>,
    pub human_decision: WaysHumanDecision,
    pub application: WaysApplicationOutcome,
    pub selected_session_turn: Option<WaysSelectedSessionTurn>,
    pub cleanup: WaysCleanupEvidence,
}

impl WaysDecisionRecord {
    pub fn from_json(bytes: &[u8], limits: WaysRetentionLimits) -> Result<Self, WaysDecisionError> {
        limits.validate()?;
        if bytes.len() > limits.record_bytes {
            return Err(WaysDecisionError::Capacity);
        }
        let record: Self = serde_json::from_slice(bytes)?;
        record.validate(limits)?;
        Ok(record)
    }
    pub fn validate(&self, limits: WaysRetentionLimits) -> Result<usize, WaysDecisionError> {
        limits.validate()?;
        if self.schema_version != WAYS_DECISION_SCHEMA_VERSION
            || self.retention_limits_version != limits.version
        {
            return Err(WaysDecisionError::Version);
        }
        if self.candidates.is_empty() || self.candidates.len() > limits.candidates {
            return Err(WaysDecisionError::Capacity);
        }
        let field = limits.field_bytes;
        self.task.validate(field)?;
        git_pair(
            &self.starting_repository.commit_oid,
            &self.starting_repository.tree_oid,
        )?;
        let mut ids = HashSet::new();
        let mut usage_ids = HashSet::new();
        for candidate in &self.candidates {
            if candidate.id.set_id != self.set_id || !ids.insert(&candidate.id) {
                return invalid("duplicate or foreign candidate");
            }
            if let Recorded::Available { value } = &candidate.model {
                model(value, field)?;
            }
            if let Recorded::Available { value } = &candidate.isolation {
                name(value, field)?;
            }
            if let Recorded::Available { value } = &candidate.failure_or_no_change_reason {
                value.validate(field)?;
            }
            candidate.outcome.validate(field)?;
            candidate.route.validate(field)?;
            candidate.reviewable_diff.validate(field)?;
            candidate.tools.validate(limits.items_per_field)?;
            candidate.changed_paths.validate(limits.items_per_field)?;
            let mut tool_ids = HashSet::new();
            for tool in &candidate.tools.items {
                if !tool_ids.insert(&tool.event_ref) {
                    return invalid("duplicate tool event");
                }
                tool.detail.validate(field)?;
            }
            let mut paths = HashSet::new();
            for path in &candidate.changed_paths.items {
                name(path, field)?;
                if path.starts_with('/')
                    || path.split('/').any(|part| part == "..")
                    || path.contains('\0')
                    || !paths.insert(path)
                {
                    return invalid("invalid or duplicate changed path");
                }
            }
            if let Recorded::Available { value } = &candidate.patch {
                self.validate_patch(value)?;
                if value.candidate != candidate.id {
                    return invalid("candidate owns another candidate patch");
                }
            }
            if candidate.checks.len() > limits.items_per_field {
                return Err(WaysDecisionError::Capacity);
            }
            let mut checks = HashSet::new();
            for check in &candidate.checks {
                if !checks.insert(&check.check_id) {
                    return invalid("duplicate check");
                }
                if let Recorded::Available { value } = &check.command {
                    name(value, field)?;
                }
                check.output.validate(field)?;
                if let Recorded::Available { value } = &check.checked_patch {
                    self.validate_patch(value)?;
                    if value.candidate != candidate.id || candidate.patch != check.checked_patch {
                        return invalid("check not bound to exact candidate patch");
                    }
                }
                match (&check.outcome, &check.exit_code) {
                    (WaysCheckOutcome::Passed, Recorded::Available { value: 0 })
                        if matches!(check.checked_patch, Recorded::Available { .. }) => {}
                    (WaysCheckOutcome::Failed, Recorded::Available { value }) if *value != 0 => (),
                    (WaysCheckOutcome::VerificationRejected { reason }, _) => {
                        reason.validate(field)?;
                    }
                    (WaysCheckOutcome::Interrupted | WaysCheckOutcome::Unknown, _) => (),
                    _ => return invalid("check result contradicts exit evidence"),
                }
            }
            usage(&candidate.usage, &mut usage_ids)?;
        }
        if self.shared_usage.len() > limits.items_per_field {
            return Err(WaysDecisionError::Capacity);
        }
        for value in &self.shared_usage {
            usage(value, &mut usage_ids)?;
        }
        if let Some(judge) = &self.judge {
            judge.criteria.validate(field)?;
            judge.result.validate(field)?;
            model(&judge.model, field)?;
            usage(&judge.usage, &mut usage_ids)?;
            if judge.candidate_patches.is_empty()
                || judge.candidate_patches.len() > limits.candidates
            {
                return invalid("invalid Judge candidate set");
            }
            let mut judged = HashSet::new();
            for patch in &judge.candidate_patches {
                self.require_candidate_patch(patch)?;
                if !judged.insert(&patch.candidate) {
                    return invalid("duplicate Judge candidate");
                }
            }
            if judge
                .recommended
                .as_ref()
                .is_some_and(|id| !judged.contains(id))
            {
                return invalid("Judge recommended an unjudged candidate");
            }
        }
        let selected = match &self.human_decision.choice {
            WaysHumanChoice::Keep { patch } => {
                self.require_candidate_patch(patch)?;
                Some(patch)
            }
            WaysHumanChoice::NoKeep => None,
        };
        let (application_identity, settled) = match &self.application {
            WaysApplicationOutcome::NotStarted => (None, false),
            WaysApplicationOutcome::Pending { identity } => (Some(identity), false),
            WaysApplicationOutcome::Failed { identity, detail }
            | WaysApplicationOutcome::ReconciliationRequired { identity, detail } => {
                detail.validate(field)?;
                (Some(identity), false)
            }
            WaysApplicationOutcome::Applied { identity, .. } => (Some(identity), true),
            WaysApplicationOutcome::NoKeepRecorded { .. } => {
                if selected.is_some() {
                    return invalid("Keep intent claims no-Keep result");
                }
                (None, true)
            }
        };
        if let Some(identity) = application_identity {
            if selected != Some(&identity.patch) {
                return invalid("application differs from exact Keep intent");
            }
            git_pair(&identity.preimage_tree_oid, &identity.postimage_tree_oid)?;
            if identity.preimage_tree_oid.len() != identity.patch.base_tree_oid.len() {
                return invalid("application uses a different Git object format");
            }
        }
        if let Some(link) = &self.selected_session_turn {
            if link.session_id != self.session_id
                || !matches!(self.application, WaysApplicationOutcome::Applied { .. })
            {
                return invalid("Session link without successful exact Keep");
            }
        }
        if self.cleanup.targets.len() > limits.items_per_field {
            return Err(WaysDecisionError::Capacity);
        }
        let mut resources = HashSet::new();
        for target in &self.cleanup.targets {
            if !ids.contains(&target.candidate) {
                return invalid("foreign cleanup candidate");
            }
            if target.ownership_ref == self.starting_repository.repository_ref
                || self.candidates.iter().any(|candidate| matches!(&candidate.patch,
                    Recorded::Available { value } if value.protected_artifact_ref == target.ownership_ref)) {
                return invalid("cleanup targets retained evidence or starting repository ownership");
            }
            name(&target.backend, field)?;
            name(&target.resource_id, field)?;
            if !resources.insert((&target.kind, &target.backend, &target.resource_id)) {
                return invalid("duplicate cleanup resource");
            }
            if let WaysCleanupOutcome::Failed { detail } = &target.outcome {
                detail.validate(field)?;
            }
            if matches!(target.outcome, WaysCleanupOutcome::Completed { .. })
                && (!settled || (selected.is_some() && self.selected_session_turn.is_none()))
            {
                return invalid("cleanup precedes application and Session settlement");
            }
        }
        if self.cleanup.completed_at_unix_ms.is_some()
            && (!self.cleanup.inventory_complete
                || !settled
                || (selected.is_some() && self.selected_session_turn.is_none())
                || self
                    .cleanup
                    .targets
                    .iter()
                    .any(|target| !matches!(target.outcome, WaysCleanupOutcome::Completed { .. })))
        {
            return invalid("cleanup completion lacks exact completed inventory");
        }
        encoded_len(self, limits.record_bytes)
    }
    fn validate_patch(&self, patch: &ProtectedWaysPatch) -> Result<(), WaysDecisionError> {
        if patch.candidate.set_id != self.set_id
            || patch.base_commit_oid != self.starting_repository.commit_oid
            || patch.base_tree_oid != self.starting_repository.tree_oid
            || !digest(&patch.patch_sha256)
        {
            return invalid("protected patch source mismatch");
        }
        git_pair(&patch.candidate_commit_oid, &patch.candidate_tree_oid)?;
        if patch.candidate_commit_oid.len() != patch.base_commit_oid.len() {
            return invalid("mixed Git object formats");
        }
        Ok(())
    }
    fn require_candidate_patch(&self, patch: &ProtectedWaysPatch) -> Result<(), WaysDecisionError> {
        self.validate_patch(patch)?;
        if !self.candidates.iter().any(|candidate| {
            candidate.id == patch.candidate
                && candidate.patch
                    == (Recorded::Available {
                        value: patch.clone(),
                    })
        }) {
            return invalid("reference does not resolve to retained exact candidate patch");
        }
        Ok(())
    }
}

/// Pure admission check over the complete retained inventory plus the proposed
/// record. This never evicts or edits entries, deduplicates away promised bytes,
/// authorizes cleanup, or pretends that a configured store has persisted them.
pub fn validate_ways_retention(
    records: &[WaysDecisionRecord],
    limits: WaysRetentionLimits,
) -> Result<usize, WaysDecisionError> {
    limits.validate()?;
    if records.len() > limits.records {
        return Err(WaysDecisionError::Capacity);
    }
    let mut ids = HashSet::new();
    let mut sets = HashSet::new();
    let mut bytes = 0usize;
    for record in records {
        if !ids.insert(&record.decision_id) || !sets.insert((&record.session_id, &record.set_id)) {
            return invalid("duplicate retained decision or set");
        }
        bytes = bytes
            .checked_add(record.validate(limits)?)
            .ok_or(WaysDecisionError::Capacity)?;
        if bytes > limits.aggregate_bytes {
            return Err(WaysDecisionError::Capacity);
        }
    }
    Ok(bytes)
}
fn invalid<T>(message: &'static str) -> Result<T, WaysDecisionError> {
    Err(WaysDecisionError::Invalid(message))
}
fn bounded(value: &str, limit: usize) -> Result<(), WaysDecisionError> {
    if value.len() > limit {
        Err(WaysDecisionError::Capacity)
    } else {
        Ok(())
    }
}
fn name(value: &str, limit: usize) -> Result<(), WaysDecisionError> {
    bounded(value, limit)?;
    if value.is_empty() {
        invalid("empty required text")
    } else {
        Ok(())
    }
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
fn git_pair(left: &str, right: &str) -> Result<(), WaysDecisionError> {
    if !matches!(left.len(), 40 | 64)
        || left.len() != right.len()
        || !left
            .bytes()
            .chain(right.bytes())
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        invalid("invalid Git object identity")
    } else {
        Ok(())
    }
}
fn model(value: &ExecutionModelRef, limit: usize) -> Result<(), WaysDecisionError> {
    name(&value.provider_id, limit)?;
    name(&value.model_id, limit)
}
fn usage<'a>(
    value: &'a WaysUsage,
    seen: &mut HashSet<&'a EvidenceRef>,
) -> Result<(), WaysDecisionError> {
    if !value.cost_usd_known_subtotal.is_finite()
        || value.cost_usd_known_subtotal < 0.0
        || !seen.insert(&value.measurement_id)
    {
        invalid("invalid or multiply attributed usage")
    } else {
        Ok(())
    }
}
struct ByteCounter {
    count: usize,
    limit: usize,
}
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.count = self
            .count
            .checked_add(bytes.len())
            .filter(|value| *value <= self.limit)
            .ok_or_else(|| std::io::Error::other("Ways record limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_len(value: &WaysDecisionRecord, limit: usize) -> Result<usize, WaysDecisionError> {
    let mut counter = ByteCounter { count: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|error| {
        if error.is_io() {
            WaysDecisionError::Capacity
        } else {
            WaysDecisionError::Json(error)
        }
    })?;
    Ok(counter.count)
}

#[cfg(test)]
#[path = "ways_decision_tests.rs"]
mod tests;
