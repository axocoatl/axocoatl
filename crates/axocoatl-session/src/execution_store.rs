//! Canonical v2 logical-turn history under a held format-ownership boundary.
//!
//! One atomic Session journal owns all of its turn identities. It prevents a
//! second unfinished turn, resolves closed predecessors against retained history,
//! and reserves bounded settlement space before admitting more work. Reopening
//! durably interrupts a running epoch; it never resumes dispatch automatically.
//! This storage capability is not proof that orphaned external work is settled.
//! The live daemon must establish execution readiness separately before using it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

use crate::execution_content::{
    DurableExecutionRequest, DurableLegacyHistory, LegacyTurnPredecessor,
};
use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_ownership::{OwnershipError, UpgradedFormatOwnership};
use crate::turn_contract::{
    ActivationState, CommandId, EffectDisposition, EvidenceRef, LogicalTurnId, LogicalTurnState,
    SessionId, TurnContract, TurnContractEnvelope, TurnContractError, TurnContractEvent,
    MAX_CONTRACT_COMMANDS, MAX_CONTRACT_ENVELOPE_BYTES, MAX_RETAINED_CONTRACT_BYTES,
    TURN_CONTRACT_SCHEMA_VERSION,
};

const FILE: &str = "execution.v2.json";
const MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;
const MAX_SESSION_RECORDS: usize = 65_536;
const MAX_SESSION_TURNS: usize = 256;
/// Settlement contains bounded identities/references, never raw tool output.
const SMALL_SETTLEMENT_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ExecutionStoreError {
    #[error("execution storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("execution JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("execution ownership: {0}")]
    Ownership(#[from] OwnershipError),
    #[error("execution contract: {0}")]
    Contract(#[from] TurnContractError),
    #[error("legacy history: {0}")]
    LegacyHistory(String),
    #[error("invalid execution journal: {0}")]
    Invalid(&'static str),
    #[error("Session already has an unfinished logical turn")]
    UnfinishedTurn,
    #[error("execution admission would consume reserved settlement capacity")]
    Capacity,
    #[error("execution write failed; reopen before acknowledging or doing more work")]
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStoreOwner {
    pub workspace_id: String,
    pub session_id: SessionId,
}

/// Identity issued by the held canonical Session journal. This is persistence
/// provenance for related stores, never permission to execute external work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSessionIdentity {
    journal_id: String,
    owner: ExecutionStoreOwner,
}

impl DurableSessionIdentity {
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }

    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.owner
    }
}

/// An immutable legacy history frontier committed by the canonical v2 journal.
/// Keeping legacy turns closed does not establish safe replay of their effects.
#[derive(Debug, Clone, PartialEq)]
pub struct DurableLegacySeal {
    identity: DurableSessionIdentity,
    reference: EvidenceRef,
    last_predecessor: Option<LegacyTurnPredecessor>,
}

impl DurableLegacySeal {
    pub fn identity(&self) -> &DurableSessionIdentity {
        &self.identity
    }

    pub fn reference(&self) -> &EvidenceRef {
        &self.reference
    }

    pub fn last_predecessor(&self) -> Option<&LegacyTurnPredecessor> {
        self.last_predecessor.as_ref()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySealRecord {
    reference: EvidenceRef,
    last_predecessor: Option<LegacyTurnPredecessor>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBinding {
    turn_id: LogicalTurnId,
    reference: EvidenceRef,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    ownership_id: String,
    journal_id: String,
    owner: ExecutionStoreOwner,
    records: Vec<TurnContractEnvelope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requests: Vec<RequestBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy: Option<LegacySealRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_origin: Option<crate::native_history::NativeHistoryOrigin>,
}

/// Durable history evidence, not a permission to dispatch an activation/tool.
/// No deserializer or caller-controlled constructor can manufacture this receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableTurnReceipt {
    journal_id: String,
    sequence: u64,
    envelope: TurnContractEnvelope,
}

impl DurableTurnReceipt {
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn envelope(&self) -> &TurnContractEnvelope {
        &self.envelope
    }
    pub fn turn_revision(&self) -> u64 {
        self.envelope.expected_revision + 1
    }
}

/// A projection copied from successfully persisted canonical Session history.
/// Live snapshots can become stale; controllers must still check current
/// revisions before dispatch. A closed snapshot's accepted set is immutable.
#[derive(Debug, Clone)]
pub struct DurableTurnSnapshot {
    journal_id: String,
    owner: ExecutionStoreOwner,
    contract: TurnContract,
    turn_id: LogicalTurnId,
    request_ref: Option<EvidenceRef>,
    legacy_predecessor: Option<LegacyTurnPredecessor>,
}

impl DurableTurnSnapshot {
    pub fn turn_id(&self) -> &LogicalTurnId {
        &self.turn_id
    }
    pub fn request_ref(&self) -> Option<&EvidenceRef> {
        self.request_ref.as_ref()
    }
    pub fn legacy_predecessor(&self) -> Option<&LegacyTurnPredecessor> {
        self.legacy_predecessor.as_ref()
    }
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }
    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.owner
    }
    pub fn contract(&self) -> &TurnContract {
        &self.contract
    }
}

#[derive(Clone, Default)]
struct Projection {
    turns: HashMap<LogicalTurnId, TurnContract>,
    commands: HashMap<CommandId, usize>,
}

#[derive(Clone, Copy)]
struct Limits {
    turn_bytes: usize,
    turn_commands: usize,
    session_bytes: usize,
    session_records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turn_bytes: MAX_RETAINED_CONTRACT_BYTES,
            turn_commands: MAX_CONTRACT_COMMANDS,
            session_bytes: MAX_SESSION_BYTES,
            session_records: MAX_SESSION_RECORDS,
        }
    }
}

