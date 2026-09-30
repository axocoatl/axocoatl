//! Versioned, read-only Session history snapshots.
//!
//! The host owns format selection and migration provenance. Legacy construction
//! reads an actual ledger; upgraded construction reads only its canonically
//! sealed frontier and exact owned content. These projections confer no runtime
//! authority and must never be used to reconstruct accepted actor state.

use std::collections::HashSet;

use serde::Serialize;

use crate::control_command::ControlCommandStore;
use crate::execution_content::{
    ActivationStreamPayload, ContentResolution, ExecutionContentError, ExecutionContentStore,
    ExecutionTurnView,
};
use crate::execution_store::{ExecutionStoreError, SessionExecutionStore};
use crate::turn_contract::{ActivationState, LogicalTurnId, TurnContractEvent};

#[path = "session_history_guidance.rs"]
mod guidance;
use crate::turn_ledger::{
    transcript_messages_for_turn, SessionTranscriptMessage, SessionTranscriptRole, SessionTurn,
    SessionTurnLifecycle, SessionTurnSearchHit, SessionTurnStore, TurnSearchField,
};
pub use guidance::append_guidance_markdown;

#[derive(Debug, thiserror::Error)]
pub enum SessionHistoryError {
    #[error("canonical history: {0}")]
    Canonical(#[from] ExecutionStoreError),
    #[error("history content: {0}")]
    Content(#[from] ExecutionContentError),
    #[error("upgraded Session history has no canonical legacy seal")]
    MissingLegacySeal,
    #[error("duplicate history turn identity: {0}")]
    TurnIdentityCollision(String),
    #[error("rewind names a turn outside this Session history: {0}")]
    UnknownRewindTurn(String),
    #[error("Session history contains v2 execution; this consumer requires exact legacy rows")]
    RequiresVersionedConsumer,
    #[error("history serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryVisibility {
    Visible,
    IncludingSuperseded,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "history_version", content = "turn", rename_all = "snake_case")]
pub enum SessionHistoryEntry {
    LegacyV1(Box<SessionTurn>),
    ExecutionV2(Box<ExecutionTurnView>),
}

impl SessionHistoryEntry {
    pub fn turn_id(&self) -> &str {
        match self {
            Self::LegacyV1(turn) => &turn.id,
            Self::ExecutionV2(turn) => turn.turn_id.as_str(),
        }
    }

    pub fn is_visible(&self) -> bool {
        match self {
            Self::LegacyV1(turn) => !turn.superseded,
            Self::ExecutionV2(turn) => !turn.superseded,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionHistorySearchHit {
    pub entry: SessionHistoryEntry,
    pub matched_fields: Vec<TurnSearchField>,
}

/// Legacy messages retain their exact historical projection. V2 is grouped by
/// logical turn so accepted outputs, partial evidence, missing bodies and state
/// stay explicit; this is a presentation transcript, not model input.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "history_version",
    content = "content",
    rename_all = "snake_case"
)]
pub enum SessionHistoryTranscriptEntry {
    LegacyV1(Box<SessionTranscriptMessage>),
    ExecutionV2(Box<ExecutionTurnView>),
}

/// A coherent read snapshot, not a live view or an authority token. Callers hold
/// their store/controller synchronization while constructing the snapshot.
#[derive(Debug, Clone)]
pub struct SessionHistory {
    session_id: String,
    entries: Vec<SessionHistoryEntry>,
}

/// A legacy catalog has the actual ledger's cross-Session append order. There
/// is deliberately no constructor merging v2 journals: their independent local
/// sequences do not establish a global ordering or a global history authority.
#[derive(Clone)]
pub struct SessionHistoryCatalog<'a> {
    legacy: &'a SessionTurnStore,
}

impl<'a> SessionHistoryCatalog<'a> {
    pub fn from_legacy(legacy: &'a SessionTurnStore) -> Result<Self, SessionHistoryError> {
        // The actual ledger fold already validated global turn uniqueness.
        // Borrow it under the caller's lock instead of copying every body for
        // an ordinary exact lookup or small search result.
        Ok(Self { legacy })
    }

