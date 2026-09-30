//! Durable admission of events for a configured Session team.
//!
//! This inbox owns receipts and reserved turn identities, not execution. An
//! authenticated ingress must resolve an explicitly enabled binding before
//! calling `admit`. The Session controller must then revalidate authority,
//! environment readiness and exclusive ownership before executing a reservation.
//! After a crash, a reservation requires lookup of that exact turn; it never
//! establishes that provider or tool work can safely be repeated.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

#[path = "team_work_budget.rs"]
pub(crate) mod budget;
#[path = "team_work_conditions.rs"]
mod conditions;
pub use budget::{
    CeilingDecision, DurableTeamWorkAllocation, SettlementBasis, TeamWorkGrantAllocation,
    TeamWorkGrantSettlement,
};
pub use conditions::{
    standing_check_definitions, standing_condition_id, standing_readiness_text,
    REPOSITORY_SNAPSHOT_COMMAND,
};

const SCHEMA_VERSION: u32 = 1;
const FILE_NAME: &str = "team-work.v1.json";
const MAX_RECEIPTS: usize = 512;
const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 32 * 1024;
const MAX_BINDINGS: usize = MAX_RECEIPTS;
const INITIALIZED: &str = "team-work.initialized.v1";

/// The host resolves the producer credential; it is never copied into receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TeamWorkSource {
    Manual,
    SignedWebhook {
        configuration_name: String,
    },
    SessionCompletion {
        session_id: String,
    },
    /// Stigmergic work inside this Session's own repository. The host leaves
    /// deposits on paths (findings, changes, failed checks); a route's Agent
    /// receives targeted work when the evaporated signal on the paths it
    /// watches crosses its threshold. Work is still admitted, reserved and
    /// executed through this inbox and the ordinary native controller.
    SignalField {
        routes: Vec<SignalRoute>,
        /// Evaporation half-life. `None` keeps deposits at full strength.
        #[serde(default)]
        half_life_ms: Option<u64>,
        /// Automatic dispatches allowed per episode: a stretch of work that
        /// starts with a deposit after the field was last quiet.
        max_dispatches: u32,
        /// Measured tokens the signal turns of one episode may use before
        /// further crossings are held for a person. `None` is no extra bound.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_episode_tokens: Option<u64>,
    },
}

/// One Session team slot's responsibility in a signal field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalRoute {
    pub slot_id: String,
    /// Repository path patterns whose signals this slot senses.
    pub watches: Vec<String>,
    /// Threshold in thousandths of one finding's deposit (1000 = one finding).
    pub threshold_milli: u32,
    /// Path patterns this slot may change during signal work. Absent means
    /// its watched paths; empty makes it read-only, like a reviewer that
    /// watches everything and reports findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owns: Option<Vec<String>>,
}

impl SignalRoute {
    /// The paths this slot may change during signal work.
    pub fn owned(&self) -> &[String] {
        self.owns.as_deref().unwrap_or(&self.watches)
    }
}

pub const MAX_SIGNAL_ROUTES: usize = 32;
pub const MAX_SIGNAL_WATCHES: usize = 32;
pub const MAX_SIGNAL_DISPATCHES: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkGrantReference {
    pub id: String,
    pub revision: u64,
    pub limits: crate::control_authority::GrantLimits,
    pub expires_at_ms: u64,
}

/// Append-only binding revisions. Disarming affects later admission/dispatch;
/// it does not pretend to stop a provider call that already started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmedTeamWorkBinding {
    pub binding: TeamWorkBinding,
    pub source: TeamWorkSource,
    pub armed: bool,
    pub instruction: String,
    #[serde(default)]
    pub required_checks: Vec<Vec<String>>,
    pub grants: Vec<TeamWorkGrantReference>,
    pub authorized_at_ms: u64,
    #[serde(default)]
    pub source_after_turn: Option<String>,
}

/// Immutable binding selected by the host, never by an event's payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkBinding {
    pub binding_id: String,
    pub binding_revision: u64,
    pub workspace_id: String,
    pub session_id: String,
    pub team_revision: u64,
    pub grant_id: String,
    pub grant_revision: u64,
    pub source_id: String,
    pub event_kind: String,
}

/// An exact build, revision, or other immutable subject that was admitted.
/// A moving branch name or preview URL alone is not a subject identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkSubject {
    pub kind: String,
    pub reference_id: String,
    pub version: String,
}