/// The held root guard excludes legacy format owners for this store's lifetime;
/// the separate Session directory inode lock excludes two writers sharing it.
/// No mutable projection is exposed. All changes pass replay validation and fsync.
pub struct SessionExecutionStore {
    ownership: Arc<UpgradedFormatOwnership>,
    dir: SecureDir,
    journal: Journal,
    projection: Projection,
    poisoned: bool,
    limits: Limits,
    #[cfg(test)]
    lose_next_ack: bool,
}

impl SessionExecutionStore {
    pub fn open(
        ownership: Arc<UpgradedFormatOwnership>,
        owner: ExecutionStoreOwner,
    ) -> Result<Self, ExecutionStoreError> {
        Self::open_inner(ownership, owner, false)
    }

    /// Restart and explicit reopen must not recreate a missing canonical journal.
    pub fn open_existing(
        ownership: Arc<UpgradedFormatOwnership>,
        owner: ExecutionStoreOwner,
    ) -> Result<Self, ExecutionStoreError> {
        Self::open_inner(ownership, owner, true)
    }

    fn open_inner(
        ownership: Arc<UpgradedFormatOwnership>,
        owner: ExecutionStoreOwner,
        existing_only: bool,
    ) -> Result<Self, ExecutionStoreError> {
        if owner.workspace_id.is_empty()
            || owner.workspace_id.len() > 128
            || owner.workspace_id.chars().any(char::is_control)
        {
            return Err(ExecutionStoreError::Invalid("workspace identity"));
        }
        let dir = if existing_only {
            ownership.existing_session_directory(owner.session_id.as_str())?
        } else {
            ownership.session_directory(owner.session_id.as_str())?
        };
        dir.try_lock_exclusive()?;
        let journal = match dir.read_limited(FILE, MAX_SESSION_BYTES) {
            Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if existing_only {
                    return Err(error.into());
                }
                if !dir.entries_limited(1)?.is_empty() {
                    return Err(ExecutionStoreError::Invalid(
                        "missing journal in a nonempty Session namespace",
                    ));
                }
                Journal {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    ownership_id: ownership.manifest().ownership_id.clone(),
                    journal_id: uuid::Uuid::new_v4().to_string(),
                    owner: owner.clone(),
                    records: Vec::new(),
                    requests: Vec::new(),
                    legacy: None,
                    native_origin: None,
                }
            }
            Err(error) => return Err(error.into()),
        };
        if journal.schema_version != TURN_CONTRACT_SCHEMA_VERSION
            || journal.owner != owner
            || journal.ownership_id != ownership.manifest().ownership_id
            || uuid::Uuid::parse_str(&journal.journal_id)
                .ok()
                .is_none_or(|id| id.is_nil() || id.to_string() != journal.journal_id)
            || journal.records.len() > MAX_SESSION_RECORDS
        {
            return Err(ExecutionStoreError::Invalid(
                "unsupported schema, owner, or bounds",
            ));
        }
        if let Some(origin) = &journal.native_origin {
            origin.validate(&ownership, &owner)?;
            if journal.legacy.is_some() {
                return Err(ExecutionStoreError::Invalid(
                    "native origin and legacy seal are mutually exclusive",
                ));
            }
        }
        let mut projection = Projection::default();
        for (index, envelope) in journal.records.iter().enumerate() {
            if projection.commands.contains_key(&envelope.command_id) {
                return Err(ExecutionStoreError::Invalid("duplicate canonical command"));
            }
            apply_projection(&mut projection, &owner, envelope, index)?;
            check_turn_capacity(&projection.turns[&envelope.turn_id], Limits::default())?;
        }
        validate_bindings(&journal, &projection)?;
        let mut store = Self {
            ownership,
            dir,
            journal,
            projection,
            poisoned: false,
            limits: Limits::default(),
            #[cfg(test)]
            lose_next_ack: false,
        };
        // Re-establish the durability barrier even when a previous process died
        // after rename but before syncing its parent; no receipt precedes this.
        store.persist(store.journal.clone(), store.projection.clone())?;
        store.interrupt_recovered_epoch()?;
        Ok(store)
    }

    pub fn owner(&self) -> &ExecutionStoreOwner {
        &self.journal.owner
    }

    pub fn identity(&self) -> Result<DurableSessionIdentity, ExecutionStoreError> {
        self.verify()?;
        Ok(DurableSessionIdentity {
            journal_id: self.journal.journal_id.clone(),
            owner: self.journal.owner.clone(),
        })
    }

    /// Verify the host's retained data-root capability against this store's
    /// held format owner before attaching external execution resources.
    pub fn verify_data_root(&self, root: &SecureDir) -> Result<(), ExecutionStoreError> {
        self.verify()?;
        self.ownership.verify_root(root)?;
        Ok(())
    }

    /// Capture the existing v1 source beneath this exact held data root.
    pub fn legacy_history_snapshot(
        &self,
    ) -> Result<crate::execution_legacy::OwnedLegacyHistorySnapshot, ExecutionStoreError> {
        let identity = self.identity()?;
        if self.journal.legacy.is_some()
            || self.journal.native_origin.is_some()
            || !self.journal.records.is_empty()
        {
            return Err(ExecutionStoreError::Invalid(
                "legacy capture must precede the canonical seal and v2 work",
            ));
        }
        crate::execution_legacy::OwnedLegacyHistorySnapshot::capture(
            self.ownership.clone(),
            identity,
        )
    }

    pub fn component_namespace(
        &self,
        component: ExecutionComponent,
    ) -> Result<OwnedExecutionNamespace, ExecutionStoreError> {
        Ok(OwnedExecutionNamespace::provision(
            self.dir.clone(),
            self.ownership.clone(),
            self.identity()?,
            component,
        )
        .map_err(std::io::Error::from)?)
    }
    /// Recovery opens only existing initialized component journals. This check
    /// precedes their ordinary writer opener, preserving its ownership checks.
    pub fn existing_component_namespace(
        &self,
        component: ExecutionComponent,
        primary: &std::path::Path,
    ) -> Result<OwnedExecutionNamespace, ExecutionStoreError> {
        Ok(OwnedExecutionNamespace::existing(
            self.dir.clone(),
            self.ownership.clone(),
            self.identity()?,
            component,
            primary,
        )
        .map_err(std::io::Error::from)?)
    }

    /// Inspect an existing component without provisioning or reopening any
    /// execution store. The resulting bytes are read evidence, never receipts.
    pub(crate) fn read_existing_component(
        &self,
        component: &ExecutionComponent,
        primary: &std::path::Path,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ExecutionStoreError> {
        self.verify()?;
        let bytes = crate::execution_namespace::read_existing_component(
            &self.dir,
            &self.ownership,
            &self.identity()?,
            component,
            primary,
            max_bytes,
        )?;
        self.verify()?;
        Ok(bytes)
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join(FILE)
    }

    pub fn turn(&self, id: &LogicalTurnId) -> Result<Option<&TurnContract>, ExecutionStoreError> {
        self.verify()?;
        Ok(self.projection.turns.get(id))
    }

    pub fn snapshot(&self, id: &LogicalTurnId) -> Result<DurableTurnSnapshot, ExecutionStoreError> {
        self.verify()?;
        let contract = self
            .projection
            .turns
            .get(id)
            .ok_or(ExecutionStoreError::Invalid("logical turn is absent"))?;
        Ok(DurableTurnSnapshot {
            journal_id: self.journal.journal_id.clone(),
            owner: self.journal.owner.clone(),
            contract: contract.clone(),
            turn_id: id.clone(),
            request_ref: self
                .journal
                .requests
                .iter()
                .find(|binding| binding.turn_id == *id)
                .map(|binding| binding.reference.clone()),
            legacy_predecessor: (self
                .journal
                .records
                .first()
                .is_some_and(|record| record.turn_id == *id))
            .then(|| {
                self.journal
                    .legacy
                    .as_ref()
                    .and_then(|seal| seal.last_predecessor.clone())
            })
            .flatten(),
        })
    }

    pub fn unfinished_turn(
        &self,
    ) -> Result<Option<(&LogicalTurnId, &TurnContract)>, ExecutionStoreError> {
        self.verify()?;
        Ok(self
            .projection
            .turns
            .iter()
            .find(|(_, turn)| turn.state().is_some_and(|state| !state.is_closed())))
    }

    pub fn records(&self) -> Result<&[TurnContractEnvelope], ExecutionStoreError> {
        self.verify()?;
        Ok(&self.journal.records)
    }

    /// Commit a retained request in the same atomic write as its Begin. A
    /// published Begin can never acquire different request bytes on a retry.
    pub fn begin_with_request(
        &mut self,
        envelope: TurnContractEnvelope,
        request: &DurableExecutionRequest,
    ) -> Result<DurableTurnReceipt, ExecutionStoreError> {
        self.verify()?;
        if request.journal_id() != self.journal.journal_id
            || request.owner() != self.owner()
            || request.turn_id() != &envelope.turn_id
            || !matches!(envelope.event, TurnContractEvent::Begin { .. })
        {
            return Err(ExecutionStoreError::Invalid(
                "request ownership or Begin mismatch",
            ));
        }
        let binding = RequestBinding {
            turn_id: envelope.turn_id.clone(),
            reference: request.reference().clone(),
        };
        if let Some(index) = self.projection.commands.get(&envelope.command_id) {
            if self.journal.records[*index] != envelope || !self.journal.requests.contains(&binding)
            {
                return Err(TurnContractError::CommandConflict.into());
            }
            return Ok(self.receipt(*index));
        }
        let index = self.journal.records.len();
        let mut projection = self.projection.clone();
        apply_projection(&mut projection, self.owner(), &envelope, index)?;
        let mut journal = self.journal.clone();
        journal.records.push(envelope);
        journal.requests.push(binding);
        self.persist(journal, projection)?;
        Ok(self.receipt(index))
    }

    /// Seal supported legacy history before the first v2 turn. The source
    /// receipt comes from retained immutable content, not a caller-made digest.
    pub fn seal_legacy_history(
        &mut self,
        history: &DurableLegacyHistory,
    ) -> Result<DurableLegacySeal, ExecutionStoreError> {
        self.verify()?;
        if history.journal_id() != self.journal.journal_id || history.owner() != self.owner() {
            return Err(ExecutionStoreError::Invalid("foreign legacy history"));
        }
        if self.journal.native_origin.is_some() {
            return Err(ExecutionStoreError::Invalid(
                "native Session cannot acquire a legacy seal",
            ));
        }
        let seal = LegacySealRecord {
            reference: history.reference().clone(),
            last_predecessor: history.last_predecessor().cloned(),
        };
        if let Some(existing) = &self.journal.legacy {
            if existing != &seal {
                return Err(ExecutionStoreError::Invalid("legacy frontier is immutable"));
            }
        } else {
            if !self.journal.records.is_empty() {
                return Err(ExecutionStoreError::Invalid(
                    "legacy frontier must precede v2 work",
                ));
            }
            crate::execution_legacy::verify_source(&self.ownership, history.source())?;
            let mut journal = self.journal.clone();
            journal.legacy = Some(seal);
            self.persist(journal, self.projection.clone())?;
        }
        self.legacy_seal()?
            .ok_or(ExecutionStoreError::Invalid("missing legacy seal"))
    }

    /// Record only an actual creation receipt before any native work. This
    /// does not infer origin from an empty journal or absent legacy source.
    pub fn record_native_origin(
        &mut self,
        receipt: &crate::native_history::NativeSessionCreationReceipt,
    ) -> Result<(), ExecutionStoreError> {
        self.verify()?;
        receipt.origin.validate(&self.ownership, self.owner())?;
        if self.journal.legacy.is_some() {
            return Err(ExecutionStoreError::Invalid(
                "sealed legacy Session cannot become native",
            ));
        }
        if let Some(origin) = &self.journal.native_origin {
            return if origin == &receipt.origin {
                Ok(())
            } else {
                Err(ExecutionStoreError::Invalid(
                    "native Session origin is immutable",
                ))
            };
        }
        receipt.verify(&self.ownership, self.owner())?;
        if !self.journal.records.is_empty() {
            return Err(ExecutionStoreError::Invalid(
                "native origin must precede the first Begin",
            ));
        }
        let mut journal = self.journal.clone();
        journal.native_origin = Some(receipt.origin.clone());
        self.persist(journal, self.projection.clone())
    }

    pub fn native_origin(
        &self,
    ) -> Result<Option<&crate::native_history::NativeHistoryOrigin>, ExecutionStoreError> {
        self.verify()?;
        Ok(self.journal.native_origin.as_ref())
    }

    pub fn legacy_seal(&self) -> Result<Option<DurableLegacySeal>, ExecutionStoreError> {
        let identity = self.identity()?;
        Ok(self.journal.legacy.as_ref().map(|seal| DurableLegacySeal {
            identity,
            reference: seal.reference.clone(),
            last_predecessor: seal.last_predecessor.clone(),
        }))
    }

    /// Exact repeats return the original durable receipt before checking stale
    /// revisions or current closure. A reused command ID with changed content
    /// fails even when it names a different logical turn in this Session.
    pub fn append(
        &mut self,
        envelope: TurnContractEnvelope,
    ) -> Result<DurableTurnReceipt, ExecutionStoreError> {
        self.verify()?;
        if let Some(index) = self.projection.commands.get(&envelope.command_id) {
            if self.journal.records[*index] != envelope {
                return Err(TurnContractError::CommandConflict.into());
            }
            return Ok(self.receipt(*index));
        }
        let index = self.journal.records.len();
        let mut projection = self.projection.clone();
        apply_projection(&mut projection, &self.journal.owner, &envelope, index)?;
        let mut journal = self.journal.clone();
        journal.records.push(envelope);
        self.persist(journal, projection)?;
        Ok(self.receipt(index))
    }

    fn receipt(&self, index: usize) -> DurableTurnReceipt {
        DurableTurnReceipt {
            journal_id: self.journal.journal_id.clone(),
            sequence: index as u64 + 1,
            envelope: self.journal.records[index].clone(),
        }
    }

    fn interrupt_recovered_epoch(&mut self) -> Result<(), ExecutionStoreError> {
        let recovered = self.projection.turns.iter().find_map(|(id, turn)| {
            (turn.state() == Some(LogicalTurnState::Running)).then(|| {
                (
                    id.clone(),
                    turn.revision(),
                    turn.epochs()
                        .last()
                        .expect("validated live epoch")
                        .id
                        .clone(),
                )
            })
        });
        if let Some((turn_id, expected_revision, epoch_id)) = recovered {
            self.append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!("recovery:{}", uuid::Uuid::new_v4()))?,
                expected_revision,
                session_id: self.journal.owner.session_id.clone(),
                turn_id,
                event: TurnContractEvent::InterruptEpoch { epoch_id },
            })?;
        }
        Ok(())
    }

    fn verify(&self) -> Result<(), ExecutionStoreError> {
        if self.poisoned {
            return Err(ExecutionStoreError::RecoveryRequired);
        }
        self.ownership.verify_installed()?;
        self.dir.verify_ambient_identity()?;
        Ok(())
    }

    fn persist(
        &mut self,
        journal: Journal,
        projection: Projection,
    ) -> Result<(), ExecutionStoreError> {
        self.verify()?;
        validate_bindings(&journal, &projection)?;
        let mut bytes_reserved = 0usize;
        let mut records_reserved = 0usize;
        for turn in projection.turns.values() {
            let (bytes, records) = check_turn_capacity(turn, self.limits)?;
            bytes_reserved += bytes;
            records_reserved += records;
        }
        let bytes = serde_json::to_vec(&journal)?;
        if bytes.len().saturating_add(bytes_reserved) > self.limits.session_bytes
            || journal.records.len().saturating_add(records_reserved) > self.limits.session_records
        {
            return Err(ExecutionStoreError::Capacity);
        }
        if let Err(error) = self.dir.atomic_write(FILE, &bytes) {
            self.poisoned = true;
            return Err(error.into());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.lose_next_ack) {
            self.poisoned = true;
            return Err(std::io::Error::other(
                "injected acknowledgement loss after durable publication",
            )
            .into());
        }
        self.journal = journal;
        self.projection = projection;
        Ok(())
    }
}