    /// Explicitly materialize catalog entries. Ordinary lookup/search only
    /// clone matching rows and do not allocate a second global history copy.
    pub fn entries(&self, visibility: HistoryVisibility) -> Vec<SessionHistoryEntry> {
        self.legacy
            .ordered_turns()
            .filter(|turn| visibility == HistoryVisibility::IncludingSuperseded || !turn.superseded)
            .cloned()
            .map(|turn| SessionHistoryEntry::LegacyV1(Box::new(turn)))
            .collect()
    }

    pub fn search(&self, query: &str) -> Vec<SessionHistorySearchHit> {
        self.legacy
            .search(None, query)
            .into_iter()
            .map(|hit| SessionHistorySearchHit {
                entry: SessionHistoryEntry::LegacyV1(Box::new(hit.turn)),
                matched_fields: hit.matched_fields,
            })
            .collect()
    }

    pub fn legacy_search(
        &self,
        query: &str,
    ) -> Result<Vec<SessionTurnSearchHit>, SessionHistoryError> {
        // This catalog can only borrow an actual legacy ledger. No v2/mixed
        // constructor exists that could silently drop unsupported entries.
        Ok(self.legacy.search(None, query))
    }

    /// Global exact lookup preserves the legacy caller's ability to detect an
    /// ID owned by a different Session, including superseded rows.
    pub fn legacy_get(&self, turn_id: &str) -> Result<Option<SessionTurn>, SessionHistoryError> {
        Ok(self.legacy.get(turn_id))
    }
}

impl SessionHistory {
    pub fn from_legacy(
        legacy: &SessionTurnStore,
        session_id: &str,
    ) -> Result<Self, SessionHistoryError> {
        let entries = legacy
            .list_including_superseded(session_id)
            .into_iter()
            .map(|turn| SessionHistoryEntry::LegacyV1(Box::new(turn)))
            .collect();
        Self::checked(session_id.to_string(), entries)
    }

    pub fn from_upgraded(
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
    ) -> Result<Self, SessionHistoryError> {
        Self::build_upgraded(canonical, content, None)
    }

    /// Current receipts come from the actual retained controller store, never
    /// from serialized caller input. Older closed turns use protected reads.
    pub fn from_upgraded_with_commands(
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        turn_id: &LogicalTurnId,
        commands: &ControlCommandStore,
    ) -> Result<Self, SessionHistoryError> {
        Self::build_upgraded(canonical, content, Some((turn_id, commands)))
    }

    fn build_upgraded(
        canonical: &SessionExecutionStore,
        content: &ExecutionContentStore,
        current_commands: Option<(&LogicalTurnId, &ControlCommandStore)>,
    ) -> Result<Self, SessionHistoryError> {
        content.verify_canonical_owner(canonical)?;
        let mut entries = if let Some(seal) = canonical.legacy_seal()? {
            content
                .read_legacy_history(&seal)?
                .turns
                .iter()
                .cloned()
                .map(|turn| SessionHistoryEntry::LegacyV1(Box::new(turn)))
                .collect::<Vec<_>>()
        } else if canonical.native_origin()?.is_some() {
            Vec::new()
        } else {
            return Err(SessionHistoryError::MissingLegacySeal);
        };
        // Journal Begin order is canonical even when a later request contains
        // an older clock timestamp. Continuation never creates a second row.
        for record in canonical.records()? {
            if matches!(record.event, TurnContractEvent::Begin { .. }) {
                let snapshot = canonical.snapshot(&record.turn_id)?;
                let mut view = content.project(&snapshot)?;
                if view
                    .activations
                    .iter()
                    .any(|activation| !activation.guidance.is_empty())
                {
                    let receipts = match current_commands {
                        Some((turn_id, commands)) if turn_id == snapshot.turn_id() => {
                            commands.read_owned_views(canonical, turn_id)
                        }
                        _ => ControlCommandStore::read_historical_views(
                            canonical,
                            snapshot.turn_id(),
                        ),
                    };
                    guidance::join_delivery(canonical, &mut view, receipts.as_deref().ok());
                }
                entries.push(SessionHistoryEntry::ExecutionV2(Box::new(view)));
            }
        }
        Self::checked(canonical.owner().session_id.as_str().to_string(), entries)
    }

