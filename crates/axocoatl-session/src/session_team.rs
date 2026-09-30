//! Versioned future Session configuration under its actual canonical owner.
//!
//! Applying a team revision never rewrites a turn graph, promotes a checkpoint,
//! grants execution authority, or mutates a reusable Agent definition. The host
//! selects one immutable revision at Begin, resolves its exact retained inputs,
//! and supplies verified committed savepoints. Layout is presentation only.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::execution_content::{
    ActivationEvidenceContent, ExecutionContentError, ExecutionContentStore,
};
use crate::execution_namespace::{ExecutionComponent, OwnedExecutionNamespace};
use crate::execution_store::{
    DurableSessionIdentity, ExecutionStoreError, ExecutionStoreOwner, SessionExecutionStore,
};
use crate::turn_contract::{
    CommandId, CompletionCondition, ConditionKind, ConversationSavepoint, DefinitionSnapshotRef,
    DependencyEdge, EvidenceRef, GraphNode, GraphSnapshotId, LogicalTurnId, NodeConversationId,
    SessionTeamSlotId, TurnContractError, TurnContractEvent, TurnGraphSnapshot, TurnNodeId,
    MAX_CONTRACT_COMMANDS, MAX_CONTRACT_ENVELOPE_BYTES, MAX_CONTRACT_NODES,
    MAX_RETAINED_CONTRACT_BYTES,
};

const FILE: &str = "session-team.v1.json";
pub const SESSION_TEAM_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamSlot {
    pub slot_id: SessionTeamSlotId,
    pub node_id: TurnNodeId,
    pub definition: DefinitionSnapshotRef,
    pub conversation_id: NodeConversationId,
    pub required: bool,
    /// Exact retained Budget body, not a current turn's authority grant.
    pub budget: EvidenceRef,
    /// Explicit human-approved exact policy. Absence preserves older team
    /// records and provides no authority to execute a future turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<EvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamGraph {
    pub slots: Vec<SessionTeamSlot>,
    pub dependencies: Vec<DependencyEdge>,
    pub conditions: Vec<CompletionCondition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamPosition {
    pub slot_id: SessionTeamSlotId,
    pub x: f64,
    pub y: f64,
}

/// Every proposed slot has an explicit continuity decision in the Apply diff.
/// No reference alone establishes provider-native transcript compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionTeamContinuity {
    /// Same current slot, exact definition snapshot, and conversation identity.
    PreserveUnchanged,
    /// New/replaced/reset work starts a conversation never used by this team.
    Reset,
    /// Requires the host's real accepted-history projection proof. This store
    /// provides no default validator that turns a JSON assertion into proof.
    PreserveWithProjection { evidence: EvidenceRef },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotContinuityDecision {
    pub slot_id: SessionTeamSlotId,
    pub decision: SessionTeamContinuity,
}

/// Explicit initial import from an actually retained graph. Selected slots in
/// the Apply payload are the membership; no dynamic node is silently imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamSource {
    pub turn_id: LogicalTurnId,
    pub snapshot_id: GraphSnapshotId,
    pub graph_revision: u64,
}