fn validate_bindings(
    journal: &Journal,
    projection: &Projection,
) -> Result<(), ExecutionStoreError> {
    let mut turns = std::collections::HashSet::new();
    for binding in &journal.requests {
        if !projection.turns.contains_key(&binding.turn_id) || !turns.insert(&binding.turn_id) {
            return Err(ExecutionStoreError::Invalid(
                "duplicate or absent request owner",
            ));
        }
    }
    if journal
        .legacy
        .as_ref()
        .and_then(|seal| seal.last_predecessor.as_ref())
        .is_some_and(|predecessor| !predecessor.status.is_terminal())
    {
        return Err(ExecutionStoreError::Invalid(
            "legacy predecessor is unfinished",
        ));
    }
    Ok(())
}

fn apply_projection(
    projection: &mut Projection,
    owner: &ExecutionStoreOwner,
    envelope: &TurnContractEnvelope,
    index: usize,
) -> Result<(), ExecutionStoreError> {
    if envelope.session_id != owner.session_id {
        return Err(ExecutionStoreError::Invalid("foreign Session event"));
    }
    if let TurnContractEvent::Begin { predecessor, .. } = &envelope.event {
        if projection
            .turns
            .values()
            .any(|turn| turn.state().is_some_and(|state| !state.is_closed()))
        {
            return Err(ExecutionStoreError::UnfinishedTurn);
        }
        if !projection.turns.contains_key(&envelope.turn_id)
            && projection.turns.len() >= MAX_SESSION_TURNS
        {
            return Err(ExecutionStoreError::Capacity);
        }
        if let Some(previous) = predecessor {
            let canonical = projection
                .turns
                .get(previous.turn_id())
                .ok_or(ExecutionStoreError::Invalid(
                    "predecessor absent from canonical Session history",
                ))?
                .closed_reference()?;
            if &canonical != previous {
                return Err(ExecutionStoreError::Invalid(
                    "predecessor differs from canonical closed history",
                ));
            }
        }
    }
    let size = serde_json::to_vec(envelope)?.len();
    if small_settlement(&envelope.event) && size > SMALL_SETTLEMENT_BYTES {
        return Err(ExecutionStoreError::Invalid(
            "settlement must use bounded evidence references",
        ));
    }
    let turn = projection
        .turns
        .entry(envelope.turn_id.clone())
        .or_default();
    if !turn.apply(envelope)? {
        return Err(ExecutionStoreError::Invalid("duplicate canonical event"));
    }
    projection
        .commands
        .insert(envelope.command_id.clone(), index);
    Ok(())
}