    fn checked(
        session_id: String,
        entries: Vec<SessionHistoryEntry>,
    ) -> Result<Self, SessionHistoryError> {
        require_unique_ids(&entries)?;
        Ok(Self {
            session_id,
            entries,
        })
    }

    /// Apply exact IDs from the owning durable rewind journal. Raw lookup and
    /// IncludingSuperseded remain available; no execution evidence is erased.
    pub fn apply_superseded(&mut self, turns: &[String]) -> Result<(), SessionHistoryError> {
        for id in turns {
            let entry = self
                .entries
                .iter_mut()
                .find(|entry| entry.turn_id() == id)
                .ok_or_else(|| SessionHistoryError::UnknownRewindTurn(id.clone()))?;
            match entry {
                SessionHistoryEntry::LegacyV1(turn) => turn.superseded = true,
                SessionHistoryEntry::ExecutionV2(turn) => turn.superseded = true,
            }
        }
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn entries(&self, visibility: HistoryVisibility) -> Vec<&SessionHistoryEntry> {
        visible_entries(&self.entries, visibility)
    }

    /// Exact identity lookup includes superseded legacy rows for audit, matching
    /// the legacy `get` contract. Callers apply presentation visibility explicitly.
    pub fn get(&self, turn_id: &str) -> Option<&SessionHistoryEntry> {
        self.entries.iter().find(|entry| entry.turn_id() == turn_id)
    }

    pub fn legacy_get(&self, turn_id: &str) -> Result<Option<SessionTurn>, SessionHistoryError> {
        legacy_get_entry(&self.entries, turn_id)
    }

    pub fn search(&self, query: &str) -> Vec<SessionHistorySearchHit> {
        search_entries(&self.entries, query)
    }

    pub fn transcript(&self) -> Vec<SessionHistoryTranscriptEntry> {
        let mut transcript = Vec::new();
        for entry in self.entries(HistoryVisibility::Visible) {
            match entry {
                SessionHistoryEntry::LegacyV1(turn) => transcript.extend(
                    transcript_messages_for_turn(turn)
                        .into_iter()
                        .map(|message| SessionHistoryTranscriptEntry::LegacyV1(Box::new(message))),
                ),
                SessionHistoryEntry::ExecutionV2(turn) => {
                    transcript.push(SessionHistoryTranscriptEntry::ExecutionV2(turn.clone()))
                }
            }
        }
        transcript
    }

    /// Compatibility is all-or-nothing. Even a caller requesting hidden rows
    /// cannot accidentally drop v2 work and mistake a legacy prefix for history.
    pub fn legacy_rows(
        &self,
        visibility: HistoryVisibility,
    ) -> Result<Vec<SessionTurn>, SessionHistoryError> {
        self.require_legacy_consumer()?;
        Ok(self
            .entries(visibility)
            .into_iter()
            .filter_map(|entry| match entry {
                SessionHistoryEntry::LegacyV1(turn) => Some(turn.as_ref().clone()),
                SessionHistoryEntry::ExecutionV2(_) => None,
            })
            .collect())
    }

    pub fn legacy_transcript(&self) -> Result<Vec<SessionTranscriptMessage>, SessionHistoryError> {
        Ok(self
            .legacy_rows(HistoryVisibility::Visible)?
            .iter()
            .flat_map(transcript_messages_for_turn)
            .collect())
    }

    pub fn legacy_search(
        &self,
        query: &str,
    ) -> Result<Vec<SessionTurnSearchHit>, SessionHistoryError> {
        legacy_search_entries(&self.entries, query)
    }

    fn require_legacy_consumer(&self) -> Result<(), SessionHistoryError> {
        require_legacy_entries(&self.entries)
    }

    /// Explicitly versioned export, distinct from the legacy JSON array API.
    pub fn export_json(
        &self,
        visibility: HistoryVisibility,
    ) -> Result<String, SessionHistoryError> {
        Ok(serde_json::to_string_pretty(&self.entries(visibility))?)
    }

    /// Human-readable presentation. Missing content remains visible, while
    /// timestamps and tool outcomes absent from the read view are not invented.
    pub fn export_markdown(&self) -> String {
        let mut markdown = String::new();
        for entry in self.entries(HistoryVisibility::Visible) {
            match entry {
                SessionHistoryEntry::LegacyV1(turn) => {
                    markdown.push_str(&format!("## Legacy turn {}\n\n", turn.id));
                    for message in transcript_messages_for_turn(turn) {
                        let role = match message.role {
                            SessionTranscriptRole::User => "User".to_string(),
                            SessionTranscriptRole::Assistant => message.agent_id.map_or_else(
                                || "Assistant".to_string(),
                                |id| format!("Assistant ({id})"),
                            ),
                        };
                        markdown.push_str(&format!("### {role}\n\n{}\n\n", message.content));
                    }
                    if turn.status != SessionTurnLifecycle::Completed {
                        markdown.push_str(&format!("Turn status: {:?}\n\n", turn.status));
                    }
                    if let Some(error) = &turn.error {
                        markdown.push_str(&format!("Error: {error}\n\n"));
                    }
                }
                SessionHistoryEntry::ExecutionV2(turn) => {
                    markdown.push_str(&format!(
                        "## Execution turn {}\n\nLogical state: {:?}\n\n",
                        turn.turn_id.as_str(),
                        turn.state
                    ));
                    match &turn.request {
                        ContentResolution::Available { content, .. } => {
                            markdown.push_str(&format!("### User\n\n{}\n\n", content.display_input))
                        }
                        resolution => unavailable(&mut markdown, "Request", resolution),
                    }
                    append_turn_stop_markdown(&mut markdown, turn);
                    for activation in &turn.activations {
                        markdown.push_str(&format!(
                            "### Activation {}\n\nState: {:?}; currently accepted: {}\n\n",
                            activation.activation.activation.activation_id.as_str(),
                            activation.activation.state,
                            activation.currently_accepted
                        ));
                        match &activation.output {
                            ContentResolution::Available { content, .. } => markdown.push_str(
                                &format!("Final output evidence:\n\n{}\n\n", content.text),
                            ),
                            resolution => unavailable(&mut markdown, "Final output", resolution),
                        }
                        for partial in &activation.partial_outputs {
                            markdown.push_str(&format!(
                                "Partial output evidence (not acceptance):\n\n{}\n\n",
                                partial.text
                            ));
                        }
                        append_reserved_output_markdown(&mut markdown, activation);
                        append_guidance_markdown(&mut markdown, activation);
                    }
                }
            }
        }
        markdown
    }
}

fn require_unique_ids(entries: &[SessionHistoryEntry]) -> Result<(), SessionHistoryError> {
    let mut ids = HashSet::new();
    for entry in entries {
        if !ids.insert(entry.turn_id()) {
            return Err(SessionHistoryError::TurnIdentityCollision(
                entry.turn_id().to_string(),
            ));
        }
    }
    Ok(())
}

fn visible_entries(
    entries: &[SessionHistoryEntry],
    visibility: HistoryVisibility,
) -> Vec<&SessionHistoryEntry> {
    entries
        .iter()
        .filter(|entry| visibility == HistoryVisibility::IncludingSuperseded || entry.is_visible())
        .collect()
}

fn require_legacy_entries(entries: &[SessionHistoryEntry]) -> Result<(), SessionHistoryError> {
    if entries
        .iter()
        .any(|entry| matches!(entry, SessionHistoryEntry::ExecutionV2(_)))
    {
        Err(SessionHistoryError::RequiresVersionedConsumer)
    } else {
        Ok(())
    }
}

fn legacy_get_entry(
    entries: &[SessionHistoryEntry],
    turn_id: &str,
) -> Result<Option<SessionTurn>, SessionHistoryError> {
    require_legacy_entries(entries)?;
    Ok(entries.iter().find_map(|entry| match entry {
        SessionHistoryEntry::LegacyV1(turn) if turn.id == turn_id => Some(turn.as_ref().clone()),
        _ => None,
    }))
}

fn search_entries(entries: &[SessionHistoryEntry], query: &str) -> Vec<SessionHistorySearchHit> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    visible_entries(entries, HistoryVisibility::Visible)
        .into_iter()
        .filter_map(|entry| {
            let matched_fields = match entry {
                SessionHistoryEntry::LegacyV1(turn) => {
                    SessionTurnStore::matching_fields(turn, &query)
                }
                SessionHistoryEntry::ExecutionV2(turn) => execution_matches(turn, &query),
            };
            (!matched_fields.is_empty()).then(|| SessionHistorySearchHit {
                entry: entry.clone(),
                matched_fields,
            })
        })
        .collect()
}

fn legacy_search_entries(
    entries: &[SessionHistoryEntry],
    query: &str,
) -> Result<Vec<SessionTurnSearchHit>, SessionHistoryError> {
    require_legacy_entries(entries)?;
    Ok(search_entries(entries, query)
        .into_iter()
        .filter_map(|hit| match hit.entry {
            SessionHistoryEntry::LegacyV1(turn) => Some(SessionTurnSearchHit {
                turn: *turn,
                matched_fields: hit.matched_fields,
            }),
            SessionHistoryEntry::ExecutionV2(_) => None,
        })
        .collect())
}

fn execution_matches(turn: &ExecutionTurnView, query: &str) -> Vec<TurnSearchField> {
    let contains = |value: &str| value.to_lowercase().contains(query);
    let mut matched = Vec::new();
    if let ContentResolution::Available { content, .. } = &turn.request {
        // Search the visible user request, not hidden prompt augmentation.
        if contains(&content.display_input) {
            matched.push(TurnSearchField::UserInput);
        }
        if content.context.iter().any(|context| {
            contains(&context.display_name)
                || contains(&context.kind)
                || context.origin.as_deref().is_some_and(contains)
        }) {
            matched.push(TurnSearchField::Context);
        }
    }
    if turn.activations.iter().any(|activation| {
        activation.activation.state != ActivationState::Superseded
            && (matches!(&activation.output, ContentResolution::Available { content, .. }
                if contains(&content.text))
                || activation
                    .partial_outputs
                    .iter()
                    .any(|output| contains(&output.text))
                || activation
                    .reserved_outputs
                    .iter()
                    .any(|output| contains(&output.content.output.text))
                || contains(
                    &activation
                        .stream
                        .iter()
                        .filter_map(|event| match &event.content.payload {
                            ActivationStreamPayload::Text { delta } => Some(delta.as_str()),
                            ActivationStreamPayload::ProviderRetry { .. } => Some("\n"),
                            _ => None,
                        })
                        .collect::<String>(),
                ))
    }) {
        matched.push(TurnSearchField::Output);
    }
    // Guidance is recorded input context, not Agent output. Keep the existing
    // superseded-activation search policy; missing bodies cannot match text.
    if turn.activations.iter().any(|activation| {
        activation.activation.state != ActivationState::Superseded
            && activation.guidance.iter().any(|item| {
                matches!(&item.instruction, ContentResolution::Available { content, .. }
                    if contains(content))
            })
    }) && !matched.contains(&TurnSearchField::Context)
    {
        matched.push(TurnSearchField::Context);
    }
    if turn.stop_requested.as_ref().is_some_and(|intent| {
        contains(if intent.partial_finish.is_some() {
            "Partial Finish confirmed"
        } else {
            "Stop requested"
        }) || intent
            .unrun_nodes
            .iter()
            .any(|node| contains(&format!("{} · Stopped before starting", node.as_str())))
    }) && !matched.contains(&TurnSearchField::Context)
    {
        matched.push(TurnSearchField::Context);
    }
    // Failure references do not contain an error message. Do not search their
    // opaque identifiers as though they were resolved failure evidence.
    matched
}

fn unavailable<T>(markdown: &mut String, label: &str, resolution: &ContentResolution<T>) {
    match resolution {
        ContentResolution::NotRecorded => markdown.push_str(&format!("{label}: not recorded.\n\n")),
        ContentResolution::Missing { reference } => markdown.push_str(&format!(
            "{label}: unavailable (reference {}).\n\n",
            reference.as_str()
        )),
        ContentResolution::Available { .. } => {}
    }
}

/// Append retained bounded output evidence for presentation exports. This
/// preserves partial/truncated/empty bodies and never establishes acceptance.
/// An available final body's exact reference is already rendered by the caller.
pub fn append_reserved_output_markdown(
    markdown: &mut String,
    activation: &crate::execution_content::ExecutionActivationView,
) {
    use crate::execution_content::OutputKind;
    for reserved in &activation.reserved_outputs {
        if matches!(&activation.output, ContentResolution::Available { reference, .. } if reference == &reserved.reference)
        {
            continue;
        }
        let retained = &reserved.content;
        let partial = retained.output.kind == OutputKind::Partial;
        markdown.push_str(if partial {
            "Partial output evidence (not acceptance):\n\n"
        } else {
            "Output evidence (not acceptance):\n\n"
        });
        if retained.output.text.is_empty() {
            markdown.push_str(if partial {
                "Recorded empty partial output.\n\n"
            } else {
                "Recorded empty output.\n\n"
            });
        } else {
            markdown.push_str(&retained.output.text);
            markdown.push_str("\n\n");
        }
        markdown.push_str(&format!("Evidence: `{}`.\n\n", reserved.reference.as_str()));
        if retained.original_byte_len > retained.output.text.len() as u64 {
            markdown.push_str(&format!(
                "Truncated recorded evidence: retained {} of {} bytes; original SHA-256 `{}`.\n\n",
                retained.output.text.len(),
                retained.original_byte_len,
                retained.original_sha256,
            ));
        }
    }
}

/// Preserve the exact Stop request and its never-started nodes. This is turn
/// evidence; do not synthesize generations or infer that effects were undone.
pub fn append_turn_stop_markdown(markdown: &mut String, turn: &ExecutionTurnView) {
    if let Some(intent) = &turn.stop_requested {
        markdown.push_str(&format!(
            "{} (revision {}). Command: `{}`; source evidence: `{}`.\n\n",
            if intent.partial_finish.is_some() {
                "Partial Finish confirmed"
            } else {
                "Stop requested"
            },
            intent.requested_revision,
            intent.command_id.as_str(),
            intent.evidence.as_str()
        ));
        for node in &intent.unrun_nodes {
            markdown.push_str(&format!(
                "- `{}` · {}\n",
                node.as_str(),
                if intent.partial_finish.is_some() {
                    "Skipped by partial Finish"
                } else {
                    "Stopped before starting"
                }
            ));
        }
        if let Some(selection) = &intent.partial_finish {
            markdown.push_str("Selected accepted sink results (only these supply the final answer and conversation):\n");
            for activation in &selection.selected_activations {
                markdown.push_str(&format!(
                    "- `{}` · generation {} · activation `{}`\n",
                    activation.node_id.as_str(),
                    activation.generation,
                    activation.activation_id.as_str()
                ));
            }
            if selection.selected_activations.is_empty() {
                markdown.push_str("- No result selected.\n");
            }
            markdown.push_str("Missing conditions (not marked passed):\n");
            for condition in &selection.missing_condition_ids {
                markdown.push_str(&format!("- `{}`\n", condition.as_str()));
            }
            markdown.push_str("Missing-condition evidence:\n");
            for evidence in &selection.missing_conditions {
                markdown.push_str(&format!("- `{}`\n", evidence.as_str()));
            }
            markdown.push_str("The retained human approval contains the exact condition IDs and complete stop/skip review.\n");
        }
        markdown.push('\n');
    }
}