/// Producer identity and immutable evidence; raw webhook secrets are not stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkEvent {
    pub source_id: String,
    pub event_id: String,
    pub event_kind: String,
    /// Digest of the authenticated immutable event representation, supplied by
    /// ingress. This store checks its form, not the producer's authenticity.
    pub content_sha256: String,
    /// Producer-stable grouping, such as a release candidate. It is not a
    /// deduplication key: distinct builds in one release remain distinct work.
    pub correlation_id: String,
    /// Host-verified originating turn, when the event resulted from our own
    /// work. A later admission policy uses it to prevent self-triggering loops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by_turn_id: Option<String>,
    pub subject: TeamWorkSubject,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkRequest {
    pub binding: TeamWorkBinding,
    pub event: TeamWorkEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TeamWorkDisposition {
    Queued,
    /// Only the execution identity is reserved. Dispatch is not implied.
    Reserved,
    Dismissed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamWorkReceipt {
    pub receipt_id: String,
    /// Preallocated before acknowledgement, stable through reservation/restart.
    pub turn_id: String,
    pub received_at: u64,
    pub request: TeamWorkRequest,
    pub disposition: TeamWorkDisposition,
    /// Exact native controller admission, durably frozen with the reservation.
    /// A restart consults this request's turn and never allocates another one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_source: Option<String>,
    #[serde(default)]
    pub native_capacity_reserved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allocations: Vec<TeamWorkGrantAllocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub never_begun: Option<NativeWorkNoDispatch>,
    /// A person accepted settling unknown provider usage at its ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling_decision: Option<budget::CeilingDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeWorkNoDispatch {
    canonical_journal_id: String,
    canonical_record_count: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TeamWorkError {
    #[error("team-work storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("team-work serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unsupported team-work schema {0}")]
    UnsupportedVersion(u32),
    #[error("invalid team-work record: {0}")]
    Invalid(String),
    #[error("event identity was already admitted with different content or binding")]
    EventConflict,
    #[error("team-work receipt not found: {0}")]
    NotFound(String),
    #[error("team-work disposition conflicts with this operation")]
    DispositionConflict,
    #[error("team-work inbox is full; the event has not been acknowledged")]
    Capacity,
    #[error("a storage write failed; reopen the inbox to reconcile its durable state")]
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxData {
    schema_version: u32,
    receipts: Vec<TeamWorkReceipt>,
    #[serde(default)]
    bindings: Vec<ArmedTeamWorkBinding>,
}

/// Bounded, single-writer, owner-only inbox. No records are silently evicted.
/// The directory must be private control-plane storage, outside a repository
/// mount. Retention/deletion requires a later explicit owner-aware operation.
pub struct TeamWorkInbox {
    dir: SecureDir,
    data: InboxData,
    recovery_required: bool,
}

impl TeamWorkInbox {
    /// Open an existing, durably provisioned control-plane directory. Creating
    /// and fsyncing its ancestor entries is the host's responsibility; refusing
    /// missing directories prevents an acknowledgement above an ephemeral path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TeamWorkError> {
        let dir = SecureDir::open(path)?;
        dir.restrict_owner_only()?;
        #[cfg(unix)]
        dir.try_lock_exclusive()?;
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "team-work inbox requires a supported single-writer directory lock",
        )
        .into());

        let initialized = match dir.read_limited(INITIALIZED, 16) {
            Ok(bytes) if bytes == b"1\n" => true,
            Ok(_) => {
                return Err(TeamWorkError::Invalid(
                    "invalid team-work initialization evidence".into(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let data = match dir.read_limited(FILE_NAME, MAX_STORE_BYTES) {
            Ok(bytes) => {
                let data = serde_json::from_slice::<InboxData>(&bytes)?;
                validate_data(&data)?;
                // A previous rename may have succeeded while its directory
                // fsync failed. Reading is not a durability barrier. Rewrite
                // successfully before loaded receipts can be acknowledged.
                dir.atomic_write(FILE_NAME, &bytes)?;
                data
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if initialized {
                    return Err(TeamWorkError::Invalid(
                        "initialized team-work journal is missing".into(),
                    ));
                }
                let data = InboxData {
                    schema_version: SCHEMA_VERSION,
                    receipts: Vec::new(),
                    bindings: Vec::new(),
                };
                dir.atomic_write(FILE_NAME, &serde_json::to_vec(&data)?)?;
                data
            }
            Err(error) => return Err(error.into()),
        };
        validate_data(&data)?;
        dir.atomic_write(INITIALIZED, b"1\n")?;
        Ok(Self {
            dir,
            data,
            recovery_required: false,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(FILE_NAME)
    }

    pub fn bindings(&self) -> Result<&[ArmedTeamWorkBinding], TeamWorkError> {
        self.ensure_usable()?;
        Ok(&self.data.bindings)
    }

    pub fn current_binding(
        &self,
        binding_id: &str,
    ) -> Result<Option<&ArmedTeamWorkBinding>, TeamWorkError> {
        self.ensure_usable()?;
        Ok(self
            .data
            .bindings
            .iter()
            .rev()
            .find(|entry| entry.binding.binding_id == binding_id))
    }

    /// Caller has authenticated the owner and resolved the exact live team and
    /// grants. Optimistic revision is checked under the single inbox writer.
    pub fn configure_binding(
        &mut self,
        expected_revision: u64,
        binding: ArmedTeamWorkBinding,
    ) -> Result<ArmedTeamWorkBinding, TeamWorkError> {
        self.ensure_usable()?;
        validate_binding(&binding)?;
        let existing = self.current_binding(&binding.binding.binding_id)?;
        if existing == Some(&binding) {
            return Ok(binding);
        }
        let current = existing
            .map(|entry| entry.binding.binding_revision)
            .unwrap_or(0);
        if current != expected_revision
            || expected_revision.checked_add(1) != Some(binding.binding.binding_revision)
        {
            return Err(TeamWorkError::DispositionConflict);
        }
        if existing.is_some_and(|entry| {
            entry.binding.session_id != binding.binding.session_id
                || entry.binding.workspace_id != binding.binding.workspace_id
        }) {
            return Err(TeamWorkError::EventConflict);
        }
        if self.data.bindings.len() >= MAX_BINDINGS {
            return Err(TeamWorkError::Capacity);
        }
        let mut next = self.data.clone();
        next.bindings.push(binding.clone());
        self.commit(next)?;
        Ok(binding)
    }

    /// Acknowledge only after the receipt and its exact target are fsynced.
    /// Same binding/source/event identity with changed content is a conflict,
    /// even if the configured team has since acquired a newer revision.
    pub fn admit(
        &mut self,
        request: TeamWorkRequest,
        received_at: u64,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.admit_inner(request, received_at, false, TeamWorkDisposition::Queued)
    }

    pub fn admit_bound(
        &mut self,
        request: TeamWorkRequest,
        received_at: u64,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        let binding = self
            .current_binding(&request.binding.binding_id)?
            .ok_or_else(|| TeamWorkError::Invalid("source binding is missing".into()))?;
        if !binding.armed || binding.binding != request.binding {
            return Err(TeamWorkError::DispositionConflict);
        }
        self.admit_inner(request, received_at, true, TeamWorkDisposition::Queued)
    }

    /// Persist a verified unchanged causal event without a transient executable
    /// queue entry, including across a crash during acknowledgement.
    pub fn admit_bound_no_work(
        &mut self,
        request: TeamWorkRequest,
        received_at: u64,
        reason: String,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        bounded_text("causal disposition", &reason, 4096)?;
        let binding = self
            .current_binding(&request.binding.binding_id)?
            .ok_or_else(|| TeamWorkError::Invalid("source binding is missing".into()))?;
        if !binding.armed || binding.binding != request.binding {
            return Err(TeamWorkError::DispositionConflict);
        }
        self.admit_inner(
            request,
            received_at,
            true,
            TeamWorkDisposition::Dismissed { reason },
        )
    }

    pub fn record_blocked(
        &mut self,
        receipt_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamWorkError> {
        self.ensure_usable()?;
        if let Some(reason) = &reason {
            bounded_text("blocked reason", reason, 4096)?;
        }
        let index = self.index(receipt_id)?;
        if self.data.receipts[index].blocked_reason == reason {
            return Ok(());
        }
        let mut next = self.data.clone();
        next.receipts[index].blocked_reason = reason;
        self.commit(next)
    }

    fn admit_inner(
        &mut self,
        request: TeamWorkRequest,
        received_at: u64,
        native_capacity_reserved: bool,
        disposition: TeamWorkDisposition,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.ensure_usable()?;
        validate_request(&request)?;
        if let Some(existing) = self
            .data
            .receipts
            .iter()
            .find(|receipt| event_key(&receipt.request) == event_key(&request))
        {
            return if existing.request == request {
                Ok(existing.clone())
            } else {
                Err(TeamWorkError::EventConflict)
            };
        }
        if self.data.receipts.len() >= MAX_RECEIPTS {
            return Err(TeamWorkError::Capacity);
        }
        let receipt = TeamWorkReceipt {
            receipt_id: format!("work-{}", uuid::Uuid::new_v4()),
            turn_id: format!("turn-{}", uuid::Uuid::new_v4()),
            received_at,
            request,
            disposition,
            execution_source: None,
            native_capacity_reserved,
            allocations: Vec::new(),
            blocked_reason: None,
            never_begun: None,
            ceiling_decision: None,
        };
        let mut next = self.data.clone();
        next.receipts.push(receipt.clone());
        self.commit(next)?;
        Ok(receipt)
    }

    pub fn receipts(&self) -> Result<&[TeamWorkReceipt], TeamWorkError> {
        self.ensure_usable()?;
        Ok(&self.data.receipts)
    }

    /// Reserve the already-recorded turn, never a replacement after restart.
    /// The caller must consult that turn's authoritative ledger before dispatch.
    pub fn reserve_turn(&mut self, receipt_id: &str) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.ensure_usable()?;
        let index = self.index(receipt_id)?;
        if self.data.receipts[index].native_capacity_reserved {
            return Err(TeamWorkError::Invalid(
                "native work must reserve its exact controller request".into(),
            ));
        }
        match self.data.receipts[index].disposition {
            TeamWorkDisposition::Reserved => return Ok(self.data.receipts[index].clone()),
            TeamWorkDisposition::Dismissed { .. } => {
                return Err(TeamWorkError::DispositionConflict)
            }
            TeamWorkDisposition::Queued => {}
        }
        let mut next = self.data.clone();
        next.receipts[index].disposition = TeamWorkDisposition::Reserved;
        let receipt = next.receipts[index].clone();
        self.commit(next)?;
        Ok(receipt)
    }

    pub fn reserve_native_turn(
        &mut self,
        receipt_id: &str,
        source: String,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.reserve_native_turn_scoped(receipt_id, source, None)
    }

    /// Reserve a turn that installs only the named grants, such as a turn
    /// targeted at one Agent. The subset is fixed with the reservation.
    pub fn reserve_native_turn_for_grants(
        &mut self,
        receipt_id: &str,
        source: String,
        grants: &[String],
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.reserve_native_turn_scoped(receipt_id, source, Some(grants))
    }

    fn reserve_native_turn_scoped(
        &mut self,
        receipt_id: &str,
        source: String,
        grants: Option<&[String]>,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.ensure_usable()?;
        if source.is_empty() || source.len() > crate::turn_contract::MAX_CONTRACT_ENVELOPE_BYTES {
            return Err(TeamWorkError::Invalid(
                "native reservation exceeds its input bound".into(),
            ));
        }
        serde_json::from_str::<serde_json::Value>(&source)?;
        let index = self.index(receipt_id)?;
        let receipt = &self.data.receipts[index];
        if let Some(existing) = &receipt.execution_source {
            return if existing == &source && receipt.disposition == TeamWorkDisposition::Reserved {
                Ok(receipt.clone())
            } else {
                Err(TeamWorkError::EventConflict)
            };
        }
        if receipt.disposition != TeamWorkDisposition::Queued {
            return Err(TeamWorkError::DispositionConflict);
        }
        let mut next = self.data.clone();
        next.receipts[index].disposition = TeamWorkDisposition::Reserved;
        next.receipts[index].execution_source = Some(source);
        next.receipts[index].allocations = self.allocate_native_budget(index, grants)?;
        let receipt = next.receipts[index].clone();
        self.commit(next)?;
        Ok(receipt)
    }

    /// Dismiss only unreserved work. A reservation may already have effects;
    /// closing its turn belongs to the execution controller instead.
    pub fn dismiss(
        &mut self,
        receipt_id: &str,
        reason: String,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.ensure_usable()?;
        bounded_text("dismissal reason", &reason, 4096)?;
        let index = self.index(receipt_id)?;
        let disposition = TeamWorkDisposition::Dismissed { reason };
        if self.data.receipts[index].disposition == disposition {
            return Ok(self.data.receipts[index].clone());
        }
        if self.data.receipts[index].disposition != TeamWorkDisposition::Queued {
            return Err(TeamWorkError::DispositionConflict);
        }
        let mut next = self.data.clone();
        next.receipts[index].disposition = disposition;
        let receipt = next.receipts[index].clone();
        self.commit(next)?;
        Ok(receipt)
    }

    /// The host holds both the native admission gate and this inbox writer.
    /// Absence is used only before Begin, where this format has no execution
    /// path. A canonical turn, even interrupted, can never use this cancellation.
    pub fn dismiss_native_never_begun(
        &mut self,
        receipt_id: &str,
        canonical: &crate::execution_store::SessionExecutionStore,
        reason: String,
    ) -> Result<TeamWorkReceipt, TeamWorkError> {
        self.ensure_usable()?;
        bounded_text("dismissal reason", &reason, 4096)?;
        let index = self.index(receipt_id)?;
        let receipt = &self.data.receipts[index];
        let identity = canonical
            .identity()
            .map_err(|error| TeamWorkError::Invalid(error.to_string()))?;
        let turn = crate::turn_contract::LogicalTurnId::new(&receipt.turn_id)
            .map_err(|error| TeamWorkError::Invalid(error.to_string()))?;
        if identity.owner().session_id.as_str() != receipt.request.binding.session_id
            || identity.owner().workspace_id != receipt.request.binding.workspace_id
            || canonical
                .turn(&turn)
                .map_err(|error| TeamWorkError::Invalid(error.to_string()))?
                .is_some()
        {
            return Err(TeamWorkError::DispositionConflict);
        }
        let disposition = TeamWorkDisposition::Dismissed { reason };
        if receipt.disposition == disposition && receipt.never_begun.is_some() {
            return Ok(receipt.clone());
        }
        if receipt.disposition != TeamWorkDisposition::Reserved
            || !receipt.native_capacity_reserved
            || receipt.execution_source.is_none()
            || receipt
                .allocations
                .iter()
                .any(|allocation| allocation.settlement.is_some())
        {
            return Err(TeamWorkError::DispositionConflict);
        }
        let proof = NativeWorkNoDispatch {
            canonical_journal_id: identity.journal_id().to_owned(),
            canonical_record_count: canonical
                .records()
                .map_err(|error| TeamWorkError::Invalid(error.to_string()))?
                .len() as u64,
        };
        let mut next = self.data.clone();
        let cancelled = &mut next.receipts[index];
        cancelled.disposition = disposition;
        cancelled.blocked_reason = None;
        cancelled.never_begun = Some(proof);
        for allocation in &mut cancelled.allocations {
            allocation.settlement = Some(budget::Settlement {
                total_consumed: allocation.consumed_before.clone(),
                revoked: false,
                basis: budget::SettlementBasis::Measured,
            });
        }
        let receipt = cancelled.clone();
        self.commit(next)?;
        Ok(receipt)
    }

    fn index(&self, receipt_id: &str) -> Result<usize, TeamWorkError> {
        self.data
            .receipts
            .iter()
            .position(|receipt| receipt.receipt_id == receipt_id)
            .ok_or_else(|| TeamWorkError::NotFound(receipt_id.to_owned()))
    }

    fn ensure_usable(&self) -> Result<(), TeamWorkError> {
        if self.recovery_required {
            return Err(TeamWorkError::RecoveryRequired);
        }
        self.dir.verify_ambient_identity()?;
        Ok(())
    }

    fn commit(&mut self, next: InboxData) -> Result<(), TeamWorkError> {
        self.commit_with(next, |dir, bytes| dir.atomic_write(FILE_NAME, bytes))
    }

    fn commit_with(
        &mut self,
        next: InboxData,
        write: impl FnOnce(&SecureDir, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), TeamWorkError> {
        let bytes = serde_json::to_vec(&next)?;
        if reserved_size(&next, bytes.len())? > MAX_STORE_BYTES {
            return Err(TeamWorkError::Capacity);
        }
        if let Err(error) = write(&self.dir, &bytes) {
            // Rename may have succeeded before directory fsync failed. In-memory
            // state is now ambiguous; no retry may overwrite that newer receipt.
            self.recovery_required = true;
            return Err(error.into());
        }
        self.data = next;
        Ok(())
    }
}

// Admission reserves enough space to dismiss every queued item with the largest
// permitted JSON-escaped reason. A full inbox cannot strand acknowledged work.
fn reserved_size(data: &InboxData, serialized_size: usize) -> Result<usize, TeamWorkError> {
    let largest = serde_json::to_vec(&TeamWorkDisposition::Dismissed {
        reason: "\\".repeat(4096),
    })?
    .len();
    let queued = serde_json::to_vec(&TeamWorkDisposition::Queued)?.len();
    let native = data
        .receipts
        .iter()
        .filter(|receipt| {
            receipt.native_capacity_reserved
                && receipt.execution_source.is_none()
                && receipt.disposition == TeamWorkDisposition::Queued
        })
        .count()
        * (6 * crate::turn_contract::MAX_CONTRACT_ENVELOPE_BYTES
            + crate::turn_contract::MAX_CONTRACT_NODES * 1024);
    let blocked = data
        .receipts
        .iter()
        .filter(|receipt| !matches!(receipt.disposition, TeamWorkDisposition::Dismissed { .. }))
        .map(|receipt| {
            6 * 4096usize
                - receipt
                    .blocked_reason
                    .as_ref()
                    .map_or(0, |reason| reason.len())
        })
        .sum::<usize>();
    let settlement_bytes = serde_json::to_vec(&budget::Settlement {
        total_consumed: crate::control_authority::GrantUsage {
            activations: u32::MAX,
            invocations: u32::MAX,
            tokens: u64::MAX,
            cost_microunits: u64::MAX,
        },
        revoked: false,
        basis: budget::SettlementBasis::ReservedCeiling {
            unknown_calls: u32::MAX,
        },
    })?
    .len();
    let ceiling_bytes = serde_json::to_vec(&budget::CeilingDecision {
        decided_at_ms: u64::MAX,
    })?
    .len()
        + r#","ceiling_decision":"#.len();
    let possible_ceiling = data
        .receipts
        .iter()
        .filter(|receipt| {
            receipt.disposition == TeamWorkDisposition::Reserved
                && !receipt.allocations.is_empty()
                && receipt.ceiling_decision.is_none()
        })
        .count()
        * ceiling_bytes;
    let proof_bytes = serde_json::to_vec(&NativeWorkNoDispatch {
        canonical_journal_id: "\\".repeat(512),
        canonical_record_count: u64::MAX,
    })?
    .len();
    let pending_settlement = data
        .receipts
        .iter()
        .map(|receipt| {
            receipt
                .allocations
                .iter()
                .filter(|allocation| allocation.settlement.is_none())
                .count()
                * settlement_bytes
        })
        .sum::<usize>();
    let possible_cancellation = data
        .receipts
        .iter()
        .filter(|receipt| {
            receipt.disposition == TeamWorkDisposition::Reserved && receipt.native_capacity_reserved
        })
        .count()
        * (largest + proof_bytes);
    Ok(serialized_size
        + native
        + blocked
        + pending_settlement
        + possible_ceiling
        + possible_cancellation
        + data
            .receipts
            .iter()
            .filter(|receipt| receipt.disposition == TeamWorkDisposition::Queued)
            .count()
            * largest.saturating_sub(queued))
}

fn event_key(request: &TeamWorkRequest) -> (&str, &str, &str) {
    (
        &request.binding.binding_id,
        &request.event.source_id,
        &request.event.event_id,
    )
}

fn bounded_text(name: &str, value: &str, max: usize) -> Result<(), TeamWorkError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(TeamWorkError::Invalid(format!("invalid {name}")));
    }
    Ok(())
}

fn validate_signal_field(
    routes: &[SignalRoute],
    half_life_ms: Option<u64>,
    max_dispatches: u32,
) -> Result<(), TeamWorkError> {
    if routes.is_empty() || routes.len() > MAX_SIGNAL_ROUTES {
        return Err(TeamWorkError::Invalid(
            "a signal field needs between 1 and 32 routes".into(),
        ));
    }
    if half_life_ms.is_some_and(|half_life| half_life < 1000) {
        return Err(TeamWorkError::Invalid(
            "signal half-life must be at least one second".into(),
        ));
    }
    if max_dispatches == 0 || max_dispatches > MAX_SIGNAL_DISPATCHES {
        return Err(TeamWorkError::Invalid(
            "signal dispatch limit must be between 1 and 64".into(),
        ));
    }
    let mut slots = HashSet::new();
    for route in routes {
        bounded_text("signal slot", &route.slot_id, 512)?;
        if !slots.insert(&route.slot_id) {
            return Err(TeamWorkError::Invalid(
                "each slot may appear in one signal route".into(),
            ));
        }
        if route.watches.is_empty() || route.watches.len() > MAX_SIGNAL_WATCHES {
            return Err(TeamWorkError::Invalid(
                "each signal route needs between 1 and 32 watched patterns".into(),
            ));
        }
        if route
            .owns
            .as_ref()
            .is_some_and(|owns| owns.len() > MAX_SIGNAL_WATCHES)
        {
            return Err(TeamWorkError::Invalid(
                "each signal route may own at most 32 path patterns".into(),
            ));
        }
        for pattern in route.watches.iter().chain(route.owns.iter().flatten()) {
            bounded_text("path pattern", pattern, 512)?;
            let body = pattern.strip_suffix('/').unwrap_or(pattern);
            if body.is_empty()
                || pattern.starts_with('/')
                || pattern.contains('\\')
                || body
                    .split('/')
                    .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            {
                return Err(TeamWorkError::Invalid(format!(
                    "{pattern:?} is not a repository path pattern"
                )));
            }
        }
        if route.threshold_milli == 0 || route.threshold_milli > 100_000 {
            return Err(TeamWorkError::Invalid(
                "signal thresholds must be between 0.001 and 100".into(),
            ));
        }
    }
    Ok(())
}

fn validate_binding(entry: &ArmedTeamWorkBinding) -> Result<(), TeamWorkError> {
    standing_check_definitions(&entry.required_checks)?;
    for argv in &entry.required_checks {
        if argv.is_empty()
            || argv[0].is_empty()
            || argv.len() > 64
            || argv
                .iter()
                .any(|arg| arg.len() > 4096 || arg.contains('\0'))
        {
            return Err(TeamWorkError::Invalid(
                "invalid required check arguments".into(),
            ));
        }
    }
    let binding = &entry.binding;
    for (name, value) in [
        ("binding", &binding.binding_id),
        ("workspace", &binding.workspace_id),
        ("session", &binding.session_id),
        ("source", &binding.source_id),
        ("event kind", &binding.event_kind),
        ("grant", &binding.grant_id),
    ] {
        bounded_text(name, value, 512)?;
    }
    crate::turn_contract::SessionId::new(binding.session_id.clone())
        .map_err(|error| TeamWorkError::Invalid(error.to_string()))?;
    if binding.binding_revision == 0
        || binding.team_revision == 0
        || binding.grant_revision == 0
        || entry.grants.is_empty()
        || entry.grants.len() > crate::turn_contract::MAX_CONTRACT_NODES
    {
        return Err(TeamWorkError::Invalid(
            "binding revisions or grant set are invalid".into(),
        ));
    }
    if entry.instruction.trim().is_empty()
        || entry.instruction.len() > MAX_REQUEST_BYTES
        || entry.instruction.contains('\0')
    {
        return Err(TeamWorkError::Invalid(
            "binding instruction is empty or too large".into(),
        ));
    }
    let mut grants = HashSet::new();
    for grant in &entry.grants {
        bounded_text("grant", &grant.id, 512)?;
        if grant.revision == 0
            || grant.expires_at_ms == 0
            || grant.limits.activations == 0
            || grant.limits.invocations == 0
            || grant.limits.tokens == 0
            || !grants.insert(&grant.id)
        {
            return Err(TeamWorkError::Invalid(
                "binding grants are invalid or duplicated".into(),
            ));
        }
    }
    if !entry
        .grants
        .iter()
        .any(|grant| grant.id == binding.grant_id && grant.revision == binding.grant_revision)
    {
        return Err(TeamWorkError::Invalid(
            "primary binding grant is not in its exact grant set".into(),
        ));
    }
    match &entry.source {
        TeamWorkSource::SignedWebhook { configuration_name } => {
            bounded_text("producer configuration", configuration_name, 512)?
        }
        TeamWorkSource::SessionCompletion { session_id } => {
            crate::turn_contract::SessionId::new(session_id.clone())
                .map_err(|error| TeamWorkError::Invalid(error.to_string()))?;
        }
        TeamWorkSource::SignalField {
            routes,
            half_life_ms,
            max_dispatches,
            max_episode_tokens,
        } => {
            validate_signal_field(routes, *half_life_ms, *max_dispatches)?;
            if *max_episode_tokens == Some(0) {
                return Err(TeamWorkError::Invalid(
                    "an episode token budget must be positive".into(),
                ));
            }
        }
        TeamWorkSource::Manual => {}
    }
    if let Some(turn) = &entry.source_after_turn {
        crate::turn_contract::LogicalTurnId::new(turn.clone())
            .map_err(|error| TeamWorkError::Invalid(error.to_string()))?;
    }

    Ok(())
}

fn validate_request(request: &TeamWorkRequest) -> Result<(), TeamWorkError> {
    let binding = &request.binding;
    let event = &request.event;
    for (name, value) in [
        ("binding id", &binding.binding_id),
        ("workspace id", &binding.workspace_id),
        ("session id", &binding.session_id),
        ("grant id", &binding.grant_id),
        ("source id", &event.source_id),
        ("event id", &event.event_id),
        ("event kind", &event.event_kind),
        ("correlation id", &event.correlation_id),
        ("subject kind", &event.subject.kind),
        ("subject reference", &event.subject.reference_id),
        ("subject version", &event.subject.version),
    ] {
        bounded_text(name, value, 512)?;
    }
    if binding.binding_revision == 0 || binding.team_revision == 0 || binding.grant_revision == 0 {
        return Err(TeamWorkError::Invalid("revisions are one-based".into()));
    }
    super::turn_contract::SessionId::new(binding.session_id.clone())
        .map_err(|_| TeamWorkError::Invalid("invalid Session contract identity".into()))?;
    if let Some(cause) = &event.caused_by_turn_id {
        super::turn_contract::LogicalTurnId::new(cause.clone())
            .map_err(|_| TeamWorkError::Invalid("invalid causing-turn identity".into()))?;
    }
    if binding.source_id != event.source_id || binding.event_kind != event.event_kind {
        return Err(TeamWorkError::Invalid(
            "event does not match its binding".into(),
        ));
    }
    if event.content_sha256.len() != 64
        || !event
            .content_sha256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(TeamWorkError::Invalid("invalid event SHA-256".into()));
    }
    if event.evidence_refs.len() > 64 {
        return Err(TeamWorkError::Invalid(
            "too many evidence references".into(),
        ));
    }
    let mut references = HashSet::new();
    for reference in &event.evidence_refs {
        bounded_text("evidence reference", reference, 512)?;
        if !references.insert(reference) {
            return Err(TeamWorkError::Invalid(
                "duplicate evidence reference".into(),
            ));
        }
    }
    if serde_json::to_vec(request)?.len() > MAX_REQUEST_BYTES {
        return Err(TeamWorkError::Invalid("request exceeds byte limit".into()));
    }
    Ok(())
}

fn validate_data(data: &InboxData) -> Result<(), TeamWorkError> {
    if data.schema_version != SCHEMA_VERSION {
        return Err(TeamWorkError::UnsupportedVersion(data.schema_version));
    }
    if data.receipts.len() > MAX_RECEIPTS {
        return Err(TeamWorkError::Capacity);
    }
    if data.bindings.len() > MAX_BINDINGS {
        return Err(TeamWorkError::Capacity);
    }
    let mut owners = std::collections::HashMap::new();
    let mut revisions = std::collections::HashMap::new();
    for binding in &data.bindings {
        validate_binding(binding)?;
        let owner = (&binding.binding.workspace_id, &binding.binding.session_id);
        if owners
            .insert(&binding.binding.binding_id, owner)
            .is_some_and(|previous| previous != owner)
        {
            return Err(TeamWorkError::EventConflict);
        }
        let previous = revisions.entry(&binding.binding.binding_id).or_insert(0u64);
        if previous.checked_add(1) != Some(binding.binding.binding_revision) {
            return Err(TeamWorkError::Invalid(
                "binding revision history is discontinuous".into(),
            ));
        }
        *previous = binding.binding.binding_revision;
    }
    if reserved_size(data, serde_json::to_vec(data)?.len())? > MAX_STORE_BYTES {
        return Err(TeamWorkError::Capacity);
    }
    let mut keys = HashSet::new();
    let mut receipt_ids = HashSet::new();
    let mut turn_ids = HashSet::new();
    for receipt in &data.receipts {
        validate_request(&receipt.request)?;
        if let Some(reason) = &receipt.blocked_reason {
            bounded_text("blocked reason", reason, 4096)?;
        }
        if receipt.execution_source.as_ref().is_some_and(|source| {
            source.is_empty()
                || source.len() > crate::turn_contract::MAX_CONTRACT_ENVELOPE_BYTES
                || (receipt.disposition != TeamWorkDisposition::Reserved
                    && receipt.never_begun.is_none())
        }) {
            return Err(TeamWorkError::Invalid(
                "invalid native turn reservation".into(),
            ));
        }
        if let Some(proof) = &receipt.never_begun {
            bounded_text("nonexecution journal", &proof.canonical_journal_id, 512)?;
            if !matches!(receipt.disposition, TeamWorkDisposition::Dismissed { .. })
                || !receipt.native_capacity_reserved
                || receipt.execution_source.is_none()
                || receipt.allocations.is_empty()
                || receipt.allocations.iter().any(|allocation| {
                    allocation.settlement.as_ref().is_none_or(|settled| {
                        settled.total_consumed != allocation.consumed_before || settled.revoked
                    })
                })
            {
                return Err(TeamWorkError::DispositionConflict);
            }
        }
        if receipt.native_capacity_reserved
            && !data
                .bindings
                .iter()
                .any(|entry| entry.binding == receipt.request.binding)
        {
            return Err(TeamWorkError::Invalid(
                "native receipt lost its exact retained binding".into(),
            ));
        }
        bounded_text("receipt id", &receipt.receipt_id, 512)?;
        super::turn_contract::LogicalTurnId::new(receipt.turn_id.clone())
            .map_err(|_| TeamWorkError::Invalid("invalid reserved-turn identity".into()))?;
        if !keys.insert(event_key(&receipt.request))
            || !receipt_ids.insert(&receipt.receipt_id)
            || !turn_ids.insert(&receipt.turn_id)
        {
            return Err(TeamWorkError::Invalid(
                "duplicate persisted identity".into(),
            ));
        }
        if let TeamWorkDisposition::Dismissed { reason } = &receipt.disposition {
            bounded_text("dismissal reason", reason, 4096)?;
        }
    }
    budget::validate_allocations(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TeamWorkRequest {
        serde_json::from_str(include_str!("../tests/fixtures/qa-build-event.v1.json")).unwrap()
    }

    #[test]
    fn duplicate_delivery_and_restart_keep_the_same_work_and_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let original = inbox.admit(request(), 100).unwrap();
        assert_eq!(inbox.admit(request(), 200).unwrap(), original);
        drop(inbox);

        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(inbox.admit(request(), 300).unwrap(), original);
        let reserved = inbox.reserve_turn(&original.receipt_id).unwrap();
        assert_eq!(reserved.turn_id, original.turn_id);
        assert_eq!(reserved.request, original.request);
        assert_eq!(reserved.disposition, TeamWorkDisposition::Reserved);
        drop(inbox);

        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(inbox.reserve_turn(&original.receipt_id).unwrap(), reserved);
        assert_eq!(inbox.admit(request(), 400).unwrap(), reserved);
        assert_eq!(inbox.receipts().unwrap().len(), 1);
        assert!(matches!(
            inbox.dismiss(&original.receipt_id, "ignore this build".into()),
            Err(TeamWorkError::DispositionConflict)
        ));
    }

    #[test]
    fn redelivery_cannot_retarget_work_or_change_its_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let original = inbox.admit(request(), 100).unwrap();
        let mut changed = request();
        changed.binding.session_id = "another-client-session".into();
        assert!(matches!(
            inbox.admit(changed, 200),
            Err(TeamWorkError::EventConflict)
        ));
        let mut changed = request();
        changed.binding.team_revision += 1;
        assert!(matches!(
            inbox.admit(changed, 200),
            Err(TeamWorkError::EventConflict)
        ));
        let mut changed = request();
        changed.event.subject.version = "git:a-new-build".into();
        assert!(matches!(
            inbox.admit(changed, 200),
            Err(TeamWorkError::EventConflict)
        ));
        let mut changed = request();
        changed.event.content_sha256 = "b".repeat(64);
        assert!(matches!(
            inbox.admit(changed, 200),
            Err(TeamWorkError::EventConflict)
        ));
        assert_eq!(inbox.receipts().unwrap(), &[original]);
    }

    #[test]
    fn independent_bindings_admit_the_same_event_without_sharing_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let first = inbox.admit(request(), 100).unwrap();
        let mut other = request();
        other.binding.binding_id = "qa-second-client".into();
        other.binding.workspace_id = "workspace-second-client".into();
        other.binding.session_id = "session-second-client".into();
        let second = inbox.admit(other, 100).unwrap();
        assert_ne!(first.receipt_id, second.receipt_id);
        assert_ne!(first.turn_id, second.turn_id);
        assert_eq!(first.request.event, second.request.event);
    }

    #[test]
    fn dismissal_survives_repeated_delivery_and_cannot_be_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let original = inbox.admit(request(), 100).unwrap();
        let dismissed = inbox
            .dismiss(&original.receipt_id, "release withdrawn".into())
            .unwrap();
        drop(inbox);
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(inbox.admit(request(), 200).unwrap(), dismissed);
        assert_eq!(
            inbox
                .dismiss(&original.receipt_id, "release withdrawn".into())
                .unwrap(),
            dismissed
        );
        assert!(matches!(
            inbox.reserve_turn(&original.receipt_id),
            Err(TeamWorkError::DispositionConflict)
        ));
        assert!(matches!(
            inbox.dismiss(&original.receipt_id, "new reason".into()),
            Err(TeamWorkError::DispositionConflict)
        ));
    }

    #[test]
    fn malformed_or_mismatched_events_are_not_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut bad = request();
        bad.event.source_id = "unselected-source".into();
        assert!(inbox.admit(bad, 1).is_err());
        let mut bad = request();
        bad.event.event_kind = "release_approved".into();
        assert!(inbox.admit(bad, 1).is_err());
        let mut bad = request();
        bad.binding.team_revision = 0;
        assert!(inbox.admit(bad, 1).is_err());
        let mut bad = request();
        bad.binding.session_id = "not/a/session-identity".into();
        assert!(inbox.admit(bad, 1).is_err());
        let mut bad = request();
        bad.event.caused_by_turn_id = Some("wrong/turn".into());
        assert!(inbox.admit(bad, 1).is_err());
        let mut bad = request();
        bad.event.content_sha256 = "a mutable preview URL".into();
        assert!(inbox.admit(bad, 1).is_err());
        let mut bad = request();
        bad.event.evidence_refs = vec!["x".repeat(513)];
        assert!(inbox.admit(bad, 1).is_err());
        assert!(inbox.receipts().unwrap().is_empty());
        let stored: InboxData =
            serde_json::from_slice(&std::fs::read(inbox.path()).unwrap()).unwrap();
        assert!(stored.receipts.is_empty());
    }

    #[test]
    fn unknown_schema_or_duplicate_disk_identity_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        inbox.admit(request(), 1).unwrap();
        let path = inbox.path();
        drop(inbox);
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut unknown = original.clone();
        unknown["schema_version"] = 99.into();
        std::fs::write(&path, serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(matches!(
            TeamWorkInbox::open(dir.path()),
            Err(TeamWorkError::UnsupportedVersion(99))
        ));
        let mut duplicated = original.clone();
        duplicated["receipts"]
            .as_array_mut()
            .unwrap()
            .push(original["receipts"][0].clone());
        std::fs::write(&path, serde_json::to_vec(&duplicated).unwrap()).unwrap();
        assert!(matches!(
            TeamWorkInbox::open(dir.path()),
            Err(TeamWorkError::Invalid(_))
        ));
        std::fs::write(&path, b"{partial").unwrap();
        assert!(matches!(
            TeamWorkInbox::open(dir.path()),
            Err(TeamWorkError::Serde(_))
        ));
    }

    #[test]
    fn full_inbox_preserves_receipts_and_still_recognizes_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let original = inbox.admit(request(), 1).unwrap();
        let path = inbox.path();
        let mut data = inbox.data.clone();
        drop(inbox);
        for i in 1..MAX_RECEIPTS {
            let mut receipt = original.clone();
            receipt.receipt_id = format!("work-{i}");
            receipt.turn_id = format!("turn-{i}");
            receipt.request.event.event_id = format!("build-{i}");
            receipt.disposition = TeamWorkDisposition::Dismissed {
                reason: "Earlier work deliberately dismissed".into(),
            };
            data.receipts.push(receipt);
        }
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(inbox.admit(request(), 2).unwrap(), original);
        let mut extra = request();
        extra.event.event_id = "one-more-build".into();
        assert!(matches!(
            inbox.admit(extra, 2),
            Err(TeamWorkError::Capacity)
        ));
        assert_eq!(inbox.receipts().unwrap().len(), MAX_RECEIPTS);
    }

    #[cfg(unix)]
    #[test]
    fn second_writer_cannot_open_even_if_a_child_lock_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = TeamWorkInbox::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("lock"), b"replacement").unwrap();
        assert!(TeamWorkInbox::open(dir.path()).is_err());
        drop(inbox);
        assert!(TeamWorkInbox::open(dir.path()).is_ok());
    }

    #[test]
    fn missing_storage_ancestors_are_not_implicitly_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-yet-durable").join("inbox");
        assert!(TeamWorkInbox::open(&missing).is_err());
        assert!(!missing.parent().unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn ambiguous_rename_is_durably_reconciled_before_duplicate_acknowledgement() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let original = inbox.admit(request(), 1).unwrap();
        let mut next = inbox.data.clone();
        next.receipts[0].disposition = TeamWorkDisposition::Reserved;
        // Simulate an uncertain return after publication, when fsync could
        // have failed. The disk state and last acknowledged state diverge.
        let result = inbox.commit_with(next, |dir, bytes| {
            dir.atomic_write(FILE_NAME, bytes)?;
            Err(std::io::Error::other("injected failure after rename"))
        });
        assert!(matches!(result, Err(TeamWorkError::Io(_))));
        assert!(matches!(
            inbox.reserve_turn(&original.receipt_id),
            Err(TeamWorkError::RecoveryRequired)
        ));
        let prior_inode = std::fs::metadata(inbox.path()).unwrap().ino();
        drop(inbox);

        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        // Reopen performs a successful atomic rewrite, including a new
        // directory fsync. Reading the uncertain file alone is insufficient.
        assert_ne!(std::fs::metadata(inbox.path()).unwrap().ino(), prior_inode);
        let repeated = inbox.admit(request(), 2).unwrap();
        assert_eq!(repeated.turn_id, original.turn_id);
        assert_eq!(repeated.disposition, TeamWorkDisposition::Reserved);
        assert_eq!(inbox.receipts().unwrap().len(), 1);
    }

    #[test]
    fn byte_capacity_keeps_room_to_reserve_and_dismiss_acknowledged_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut large_request = request();
        large_request.event.evidence_refs = (0..48)
            .map(|i| format!("{i:02}{}", "a".repeat(510)))
            .collect();
        let first = inbox.admit(large_request.clone(), 1).unwrap();
        let path = inbox.path();
        let mut data = inbox.data.clone();
        drop(inbox);
        for i in 1..MAX_RECEIPTS {
            let mut receipt = first.clone();
            receipt.receipt_id = format!("work-{i}");
            receipt.turn_id = format!("turn-{i}");
            receipt.request.event.event_id = format!("build-{i}");
            let mut next = data.clone();
            next.receipts.push(receipt);
            if reserved_size(&next, serde_json::to_vec(&next).unwrap().len()).unwrap()
                > MAX_STORE_BYTES
            {
                break;
            }
            data = next;
        }
        assert!(data.receipts.len() < MAX_RECEIPTS);
        let second_id = data.receipts[1].receipt_id.clone();
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        large_request.event.event_id = "new-build-at-capacity".into();
        assert!(matches!(
            inbox.admit(large_request, 2),
            Err(TeamWorkError::Capacity)
        ));
        inbox.reserve_turn(&first.receipt_id).unwrap();
        inbox.dismiss(&second_id, "\\".repeat(4096)).unwrap();
        drop(inbox);
        assert_eq!(
            TeamWorkInbox::open(dir.path())
                .unwrap()
                .receipts()
                .unwrap()
                .len(),
            data.receipts.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_never_acknowledges_and_requires_reopen() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"untouched").unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let original = std::fs::read(inbox.path()).unwrap();
        std::fs::remove_file(inbox.path()).unwrap();
        symlink(outside.path(), inbox.path()).unwrap();
        assert!(matches!(
            inbox.admit(request(), 1),
            Err(TeamWorkError::Io(_))
        ));
        assert!(matches!(
            inbox.receipts(),
            Err(TeamWorkError::RecoveryRequired)
        ));
        assert!(matches!(
            inbox.admit(request(), 2),
            Err(TeamWorkError::RecoveryRequired)
        ));
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"untouched");
        std::fs::remove_file(inbox.path()).unwrap();
        drop(inbox);
        assert!(TeamWorkInbox::open(dir.path()).is_err());
        std::fs::write(dir.path().join(FILE_NAME), original).unwrap();
        let inbox = TeamWorkInbox::open(dir.path()).unwrap();
        assert!(inbox.receipts().unwrap().is_empty());
    }

    #[test]
    fn armed_binding_and_exact_native_reservation_survive_restart_and_disarm() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let request = request();
        let first = ArmedTeamWorkBinding {
            binding: request.binding.clone(),
            source: TeamWorkSource::Manual,
            armed: true,
            instruction: "Inspect the exact candidate\nand retain actual checks.".into(),
            required_checks: Vec::new(),
            grants: vec![TeamWorkGrantReference {
                id: request.binding.grant_id.clone(),
                revision: request.binding.grant_revision,
                limits: crate::control_authority::GrantLimits {
                    activations: 4,
                    invocations: 16,
                    tokens: 10000,
                    cost_microunits: 0,
                },
                expires_at_ms: u64::MAX,
            }],
            authorized_at_ms: 1,
            source_after_turn: None,
        };
        // Fixture binding revisions may be greater than one; a real binding
        // starts at one, while the source/team/grant versions remain exact.
        let mut first = first;
        first.binding.binding_revision = 1;
        inbox.configure_binding(0, first.clone()).unwrap();
        let mut request = request;
        request.binding = first.binding.clone();
        let admitted = inbox.admit_bound(request.clone(), 2).unwrap();
        assert!(inbox.reserve_turn(&admitted.receipt_id).is_err());
        let source = serde_json::json!({"turn_id":admitted.turn_id,"immutable":"native admission"})
            .to_string();
        let reserved = inbox
            .reserve_native_turn(&admitted.receipt_id, source.clone())
            .unwrap();
        assert_eq!(reserved.turn_id, admitted.turn_id);
        let mut disarmed = first.clone();
        disarmed.binding.binding_revision = 2;
        disarmed.armed = false;
        inbox.configure_binding(1, disarmed).unwrap();
        assert!(inbox.admit_bound(request.clone(), 3).is_err());
        drop(inbox);
        let mut reopened = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(
            reopened
                .reserve_native_turn(&admitted.receipt_id, source)
                .unwrap(),
            reserved
        );
        assert_eq!(reopened.bindings().unwrap().len(), 2);
        assert_eq!(
            reopened.receipts().unwrap()[0].request.binding,
            first.binding
        );
    }

    #[test]
    fn distinct_events_share_one_allowance_and_unsettled_or_revoked_work_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut request = request();
        request.binding.binding_revision = 1;
        let grant = TeamWorkGrantReference {
            id: request.binding.grant_id.clone(),
            revision: request.binding.grant_revision,
            limits: crate::control_authority::GrantLimits {
                activations: 3,
                invocations: 10,
                tokens: 100,
                cost_microunits: 0,
            },
            expires_at_ms: u64::MAX,
        };
        inbox
            .configure_binding(
                0,
                ArmedTeamWorkBinding {
                    binding: request.binding.clone(),
                    source: TeamWorkSource::Manual,
                    armed: true,
                    instruction: "Check the candidate".into(),
                    required_checks: Vec::new(),
                    grants: vec![grant.clone()],
                    authorized_at_ms: 1,
                    source_after_turn: None,
                },
            )
            .unwrap();
        let first = inbox.admit_bound(request.clone(), 2).unwrap();
        request.event.event_id = "second-distinct-build".into();
        let second = inbox.admit_bound(request, 3).unwrap();
        inbox
            .reserve_native_turn(&first.receipt_id, "{\"first\":true}".into())
            .unwrap();
        assert!(inbox
            .reserve_native_turn(&second.receipt_id, "{\"second\":true}".into())
            .is_err());
        let first_allocation = inbox
            .native_allocations(&first.receipt_id)
            .unwrap()
            .remove(0);
        let settled = TeamWorkGrantSettlement {
            receipt_id: first.receipt_id.clone(),
            session_id: first.request.binding.session_id.clone(),
            turn_id: first.turn_id.clone(),
            grant: grant.clone(),
            consumed_before: first_allocation.allocation.consumed_before,
            settled: budget::Settlement {
                total_consumed: crate::control_authority::GrantUsage {
                    activations: 1,
                    invocations: 2,
                    tokens: 80,
                    cost_microunits: 0,
                },
                revoked: false,
                basis: budget::SettlementBasis::Measured,
            },
        };
        inbox
            .settle_native_budget(&first.receipt_id, &[settled])
            .unwrap();
        inbox
            .reserve_native_turn(&second.receipt_id, "{\"second\":true}".into())
            .unwrap();
        let allocations = inbox.native_allocations(&second.receipt_id).unwrap();
        assert_eq!(allocations[0].allocation.consumed_before.tokens, 80);
        drop(inbox);
        let mut reopened = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(
            reopened.native_allocations(&second.receipt_id).unwrap()[0]
                .allocation
                .consumed_before
                .tokens,
            80
        );
        let second_allocation = reopened
            .native_allocations(&second.receipt_id)
            .unwrap()
            .remove(0);
        let revoked = TeamWorkGrantSettlement {
            receipt_id: second.receipt_id.clone(),
            session_id: second.request.binding.session_id.clone(),
            turn_id: second.turn_id.clone(),
            grant,
            consumed_before: second_allocation.allocation.consumed_before,
            settled: budget::Settlement {
                total_consumed: crate::control_authority::GrantUsage {
                    activations: 2,
                    invocations: 4,
                    tokens: 90,
                    cost_microunits: 0,
                },
                revoked: true,
                basis: budget::SettlementBasis::Measured,
            },
        };
        reopened
            .settle_native_budget(&second.receipt_id, &[revoked])
            .unwrap();
        let mut request = second.request.clone();
        request.event.event_id = "after-revocation".into();
        let third = reopened.admit_bound(request, 4).unwrap();
        assert!(reopened
            .reserve_native_turn(&third.receipt_id, "{\"third\":true}".into())
            .is_err());
    }
    #[test]
    fn a_ceiling_settlement_must_fit_the_reviewed_grant_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut request = request();
        request.binding.binding_revision = 1;
        let grant = TeamWorkGrantReference {
            id: request.binding.grant_id.clone(),
            revision: request.binding.grant_revision,
            limits: crate::control_authority::GrantLimits {
                activations: 3,
                invocations: 10,
                tokens: 100,
                cost_microunits: 0,
            },
            expires_at_ms: u64::MAX,
        };
        inbox
            .configure_binding(
                0,
                ArmedTeamWorkBinding {
                    binding: request.binding.clone(),
                    source: TeamWorkSource::Manual,
                    armed: true,
                    instruction: "Check the candidate".into(),
                    required_checks: Vec::new(),
                    grants: vec![grant.clone()],
                    authorized_at_ms: 1,
                    source_after_turn: None,
                },
            )
            .unwrap();
        let work = inbox.admit_bound(request, 2).unwrap();
        assert!(
            inbox.record_ceiling_decision(&work.receipt_id, 3).is_err(),
            "no decision before the work holds a budget"
        );
        inbox
            .reserve_native_turn(&work.receipt_id, "{\"work\":true}".into())
            .unwrap();
        let recorded = inbox.record_ceiling_decision(&work.receipt_id, 4).unwrap();
        assert_eq!(recorded.ceiling_decision.as_ref().unwrap().decided_at_ms, 4);
        assert_eq!(
            inbox
                .record_ceiling_decision(&work.receipt_id, 9)
                .unwrap()
                .ceiling_decision
                .unwrap()
                .decided_at_ms,
            4,
            "a repeated decision keeps the first"
        );
        let allocation = inbox
            .native_allocations(&work.receipt_id)
            .unwrap()
            .remove(0);
        let at = |tokens| TeamWorkGrantSettlement {
            receipt_id: work.receipt_id.clone(),
            session_id: work.request.binding.session_id.clone(),
            turn_id: work.turn_id.clone(),
            grant: grant.clone(),
            consumed_before: allocation.allocation.consumed_before.clone(),
            settled: budget::Settlement {
                total_consumed: crate::control_authority::GrantUsage {
                    activations: 1,
                    invocations: 2,
                    tokens,
                    cost_microunits: 0,
                },
                revoked: true,
                basis: budget::SettlementBasis::ReservedCeiling { unknown_calls: 1 },
            },
        };
        // An expanded grant can reserve past the reviewed limit; persisting that
        // total would make the inbox unreadable, so it is refused and nothing
        // changes.
        assert!(inbox
            .check_native_settlement(&work.receipt_id, &[at(150)])
            .is_err());
        assert!(inbox
            .settle_native_budget(&work.receipt_id, &[at(150)])
            .is_err());
        assert!(!inbox.receipts().unwrap()[0].allocations[0].is_settled());
        inbox
            .check_native_settlement(&work.receipt_id, &[at(100)])
            .unwrap();
        inbox
            .settle_native_budget(&work.receipt_id, &[at(100)])
            .unwrap();
        drop(inbox);
        let reopened = TeamWorkInbox::open(dir.path()).unwrap();
        let receipt = reopened.receipts().unwrap()[0].clone();
        assert_eq!(receipt.ceiling_decision.unwrap().decided_at_ms, 4);
        assert_eq!(
            receipt.allocations[0].settlement.as_ref().unwrap().basis,
            budget::SettlementBasis::ReservedCeiling { unknown_calls: 1 }
        );
    }

    #[test]
    fn targeted_work_reserves_only_its_agent_grant_with_shared_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut request = request();
        request.binding.binding_revision = 1;
        let limits = crate::control_authority::GrantLimits {
            activations: 3,
            invocations: 10,
            tokens: 100,
            cost_microunits: 0,
        };
        let owner = TeamWorkGrantReference {
            id: request.binding.grant_id.clone(),
            revision: request.binding.grant_revision,
            limits: limits.clone(),
            expires_at_ms: u64::MAX,
        };
        let reviewer = TeamWorkGrantReference {
            id: "reviewer-grant".into(),
            revision: 1,
            limits,
            expires_at_ms: u64::MAX,
        };
        inbox
            .configure_binding(
                0,
                ArmedTeamWorkBinding {
                    binding: request.binding.clone(),
                    source: TeamWorkSource::Manual,
                    armed: true,
                    instruction: "Respond to signals".into(),
                    required_checks: Vec::new(),
                    grants: vec![owner.clone(), reviewer.clone()],
                    authorized_at_ms: 1,
                    source_after_turn: None,
                },
            )
            .unwrap();
        let first = inbox.admit_bound(request.clone(), 2).unwrap();
        request.event.event_id = "second".into();
        let second = inbox.admit_bound(request.clone(), 3).unwrap();
        assert!(inbox
            .reserve_native_turn_for_grants(&first.receipt_id, "{}".into(), &["invented".into()])
            .is_err());
        assert!(inbox
            .reserve_native_turn_for_grants(&first.receipt_id, "{}".into(), &[])
            .is_err());
        let reserved = inbox
            .reserve_native_turn_for_grants(
                &first.receipt_id,
                "{\"target\":\"reviewer\"}".into(),
                &["reviewer-grant".into()],
            )
            .unwrap();
        assert_eq!(reserved.allocations.len(), 1);
        assert_eq!(reserved.allocations[0].grant, reviewer);
        // The untouched owner grant has no unresolved allocation, but the
        // reviewer grant must settle before it can be allocated again.
        assert!(inbox
            .reserve_native_turn(&second.receipt_id, "{\"whole\":true}".into())
            .is_err());
        let allocation = inbox
            .native_allocations(&first.receipt_id)
            .unwrap()
            .remove(0);
        inbox
            .settle_native_budget(
                &first.receipt_id,
                &[TeamWorkGrantSettlement {
                    receipt_id: first.receipt_id.clone(),
                    session_id: first.request.binding.session_id.clone(),
                    turn_id: first.turn_id.clone(),
                    grant: reviewer.clone(),
                    consumed_before: allocation.allocation.consumed_before,
                    settled: budget::Settlement {
                        total_consumed: crate::control_authority::GrantUsage {
                            activations: 1,
                            invocations: 3,
                            tokens: 40,
                            cost_microunits: 0,
                        },
                        revoked: false,
                        basis: budget::SettlementBasis::Measured,
                    },
                }],
            )
            .unwrap();
        let whole = inbox
            .reserve_native_turn(&second.receipt_id, "{\"whole\":true}".into())
            .unwrap();
        assert_eq!(whole.allocations.len(), 2);
        assert_eq!(whole.allocations[0].consumed_before.tokens, 0);
        assert_eq!(whole.allocations[1].consumed_before.tokens, 40);
        drop(inbox);
        let reopened = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(reopened.receipts().unwrap()[0].allocations.len(), 1);
        assert_eq!(reopened.receipts().unwrap()[1].allocations.len(), 2);
    }
    #[test]
    fn causal_no_work_and_queue_failure_are_durable_dispositions() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut request = request();
        request.binding.binding_revision = 1;
        request.event.caused_by_turn_id = Some("verified-parent".into());
        let mut binding = ArmedTeamWorkBinding {
            binding: request.binding.clone(),
            source: TeamWorkSource::SessionCompletion {
                session_id: request.binding.session_id.clone(),
            },
            armed: true,
            instruction: "Review changed candidates".into(),
            required_checks: vec![],
            grants: vec![TeamWorkGrantReference {
                id: request.binding.grant_id.clone(),
                revision: request.binding.grant_revision,
                limits: crate::control_authority::GrantLimits {
                    activations: 3,
                    invocations: 10,
                    tokens: 100,
                    cost_microunits: 0,
                },
                expires_at_ms: u64::MAX,
            }],
            authorized_at_ms: 1,
            source_after_turn: Some("prior-turn".into()),
        };
        binding.binding.source_id = format!("session:{}", binding.binding.session_id);
        request.binding = binding.binding.clone();
        request.event.source_id = binding.binding.source_id.clone();
        inbox.configure_binding(0, binding).unwrap();
        let no_work = inbox
            .admit_bound_no_work(request.clone(), 2, "Verified unchanged candidate".into())
            .unwrap();
        assert!(matches!(
            no_work.disposition,
            TeamWorkDisposition::Dismissed { .. }
        ));
        assert!(inbox
            .reserve_native_turn(&no_work.receipt_id, "{}".into())
            .is_err());
        request.event.event_id = "changed-candidate".into();
        let queued = inbox.admit_bound(request.clone(), 3).unwrap();
        inbox
            .record_blocked(
                &queued.receipt_id,
                Some("The actual provider is unavailable".into()),
            )
            .unwrap();
        drop(inbox);
        let mut reopened = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(reopened.receipts().unwrap()[0], no_work);
        let repeated = reopened.admit_bound(request, 4).unwrap();
        assert_eq!(repeated.turn_id, queued.turn_id);
        assert_eq!(
            repeated.blocked_reason.as_deref(),
            Some("The actual provider is unavailable")
        );
        reopened.record_blocked(&queued.receipt_id, None).unwrap();
        assert!(reopened.receipts().unwrap()[1].blocked_reason.is_none());
    }

    #[test]
    fn signal_field_binding_is_durable_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut request = request();
        request.binding.binding_revision = 1;
        request.binding.source_id = "signals".into();
        let route = |slot: &str, watches: &[&str], threshold_milli| SignalRoute {
            slot_id: slot.into(),
            watches: watches.iter().map(|w| (*w).into()).collect(),
            threshold_milli,
            owns: None,
        };
        let owning = |slot: &str, watches: &[&str], owns: &[&str]| SignalRoute {
            owns: Some(owns.iter().map(|w| (*w).into()).collect()),
            ..route(slot, watches, 500)
        };
        let binding = |source| ArmedTeamWorkBinding {
            binding: request.binding.clone(),
            source,
            armed: true,
            instruction: "Respond to signals on your area".into(),
            required_checks: vec![vec!["npm".into(), "run".into(), "check".into()]],
            grants: vec![TeamWorkGrantReference {
                id: request.binding.grant_id.clone(),
                revision: request.binding.grant_revision,
                limits: crate::control_authority::GrantLimits {
                    activations: 3,
                    invocations: 10,
                    tokens: 100,
                    cost_microunits: 0,
                },
                expires_at_ms: u64::MAX,
            }],
            authorized_at_ms: 1,
            source_after_turn: None,
        };
        let field = |routes, half_life_ms, max_dispatches| TeamWorkSource::SignalField {
            routes,
            half_life_ms,
            max_dispatches,
            max_episode_tokens: None,
        };
        for invalid in [
            field(vec![], None, 4),
            field(vec![route("coder", &["lib/"], 1000)], Some(10), 4),
            field(vec![route("coder", &["lib/"], 1000)], None, 0),
            field(vec![route("coder", &["lib/"], 0)], None, 4),
            field(vec![route("coder", &["../lib"], 1000)], None, 4),
            field(vec![route("coder", &["/etc"], 1000)], None, 4),
            field(vec![route("coder", &[], 1000)], None, 4),
            field(vec![owning("coder", &["lib/"], &["../x"])], None, 4),
            field(
                vec![
                    route("coder", &["lib/"], 1000),
                    route("coder", &["test/"], 1000),
                ],
                None,
                4,
            ),
        ] {
            assert!(inbox.configure_binding(0, binding(invalid)).is_err());
        }
        let valid = binding(field(
            vec![
                route("coder", &["lib/orders.js"], 1000),
                owning("reviewer", &["lib/", "*.test.js"], &[]),
            ],
            Some(30 * 60 * 1000),
            6,
        ));
        let TeamWorkSource::SignalField { routes, .. } = &valid.source else {
            unreachable!()
        };
        assert_eq!(routes[0].owned(), ["lib/orders.js".to_string()]);
        assert!(routes[1].owned().is_empty());
        inbox.configure_binding(0, valid.clone()).unwrap();
        drop(inbox);
        let reopened = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(
            reopened
                .current_binding(&valid.binding.binding_id)
                .unwrap()
                .unwrap(),
            &valid
        );
        let json = serde_json::to_value(&valid.source).unwrap();
        assert_eq!(json["kind"], "signal_field");
        assert_eq!(json["routes"][1]["threshold_milli"], 500);
    }

    #[test]
    fn reserved_work_can_be_cancelled_only_with_actual_never_begun_canonical_state() {
        use crate::execution_ownership::LegacyFormatOwnership;
        use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
        use crate::turn_contract::*;
        let dir = tempfile::tempdir().unwrap();
        let execution = tempfile::tempdir().unwrap();
        let mut inbox = TeamWorkInbox::open(dir.path()).unwrap();
        let mut request = request();
        request.binding.binding_revision = 1;
        let grant = TeamWorkGrantReference {
            id: request.binding.grant_id.clone(),
            revision: request.binding.grant_revision,
            limits: crate::control_authority::GrantLimits {
                activations: 3,
                invocations: 10,
                tokens: 100,
                cost_microunits: 0,
            },
            expires_at_ms: u64::MAX,
        };
        inbox
            .configure_binding(
                0,
                ArmedTeamWorkBinding {
                    binding: request.binding.clone(),
                    source: TeamWorkSource::Manual,
                    armed: true,
                    instruction: "Review candidate".into(),
                    required_checks: vec![],
                    grants: vec![grant],
                    authorized_at_ms: 1,
                    source_after_turn: None,
                },
            )
            .unwrap();
        let first = inbox.admit_bound(request.clone(), 2).unwrap();
        inbox
            .reserve_native_turn(&first.receipt_id, "{\"exact\":true}".into())
            .unwrap();
        assert!(inbox
            .dismiss(&first.receipt_id, "Changed source".into())
            .is_err());
        let mut canonical = SessionExecutionStore::open(
            std::sync::Arc::new(
                LegacyFormatOwnership::acquire(execution.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            ),
            ExecutionStoreOwner {
                workspace_id: request.binding.workspace_id.clone(),
                session_id: SessionId::new(&request.binding.session_id).unwrap(),
            },
        )
        .unwrap();
        let cancelled = inbox
            .dismiss_native_never_begun(&first.receipt_id, &canonical, "Changed source".into())
            .unwrap();
        assert!(cancelled.never_begun.is_some());
        assert_eq!(
            cancelled.execution_source.as_deref(),
            Some("{\"exact\":true}")
        );
        assert_eq!(
            cancelled.allocations[0]
                .settlement
                .as_ref()
                .unwrap()
                .total_consumed,
            crate::control_authority::GrantUsage::default()
        );
        request.event.event_id = "next".into();
        let next = inbox.admit_bound(request, 3).unwrap();
        inbox
            .reserve_native_turn(&next.receipt_id, "{\"next\":true}".into())
            .unwrap();
        assert_eq!(
            inbox.native_allocations(&next.receipt_id).unwrap()[0]
                .allocation
                .consumed_before,
            crate::control_authority::GrantUsage::default()
        );
        let fixture:serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json")).unwrap();
        let mut begin: TurnContractEnvelope =
            serde_json::from_value(fixture["steps"][0]["envelope"].clone()).unwrap();
        begin.session_id = SessionId::new(&next.request.binding.session_id).unwrap();
        begin.turn_id = LogicalTurnId::new(&next.turn_id).unwrap();
        let mut content = crate::execution_content::ExecutionContentStore::open_owned(
            canonical
                .component_namespace(
                    crate::execution_namespace::ExecutionComponent::ExecutionContent,
                )
                .unwrap(),
        )
        .unwrap();
        let request = content
            .retain_request(crate::execution_content::ExecutionRequestContent {
                turn_id: begin.turn_id.clone(),
                recorded_at_unix_ms: 4,
                display_input: "Actual Begin".into(),
                effective_input: "Actual Begin".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        canonical.begin_with_request(begin, &request).unwrap();
        assert!(inbox
            .dismiss_native_never_begun(
                &next.receipt_id,
                &canonical,
                "Cannot prove no execution".into()
            )
            .is_err());
        drop(inbox);
        let reopened = TeamWorkInbox::open(dir.path()).unwrap();
        assert_eq!(reopened.receipts().unwrap()[0], cancelled);
        assert_eq!(
            reopened.receipts().unwrap()[1].disposition,
            TeamWorkDisposition::Reserved
        );
    }
}