#[derive(Debug, Clone)]
pub struct SessionTeamConversationSource {
    pub slot_id: SessionTeamSlotId,
    pub definition: DefinitionSnapshotRef,
    pub conversation_id: NodeConversationId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamCommit {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub expected_configuration_revision: u64,
    pub graph: SessionTeamGraph,
    pub initial_source: Option<SessionTeamSource>,
    pub continuity: Vec<SlotContinuityDecision>,
    /// Presentation coordinates do not become dependencies or execution state.
    pub layout: Vec<SessionTeamPosition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTeamRevision {
    pub schema_version: u32,
    pub configuration_revision: u64,
    /// Canonical prefix observed at Apply; later turns may legitimately use the
    /// new conversations and must not invalidate their original fresh allocation.
    pub canonical_record_count: u64,
    pub command_id: CommandId,
    pub expected_configuration_revision: u64,
    pub graph: SessionTeamGraph,
    pub initial_source: Option<SessionTeamSource>,
    pub continuity: Vec<SlotContinuityDecision>,
    pub layout: Vec<SessionTeamPosition>,
}
impl SessionTeamRevision {
    fn from_commit(revision: u64, canonical_record_count: u64, request: SessionTeamCommit) -> Self {
        Self {
            schema_version: request.schema_version,
            configuration_revision: revision,
            canonical_record_count,
            command_id: request.command_id,
            expected_configuration_revision: request.expected_configuration_revision,
            graph: request.graph,
            initial_source: request.initial_source,
            continuity: request.continuity,
            layout: request.layout,
        }
    }
    fn request(&self) -> SessionTeamCommit {
        SessionTeamCommit {
            schema_version: self.schema_version,
            command_id: self.command_id.clone(),
            expected_configuration_revision: self.expected_configuration_revision,
            graph: self.graph.clone(),
            initial_source: self.initial_source.clone(),
            continuity: self.continuity.clone(),
            layout: self.layout.clone(),
        }
    }

    /// Structural construction only. The host must verify actual committed
    /// checkpoint bytes and select this immutable revision under Begin's lock.
    /// Every slot requires an explicit savepoint, including explicit Empty.
    pub fn initial_graph(
        &self,
        owner: &ExecutionStoreOwner,
        snapshot_id: GraphSnapshotId,
        savepoints: &[(SessionTeamSlotId, ConversationSavepoint)],
    ) -> Result<TurnGraphSnapshot, SessionTeamError> {
        let points: HashMap<_, _> = savepoints
            .iter()
            .map(|(slot, point)| (slot, point))
            .collect();
        if points.len() != savepoints.len()
            || points.len() != self.graph.slots.len()
            || self
                .graph
                .slots
                .iter()
                .any(|slot| !points.contains_key(&slot.slot_id))
        {
            return Err(SessionTeamError::Invalid(
                "initial graph requires exactly one explicit savepoint per slot",
            ));
        }
        let graph = TurnGraphSnapshot {
            snapshot_id,
            revision: 1,
            nodes: self
                .graph
                .slots
                .iter()
                .map(|slot| GraphNode {
                    node_id: slot.node_id.clone(),
                    slot_id: slot.slot_id.clone(),
                    definition: slot.definition.clone(),
                    conversation_id: slot.conversation_id.clone(),
                    required: slot.required,
                    starting_savepoint: (*points[&slot.slot_id]).clone(),
                })
                .collect(),
            dependencies: self.graph.dependencies.clone(),
            conditions: self.graph.conditions.clone(),
        };
        graph.validate(&owner.session_id)?;
        Ok(graph)
    }
}

/// Implemented only by a host that resolves actual accepted history and proves
/// its selected projection for both exact definitions. Validation is repeated on
/// reopen. A missing validator refuses preservation across changed definitions;
/// storing a reference, a user choice, or matching model names cannot approve it.
pub trait SessionTeamHistoryValidator {
    fn validate_preservation(
        &self,
        identity: &DurableSessionIdentity,
        previous: &SessionTeamConversationSource,
        proposed: &SessionTeamSlot,
        evidence: &EvidenceRef,
        content: &ExecutionContentStore,
    ) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum SessionTeamError {
    #[error("Session team storage: {0}")]
    Io(#[from] io::Error),
    #[error("Session team encoding: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Session team evidence: {0}")]
    Content(#[from] ExecutionContentError),
    #[error("Session team canonical history: {0}")]
    Canonical(#[from] ExecutionStoreError),
    #[error("Session team graph: {0}")]
    Graph(#[from] TurnContractError),
    #[error("invalid Session team: {0}")]
    Invalid(&'static str),
    #[error("Session team belongs to another canonical owner")]
    OwnerConflict,
    #[error("Session team command identity already has a different payload")]
    CommandConflict,
    #[error("Session configuration changed; expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("Session team storage capacity is exhausted; no revision was applied")]
    Capacity,
    #[error("Session team write is uncertain; reopen before acknowledging configuration")]
    RecoveryRequired,
    #[error("accepted-history preservation has not been verified: {0}")]
    UnverifiedPreservation(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamData {
    schema_version: u32,
    canonical_journal_id: String,
    owner: ExecutionStoreOwner,
    revisions: Vec<SessionTeamRevision>,
}

/// One writer, bound to the canonical Session incarnation. All acknowledged
/// revisions remain retained; overflow refuses instead of evicting history.
pub struct SessionTeamStore {
    namespace: OwnedExecutionNamespace,
    data: TeamData,
    poisoned: bool,
}
impl SessionTeamStore {
    pub fn open_owned(
        namespace: OwnedExecutionNamespace,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<Self, SessionTeamError> {
        namespace.require_root(&ExecutionComponent::SessionTeam)?;
        if canonical.identity()? != *namespace.identity() {
            return Err(SessionTeamError::OwnerConflict);
        }
        content.require_owned_identity(namespace.identity())?;
        let data = match namespace.read_limited(FILE, MAX_RETAINED_CONTRACT_BYTES) {
            Ok(bytes) => serde_json::from_slice::<TeamData>(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                namespace.check_journal_creation(FILE)?;
                TeamData {
                    schema_version: SESSION_TEAM_SCHEMA_VERSION,
                    canonical_journal_id: namespace.identity().journal_id().into(),
                    owner: namespace.identity().owner().clone(),
                    revisions: vec![],
                }
            }
            Err(error) => return Err(error.into()),
        };
        if data.owner != *namespace.identity().owner()
            || data.canonical_journal_id != namespace.identity().journal_id()
        {
            return Err(SessionTeamError::OwnerConflict);
        }
        validate_data(&data, namespace.identity(), canonical, content, history)?;
        namespace.mark_journal_initialized(FILE)?;
        // Re-publish validated bytes to resolve any prior rename/fsync ambiguity.
        namespace.atomic_write(FILE, &encoded(&data, MAX_RETAINED_CONTRACT_BYTES)?)?;
        Ok(Self {
            namespace,
            data,
            poisoned: false,
        })
    }

    pub fn configuration_revision(&self) -> Result<u64, SessionTeamError> {
        self.healthy()?;
        Ok(self.data.revisions.len() as u64)
    }
    pub fn current(&self) -> Result<Option<&SessionTeamRevision>, SessionTeamError> {
        self.healthy()?;
        Ok(self.data.revisions.last())
    }
    pub fn get(&self, revision: u64) -> Result<Option<&SessionTeamRevision>, SessionTeamError> {
        self.healthy()?;
        Ok(revision
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.data.revisions.get(index)))
    }
    pub fn identity(&self) -> &DurableSessionIdentity {
        self.namespace.identity()
    }

    /// Validate the entire candidate and all retained inputs without changing
    /// this configuration journal. The returned projection is not an Apply receipt.
    pub fn preview(
        &self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<SessionTeamRevision, SessionTeamError> {
        self.prepare_commit(request, canonical, content, history)
            .map(|(record, _)| record)
    }

    pub fn commit(
        &mut self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<SessionTeamRevision, SessionTeamError> {
        self.commit_with(request, canonical, content, history, |namespace, bytes| {
            namespace.atomic_write(FILE, bytes)
        })
    }
    fn commit_with(
        &mut self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
        write: impl FnOnce(&OwnedExecutionNamespace, &[u8]) -> io::Result<()>,
    ) -> Result<SessionTeamRevision, SessionTeamError> {
        let (record, bytes) = self.prepare_commit(request, canonical, content, history)?;
        let Some(bytes) = bytes else {
            return Ok(record);
        };
        if let Err(error) = write(&self.namespace, &bytes) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.data.revisions.push(record.clone());
        Ok(record)
    }
    fn prepare_commit(
        &self,
        request: SessionTeamCommit,
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        history: Option<&dyn SessionTeamHistoryValidator>,
    ) -> Result<(SessionTeamRevision, Option<Vec<u8>>), SessionTeamError> {
        self.healthy()?;
        if canonical.identity()? != *self.identity() {
            return Err(SessionTeamError::OwnerConflict);
        }
        content.require_owned_identity(self.identity())?;
        encoded(&request, MAX_CONTRACT_ENVELOPE_BYTES)?;
        // An exact retry is its original configuration receipt, even after later
        // edits or a different provider projection becomes unavailable.
        if let Some(record) = self
            .data
            .revisions
            .iter()
            .find(|record| record.command_id == request.command_id)
        {
            return if record.request() == request {
                Ok((record.clone(), None))
            } else {
                Err(SessionTeamError::CommandConflict)
            };
        }
        let actual = self.data.revisions.len() as u64;
        if request.expected_configuration_revision != actual {
            return Err(SessionTeamError::RevisionConflict {
                expected: request.expected_configuration_revision,
                actual,
            });
        }
        if self.data.revisions.len() >= MAX_CONTRACT_COMMANDS {
            return Err(SessionTeamError::Capacity);
        }
        let record = SessionTeamRevision::from_commit(
            actual + 1,
            canonical.records()?.len() as u64,
            request,
        );
        let mut frontier = CanonicalTeamFrontier::default();
        frontier.advance(canonical, record.canonical_record_count)?;
        validate_revision(
            &record,
            &self.data.revisions,
            self.identity(),
            &frontier,
            content,
            history,
        )?;
        let next = TeamData {
            schema_version: self.data.schema_version,
            canonical_journal_id: self.data.canonical_journal_id.clone(),
            owner: self.data.owner.clone(),
            revisions: self
                .data
                .revisions
                .iter()
                .cloned()
                .chain(std::iter::once(record.clone()))
                .collect(),
        };
        let bytes = encoded(&next, MAX_RETAINED_CONTRACT_BYTES)?;
        Ok((record, Some(bytes)))
    }
    fn healthy(&self) -> Result<(), SessionTeamError> {
        if self.poisoned {
            return Err(SessionTeamError::RecoveryRequired);
        }
        self.namespace.verify_ambient_identity()?;
        Ok(())
    }
}

fn encoded(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, SessionTeamError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.limit {
                return Err(io::Error::other("Session team encoding limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| SessionTeamError::Capacity)?;
    Ok(writer.bytes)
}

#[derive(Default)]
struct CanonicalTeamFrontier {
    count: usize,
    graphs: HashMap<LogicalTurnId, TurnGraphSnapshot>,
    conversations: HashSet<NodeConversationId>,
}
impl CanonicalTeamFrontier {
    fn advance(
        &mut self,
        canonical: &SessionExecutionStore,
        count: u64,
    ) -> Result<(), SessionTeamError> {
        let count = usize::try_from(count).map_err(|_| SessionTeamError::Capacity)?;
        let records = canonical.records()?;
        if count < self.count || count > records.len() {
            return Err(SessionTeamError::Invalid(
                "configuration canonical prefix is absent or moves backward",
            ));
        }
        // The actual owned canonical store has already validated all envelopes.
        // Only its immutable graph declarations introduce conversation identity.
        for event in &records[self.count..count] {
            if let TurnContractEvent::Begin { graph, .. }
            | TurnContractEvent::ReviseGraph { graph, .. } = &event.event
            {
                self.conversations
                    .extend(graph.nodes.iter().map(|node| node.conversation_id.clone()));
                self.graphs.insert(event.turn_id.clone(), graph.clone());
            }
        }
        self.count = count;
        Ok(())
    }
}

fn validate_data(
    data: &TeamData,
    identity: &DurableSessionIdentity,
    canonical: &SessionExecutionStore,
    content: &ExecutionContentStore,
    history: Option<&dyn SessionTeamHistoryValidator>,
) -> Result<(), SessionTeamError> {
    if data.schema_version != SESSION_TEAM_SCHEMA_VERSION {
        return Err(SessionTeamError::Invalid("unsupported team store schema"));
    }
    if data.revisions.len() > MAX_CONTRACT_COMMANDS {
        return Err(SessionTeamError::Capacity);
    }
    let mut commands = HashSet::new();
    let mut frontier = CanonicalTeamFrontier::default();
    for (index, record) in data.revisions.iter().enumerate() {
        if !commands.insert(&record.command_id) {
            return Err(SessionTeamError::CommandConflict);
        }
        frontier.advance(canonical, record.canonical_record_count)?;
        validate_revision(
            record,
            &data.revisions[..index],
            identity,
            &frontier,
            content,
            history,
        )?;
    }
    encoded(data, MAX_RETAINED_CONTRACT_BYTES)?;
    Ok(())
}

fn validate_revision(
    record: &SessionTeamRevision,
    prior: &[SessionTeamRevision],
    identity: &DurableSessionIdentity,
    frontier: &CanonicalTeamFrontier,
    content: &ExecutionContentStore,
    history: Option<&dyn SessionTeamHistoryValidator>,
) -> Result<(), SessionTeamError> {
    if record.schema_version != SESSION_TEAM_SCHEMA_VERSION
        || record.configuration_revision != prior.len() as u64 + 1
        || record.expected_configuration_revision != prior.len() as u64
    {
        return Err(SessionTeamError::Invalid(
            "team configuration revision is not its exact predecessor",
        ));
    }
    encoded(record, MAX_CONTRACT_ENVELOPE_BYTES)?;
    if record.canonical_record_count != frontier.count as u64 {
        return Err(SessionTeamError::Invalid(
            "configuration has a different canonical prefix",
        ));
    }
    let imported = match &record.initial_source {
        Some(source) => {
            if !prior.is_empty() {
                return Err(SessionTeamError::Invalid(
                    "initial team source is permitted only before its first revision",
                ));
            }
            let graph = frontier
                .graphs
                .get(&source.turn_id)
                .ok_or(SessionTeamError::Invalid(
                    "initial team source has no graph in the acknowledged canonical prefix",
                ))?;
            if graph.snapshot_id != source.snapshot_id || graph.revision != source.graph_revision {
                return Err(SessionTeamError::Invalid(
                    "initial team source differs from its exact retained graph",
                ));
            }
            Some(graph)
        }
        None => None,
    };

    let empty = record
        .graph
        .slots
        .iter()
        .map(|slot| (slot.slot_id.clone(), ConversationSavepoint::Empty))
        .collect::<Vec<_>>();
    record.initial_graph(
        identity.owner(),
        GraphSnapshotId::new("team-configuration-validation")?,
        &empty,
    )?;
    let slots: HashMap<_, _> = record
        .graph
        .slots
        .iter()
        .map(|slot| (&slot.slot_id, slot))
        .collect();
    let decisions: HashMap<_, _> = record
        .continuity
        .iter()
        .map(|item| (&item.slot_id, &item.decision))
        .collect();
    if decisions.len() != record.continuity.len()
        || decisions.len() != slots.len()
        || decisions.keys().any(|slot| !slots.contains_key(slot))
    {
        return Err(SessionTeamError::Invalid(
            "every proposed slot requires one explicit continuity decision",
        ));
    }
    if record.layout.len() > MAX_CONTRACT_NODES {
        return Err(SessionTeamError::Capacity);
    }
    let mut positions = HashSet::new();
    for position in &record.layout {
        if !position.x.is_finite()
            || !position.y.is_finite()
            || !slots.contains_key(&position.slot_id)
            || !positions.insert(&position.slot_id)
        {
            return Err(SessionTeamError::Invalid(
                "layout requires unique declared slots and finite coordinates",
            ));
        }
    }
    for slot in &record.graph.slots {
        match content.resolve_activation_evidence(&slot.definition.snapshot)? {
            ActivationEvidenceContent::Definition {
                definition_id,
                revision,
                profile,
                ..
            } if definition_id == &slot.definition.definition_id
                && *revision > 0
                && profile.definition == definition_id.as_str() => {}
            _ => {
                return Err(SessionTeamError::Invalid(
                    "slot definition differs from exact retained definition evidence",
                ))
            }
        }
        if !matches!(
            content.resolve_activation_evidence(&slot.budget)?,
            ActivationEvidenceContent::Budget { .. }
        ) {
            return Err(SessionTeamError::Invalid(
                "slot budget is missing or has the wrong evidence role",
            ));
        }
        if let Some(reference) = &slot.grant {
            let ActivationEvidenceContent::Grant { policy } =
                content.resolve_activation_evidence(reference)?
            else {
                return Err(SessionTeamError::Invalid(
                    "slot grant has the wrong retained evidence role",
                ));
            };
            let ActivationEvidenceContent::Definition { profile, .. } =
                content.resolve_activation_evidence(&slot.definition.snapshot)?
            else {
                return Err(SessionTeamError::Invalid(
                    "slot grant has no retained definition profile",
                ));
            };
            let ActivationEvidenceContent::Budget { limits } =
                content.resolve_activation_evidence(&slot.budget)?
            else {
                return Err(SessionTeamError::Invalid(
                    "slot grant has no retained budget",
                ));
            };
            if policy.holder != slot.node_id
                || !policy.profiles.contains(profile)
                || &policy.limits != limits
            {
                return Err(SessionTeamError::Invalid(
                    "slot grant differs from its exact holder, definition profile or budget",
                ));
            }
            content.resolve_activation_evidence(&policy.issuer_evidence)?;
        }
        let previous = prior
            .last()
            .and_then(|record| {
                record
                    .graph
                    .slots
                    .iter()
                    .find(|old| old.slot_id == slot.slot_id)
            })
            .map(|old| SessionTeamConversationSource {
                slot_id: old.slot_id.clone(),
                definition: old.definition.clone(),
                conversation_id: old.conversation_id.clone(),
            })
            .or_else(|| {
                imported
                    .and_then(|graph| graph.nodes.iter().find(|node| node.slot_id == slot.slot_id))
                    .map(|node| SessionTeamConversationSource {
                        slot_id: node.slot_id.clone(),
                        definition: node.definition.clone(),
                        conversation_id: node.conversation_id.clone(),
                    })
            });
        match decisions[&slot.slot_id] {
            SessionTeamContinuity::Reset => {
                if frontier.conversations.contains(&slot.conversation_id)
                    || prior.iter().any(|record| {
                        record
                            .graph
                            .slots
                            .iter()
                            .any(|old| old.conversation_id == slot.conversation_id)
                    })
                {
                    return Err(SessionTeamError::Invalid(
                        "reset requires a conversation never used by this Session team",
                    ));
                }
            }
            SessionTeamContinuity::PreserveUnchanged => {
                let old = previous.ok_or(SessionTeamError::Invalid(
                    "only a current slot can preserve its conversation",
                ))?;
                if old.definition != slot.definition || old.conversation_id != slot.conversation_id
                {
                    return Err(SessionTeamError::Invalid(
                        "changed definition or conversation requires reset or verified projection",
                    ));
                }
            }
            SessionTeamContinuity::PreserveWithProjection { evidence } => {
                let old = previous.ok_or(SessionTeamError::Invalid(
                    "projection requires an exact current predecessor slot",
                ))?;
                if old.conversation_id != slot.conversation_id {
                    return Err(SessionTeamError::Invalid(
                        "preservation must retain the exact predecessor conversation",
                    ));
                }
                history
                    .ok_or_else(|| {
                        SessionTeamError::UnverifiedPreservation(
                            "no accepted-history projection validator is installed".into(),
                        )
                    })?
                    .validate_preservation(identity, &old, slot, evidence, content)
                    .map_err(SessionTeamError::UnverifiedPreservation)?;
            }
        }
    }
    for condition in &record.graph.conditions {
        match &condition.kind {
            ConditionKind::RepositoryCheck { definition } => {
                content.resolve_repository_check_definition(definition)?;
            }
            ConditionKind::Review { criterion } => {
                if !matches!(
                    content.resolve_activation_evidence(criterion)?,
                    ActivationEvidenceContent::Guidance { .. }
                ) {
                    return Err(SessionTeamError::Invalid(
                        "review criterion lacks retained instruction evidence",
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "session_team_tests.rs"]
mod tests;