fn small_settlement(event: &TurnContractEvent) -> bool {
    matches!(
        event,
        TurnContractEvent::RequestTurnStop { .. }
            | TurnContractEvent::StartPreparedActivation { .. }
            | TurnContractEvent::AcceptActivation { .. }
            | TurnContractEvent::FailActivation { .. }
            | TurnContractEvent::RecordOutcome { .. }
            | TurnContractEvent::ProveNotDispatched { .. }
            | TurnContractEvent::ResolveConditionIntent { .. }
            | TurnContractEvent::ResolveBlocker { .. }
            | TurnContractEvent::AbandonBlocker { .. }
            | TurnContractEvent::InterruptEpoch { .. }
            | TurnContractEvent::PauseEpoch { .. }
            | TurnContractEvent::Close { .. }
    )
}

fn check_turn_capacity(
    turn: &TurnContract,
    limits: Limits,
) -> Result<(usize, usize), ExecutionStoreError> {
    let (bytes, records) = settlement_reservation(turn);
    if turn.retained_event_bytes().saturating_add(bytes) > limits.turn_bytes
        || turn.command_count().saturating_add(records) > limits.turn_commands
    {
        return Err(ExecutionStoreError::Capacity);
    }
    Ok((bytes, records))
}

fn settlement_reservation(turn: &TurnContract) -> (usize, usize) {
    let mut small = match turn.state() {
        Some(LogicalTurnState::Running) => 2, // interrupt, then explicit closure
        Some(LogicalTurnState::NeedsAttention) => 1,
        _ => return (0, 0), // late external evidence belongs to the separate audit
    };
    // Reserve the exact human Stop request in addition to interruption/closure.
    // Recording the intent consumes this reservation; no limit is increased.
    if turn.stop_requested().is_none() {
        small += 1;
    }
    for activation in turn.activations() {
        // Replacement retires never-started work, retaining its row as history.
        // There can be no later start/terminal event for that removed node.
        if turn
            .replaced_nodes()
            .iter()
            .any(|node| node.previous == activation.activation.node_id)
        {
            continue;
        }
        small += match activation.state {
            ActivationState::Unstarted => 2, // begin prepared, then accept/fail
            ActivationState::Running => 1,
            _ => 0,
        };
    }
    small += turn
        .invocations()
        .iter()
        .filter(|invocation| invocation.evidence.disposition() == EffectDisposition::OutcomeUnknown)
        .count();
    // A typed durable blocker reserves its exact response independently from
    // activation/effect settlement. An epoch interruption retires the wait.
    small += turn.pending_blocker_count();
    // Resolving an undispatched check leaves its readiness observation absent.
    // Reserve this terminal effect record separately from the observation below.
    small += turn
        .condition_runs()
        .iter()
        .filter(|run| run.resolution.is_none())
        .count();
    // A passed or failed exact-scope observation settles a check. Invalidation
    // makes historical observations stale, reserving space before revised work.
    let conditions = turn.graph().map_or(0, |graph| {
        graph
            .conditions
            .iter()
            .filter(|condition| turn.current_condition(&condition.condition_id).is_none())
            .count()
    });
    // Include the per-record JSON separator in the Session-wide reservation.
    (
        small * (SMALL_SETTLEMENT_BYTES + 1) + conditions * (MAX_CONTRACT_ENVELOPE_BYTES + 1),
        small + conditions,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::execution_ownership::LegacyFormatOwnership;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, SessionExecutionStore, serde_json::Value) {
        let root = tempfile::tempdir().unwrap();
        let guard = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let owner = ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session-a").unwrap(),
        };
        let mut store = SessionExecutionStore::open(guard, owner).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/interrupted_epoch_preserves_acceptance.json"
        ))
        .unwrap();
        for index in 0..2 {
            store
                .append(
                    serde_json::from_value(fixture["steps"][index]["envelope"].clone()).unwrap(),
                )
                .unwrap();
        }
        let activation = fixture["steps"][1]["envelope"]["event"]["input"]["activation"].clone();
        (root, store, activation)
    }

    fn envelope(revision: u64, event: serde_json::Value) -> TurnContractEnvelope {
        serde_json::from_value(json!({
            "schema_version": 2, "command_id": format!("capacity-{revision}"),
            "expected_revision": revision, "session_id": "session-a", "turn_id": "turn-a", "event": event,
        })).unwrap()
    }

    #[test]
    fn replacement_releases_only_impossible_prepared_activation_settlement_slots() {
        let fixture: Vec<TurnContractEnvelope> = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/dynamic_graph_blocker_contract.json"
        ))
        .unwrap();
        let mut turn = TurnContract::default();
        turn.apply(&fixture[0]).unwrap();
        turn.apply(&envelope(
            1,
            json!({"kind":"pause_epoch","epoch_id":"epoch-1"}),
        ))
        .unwrap();
        let mut input = serde_json::to_value(&fixture[2]).unwrap()["event"]["input"].clone();
        input["activation"]["execution_epoch_id"] = "epoch-2".into();
        let previous_activation = input["activation"].clone();
        turn.apply(&envelope(
            2,
            json!({"kind":"continue","plan":{
            "source_epoch_id":"epoch-1","epoch_id":"epoch-2","condition_runs":[],
            "selections":[{"kind":"prepare_unmaterialized","input":input},
                {"kind":"await_dependencies","node_id":"b"}]}}),
        ))
        .unwrap();
        let before = settlement_reservation(&turn);
        let mut graph = serde_json::to_value(turn.graph().unwrap()).unwrap();
        graph["snapshot_id"] = "graph-replaced".into();
        graph["revision"] = 2.into();
        graph["nodes"][0]["node_id"] = "x".into();
        graph["nodes"][0]["slot_id"] = "slot-x".into();
        graph["nodes"][0]["conversation_id"] = "conversation-x".into();
        graph["dependencies"][0]["parent"] = "x".into();
        turn.apply(&envelope(3,json!({"kind":"revise_graph","epoch_id":"epoch-2", "previous_graph":"graph-1",
            "graph":graph,"mutation":{"kind":"replace_future","previous":"a","replacement":"x","rewire_dependents":["b"]},
            "admission_evidence":"replace-prepared-a"}))).unwrap();
        let after = settlement_reservation(&turn);
        assert_eq!(before.1 - after.1, 2);
        assert_eq!(before.0 - after.0, 2 * (SMALL_SETTLEMENT_BYTES + 1));
        assert_eq!(
            turn.activations()[0].state,
            ActivationState::Unstarted,
            "historical input remains retained"
        );
        let frozen = turn.clone();
        assert!(turn
            .apply(&envelope(
                4,
                json!({"kind":"start_prepared_activation","activation":previous_activation})
            ))
            .is_err());
        assert_eq!(turn, frozen);
    }

    #[test]
    fn typed_blocker_admission_reserves_response_before_more_waits() {
        for byte_limit in [false, true] {
            let (_root, mut store, activation) = setup();
            let grant = serde_json::to_value(
                &store.unfinished_turn().unwrap().unwrap().1.activations()[0]
                    .input
                    .grant,
            )
            .unwrap();
            let open = json!({"kind":"open_blocker","blocker":{
                "schema_version":1,"blocker_id":"wait-one","activation":activation,
                "kind":{"kind":"human_approval","approval_request":"request-one"},
                "command_id":"requested-command","invocation_id":null,"grant":grant,
                "parameters":"exact-parameters","safe_boundary":"safe-boundary-one","evidence":"wait-evidence"}});
            store.append(envelope(2, open.clone())).unwrap();
            let (reserved_bytes, reserved_commands) =
                settlement_reservation(store.unfinished_turn().unwrap().unwrap().1);
            if byte_limit {
                store.limits.turn_bytes = store
                    .unfinished_turn()
                    .unwrap()
                    .unwrap()
                    .1
                    .retained_event_bytes()
                    + reserved_bytes;
            } else {
                store.limits.turn_commands = 3 + reserved_commands;
            }
            let before = std::fs::read(store.path()).unwrap();
            let mut another = open;
            another["blocker"]["blocker_id"] = "wait-two".into();
            assert!(matches!(
                store.append(envelope(3, another)),
                Err(ExecutionStoreError::Capacity)
            ));
            assert_eq!(std::fs::read(store.path()).unwrap(), before);
            store.append(envelope(3,json!({"kind":"resolve_blocker","blocker_id":"wait-one","activation":activation,
                "response":{"kind":"human_approval","approval_request":"request-one","approval_evidence":"verified-human"}}))).unwrap();
            assert_eq!(
                store
                    .unfinished_turn()
                    .unwrap()
                    .unwrap()
                    .1
                    .pending_blocker_count(),
                0
            );
            for (revision, event) in [
                (
                    4,
                    json!({"kind":"fail_activation","activation":activation,"evidence":"failed-after-response"}),
                ),
                (5, json!({"kind":"interrupt_epoch","epoch_id":"epoch-1"})),
                (6, json!({"kind":"close","closure":"finished"})),
            ] {
                store.append(envelope(revision, event)).unwrap();
            }
            assert!(store.unfinished_turn().unwrap().is_none());
        }
    }

    #[test]
    fn admission_reserves_command_and_byte_capacity_for_outcome_failure_interruption_and_closure() {
        for bytes_limit in [false, true] {
            let (_root, mut store, activation) = setup();
            store.append(envelope(2, json!({"kind":"record_intent", "invocation_id":"tool-1", "activation":activation}))).unwrap();
            if bytes_limit {
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                store.limits.turn_bytes =
                    turn.retained_event_bytes() + settlement_reservation(turn).0;
            } else {
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                store.limits.turn_commands = turn.command_count() + settlement_reservation(turn).1;
            }
            let before = std::fs::read(store.path()).unwrap();
            assert!(matches!(store.append(envelope(3, json!({"kind":"record_intent", "invocation_id":"tool-2", "activation":activation}))), Err(ExecutionStoreError::Capacity)));
            assert_eq!(std::fs::read(store.path()).unwrap(), before);
            for (revision, event) in [
                (
                    3,
                    json!({"kind":"record_outcome", "invocation_id":"tool-1", "outcome":"failed", "evidence":"executor-result"}),
                ),
                (
                    4,
                    json!({"kind":"fail_activation", "activation":activation, "evidence":"check-failed"}),
                ),
                (5, json!({"kind":"interrupt_epoch", "epoch_id":"epoch-1"})),
                (6, json!({"kind":"close", "closure":"finished"})),
            ] {
                store.append(envelope(revision, event)).unwrap();
            }
            assert!(store.unfinished_turn().unwrap().is_none());
            assert_eq!(store.records().unwrap().len(), 7);
        }
    }

    #[test]
    fn session_storage_admission_preserves_the_same_settlement_reserve() {
        for byte_limit in [false, true] {
            let (_root, mut store, activation) = setup();
            let (reserved_bytes, reserved_records) =
                settlement_reservation(store.unfinished_turn().unwrap().unwrap().1);
            if byte_limit {
                store.limits.session_bytes =
                    serde_json::to_vec(&store.journal).unwrap().len() + reserved_bytes;
            } else {
                store.limits.session_records = store.journal.records.len() + reserved_records;
            }
            assert!(matches!(store.append(envelope(2, json!({"kind":"record_intent", "invocation_id":"tool-extra", "activation":activation}))), Err(ExecutionStoreError::Capacity)));
            store.append(envelope(2, json!({"kind":"fail_activation", "activation":activation, "evidence":"failure"}))).unwrap();
            store
                .append(envelope(
                    3,
                    json!({"kind":"interrupt_epoch", "epoch_id":"epoch-1"}),
                ))
                .unwrap();
            store
                .append(envelope(4, json!({"kind":"close", "closure":"finished"})))
                .unwrap();
        }
    }

    #[test]
    fn lost_acknowledgement_retains_unknown_intent_and_recovers_original_receipt() {
        let (_root, mut store, activation) = setup();
        let guard = store.ownership.clone();
        let owner = store.owner().clone();
        let request = envelope(
            2,
            json!({"kind":"record_intent", "invocation_id":"tool-1", "activation":activation}),
        );
        let expected = DurableTurnReceipt {
            journal_id: store.journal.journal_id.clone(),
            sequence: 3,
            envelope: request.clone(),
        };
        store.lose_next_ack = true;
        assert!(store.append(request.clone()).is_err());
        assert!(matches!(
            store.records(),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        assert!(matches!(
            store.append(request.clone()),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(store);
        let mut recovered = SessionExecutionStore::open(guard, owner).unwrap();
        assert_eq!(recovered.append(request).unwrap(), expected);
        let (_, turn) = recovered.unfinished_turn().unwrap().unwrap();
        assert_eq!(turn.state(), Some(LogicalTurnState::NeedsAttention));
        assert_eq!(
            turn.invocations()[0].evidence.disposition(),
            EffectDisposition::OutcomeUnknown
        );
        assert_eq!(turn.revision(), 4);
        assert_eq!(recovered.records().unwrap().len(), 4);
    }

    fn condition_setup() -> (tempfile::TempDir, SessionExecutionStore, serde_json::Value) {
        let root = tempfile::tempdir().unwrap();
        let guard = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let mut store = SessionExecutionStore::open(
            guard,
            ExecutionStoreOwner {
                workspace_id: "workspace".into(),
                session_id: SessionId::new("session-a").unwrap(),
            },
        )
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
        )).unwrap();
        let mut begin = fixture["steps"][0]["envelope"].clone();
        let mut other = begin["event"]["graph"]["conditions"][0].clone();
        other["condition_id"] = json!("second-review");
        begin["event"]["graph"]["conditions"]
            .as_array_mut()
            .unwrap()
            .push(other);
        store
            .append(serde_json::from_value(begin).unwrap())
            .unwrap();
        for step in fixture["steps"].as_array().unwrap().iter().skip(1).take(2) {
            store
                .append(serde_json::from_value(step["envelope"].clone()).unwrap())
                .unwrap();
        }
        let run = json!({
            "session_id":"session-a", "turn_id":"turn-a", "epoch_id":"epoch-1",
            "condition_id":"review", "run_id":"check-run-1",
            "activations":[fixture["steps"][1]["envelope"]["event"]["input"]["activation"].clone()]
        });
        (root, store, run)
    }

    #[test]
    fn condition_intent_reserves_resolution_separately_from_unrun_observation() {
        for capacity in [
            "turn-records",
            "turn-bytes",
            "session-records",
            "session-bytes",
        ] {
            for resolution in ["outcome_recorded", "not_dispatched"] {
                let (_root, mut store, run) = condition_setup();
                store.append(envelope(3, json!({
                    "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
                }))).unwrap();
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                let (reserved_bytes, reserved_records) = settlement_reservation(turn);
                assert_eq!(reserved_records, 6); // resolution, two observations, Stop, interruption, closure
                match capacity {
                    "turn-records" => {
                        store.limits.turn_commands = turn.command_count() + reserved_records
                    }
                    "turn-bytes" => {
                        store.limits.turn_bytes = turn.retained_event_bytes() + reserved_bytes
                    }
                    "session-records" => {
                        store.limits.session_records =
                            store.journal.records.len() + reserved_records
                    }
                    "session-bytes" => {
                        store.limits.session_bytes =
                            serde_json::to_vec(&store.journal).unwrap().len() + reserved_bytes
                    }
                    _ => unreachable!(),
                }
                let mut competing = run.clone();
                competing["condition_id"] = json!("second-review");
                competing["run_id"] = json!("check-run-2");
                let before = std::fs::read(store.path()).unwrap();
                assert!(matches!(store.append(envelope(4, json!({
                    "kind":"record_condition_intent", "run":competing, "intent":"other-arguments"
                }))), Err(ExecutionStoreError::Capacity)));
                assert_eq!(std::fs::read(store.path()).unwrap(), before);
                store
                    .append(envelope(
                        4,
                        json!({
                            "kind":"resolve_condition_intent", "run_id":"check-run-1",
                            "resolution":{"kind":resolution, "evidence":"actual-executor-evidence"}
                        }),
                    ))
                    .unwrap();
                let turn = store.unfinished_turn().unwrap().unwrap().1;
                assert_eq!(settlement_reservation(turn).1, 5);
                assert!(turn.conditions().is_empty());
                assert!(!turn.has_unknown_effects());
                let mut revision = 5;
                if resolution == "outcome_recorded" {
                    store.append(envelope(revision, json!({
                        "kind":"record_condition", "epoch_id":"epoch-1", "condition_id":"review",
                        "activations":run["activations"], "outcome":"failed", "evidence":"actual-failed-verdict"
                    }))).unwrap();
                    revision += 1;
                }
                store
                    .append(envelope(
                        revision,
                        json!({"kind":"pause_epoch", "epoch_id":"epoch-1"}),
                    ))
                    .unwrap();
                store
                    .append(envelope(
                        revision + 1,
                        json!({"kind":"close", "closure":"finished"}),
                    ))
                    .unwrap();
                assert!(store.unfinished_turn().unwrap().is_none());
            }
        }
    }

    #[test]
    fn lost_condition_intent_acknowledgement_recovers_unknown_without_replay() {
        let (_root, mut store, run) = condition_setup();
        let guard = store.ownership.clone();
        let owner = store.owner().clone();
        let intent = envelope(
            3,
            json!({
                "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
            }),
        );
        store.lose_next_ack = true;
        assert!(matches!(
            store.append(intent.clone()),
            Err(ExecutionStoreError::Io(_))
        ));
        assert!(matches!(
            store.records(),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(store);
        let mut recovered = SessionExecutionStore::open(guard.clone(), owner.clone()).unwrap();
        let receipt = recovered.append(intent.clone()).unwrap();
        let (_, turn) = recovered.unfinished_turn().unwrap().unwrap();
        assert_eq!(turn.state(), Some(LogicalTurnState::NeedsAttention));
        assert!(turn.has_unknown_effects());
        assert_eq!(turn.condition_runs().len(), 1);
        assert_eq!(
            turn.condition_runs()[0].run.activations[0],
            turn.activations()[0].activation
        );
        assert!(turn.conditions().is_empty());
        assert_eq!(recovered.records().unwrap().len(), 5);
        drop(recovered);
        let mut again = SessionExecutionStore::open(guard, owner).unwrap();
        assert_eq!(again.records().unwrap().len(), 5);
        assert_eq!(again.append(intent).unwrap(), receipt);
        again
            .append(envelope(
                5,
                json!({
                    "kind":"resolve_condition_intent", "run_id":"check-run-1",
                    "resolution":{"kind":"outcome_recorded", "evidence":"late-actual-result"}
                }),
            ))
            .unwrap();
        let turn = again.unfinished_turn().unwrap().unwrap().1;
        assert!(!turn.has_unknown_effects());
        assert!(turn.conditions().is_empty());
    }

    #[test]
    fn lost_begin_acknowledgement_recovers_the_same_request_binding() {
        use crate::execution_content::{ExecutionContentStore, ExecutionRequestContent};

        let root = tempfile::tempdir().unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let owner = ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session-a").unwrap(),
        };
        let mut store = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
        let mut content = ExecutionContentStore::open_owned(
            store
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/turn_contract/interrupted_epoch_preserves_acceptance.json"
        ))
        .unwrap();
        let envelope: TurnContractEnvelope =
            serde_json::from_value(fixture["steps"][0]["envelope"].clone()).unwrap();
        let request = content
            .retain_request(ExecutionRequestContent {
                turn_id: envelope.turn_id.clone(),
                recorded_at_unix_ms: 42,
                display_input: "Original user request".into(),
                effective_input: "Original user request and supplied context".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        store.lose_next_ack = true;
        assert!(store
            .begin_with_request(envelope.clone(), &request)
            .is_err());
        assert!(matches!(
            store.snapshot(&envelope.turn_id),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(content);
        drop(store);
        let mut recovered = SessionExecutionStore::open(ownership, owner).unwrap();
        let snapshot = recovered.snapshot(&envelope.turn_id).unwrap();
        assert_eq!(snapshot.request_ref(), Some(request.reference()));
        assert_eq!(
            snapshot.contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        let receipt = recovered.begin_with_request(envelope, &request).unwrap();
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(recovered.records().unwrap().len(), 2);
    }

    #[test]
    fn lost_legacy_seal_acknowledgement_recovers_the_same_frontier() {
        use crate::execution_content::ExecutionContentStore;
        use crate::SessionTurnStore;

        let root = tempfile::tempdir().unwrap();
        SessionTurnStore::open(root.path().join("session-history")).unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let owner = ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session-a").unwrap(),
        };
        let mut store = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
        let mut content = ExecutionContentStore::open_owned(
            store
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let history = content
            .retain_legacy_history(&store.legacy_history_snapshot().unwrap())
            .unwrap();
        store.lose_next_ack = true;
        assert!(store.seal_legacy_history(&history).is_err());
        assert!(matches!(
            store.legacy_seal(),
            Err(ExecutionStoreError::RecoveryRequired)
        ));
        drop(content);
        drop(store);
        let mut recovered = SessionExecutionStore::open(ownership, owner).unwrap();
        let seal = recovered.seal_legacy_history(&history).unwrap();
        assert_eq!(seal.reference(), history.reference());
        assert!(seal.last_predecessor().is_none());
        assert!(recovered.records().unwrap().is_empty());
    }
}

#[cfg(all(test, unix))]
#[path = "execution_store_turn_stop_tests.rs"]
mod turn_stop_tests;
