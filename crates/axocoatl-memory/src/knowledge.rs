//! Workspace-owned knowledge, separate from actor recall and execution state.
//!
//! Accepted revisions are immutable Markdown documents. One atomically replaced
//! manifest publishes a revision and its proposal receipt together. A failed write
//! fences this instance; reopening validates every referenced document. Orphaned
//! immutable documents are harmless and never become accepted by directory order.
//! The caller serializes this store and binds it to an authorized workspace.
use std::collections::{BTreeMap, BTreeSet};

use axocoatl_core::SecureDir;
use axocoatl_session::execution_store::DurableTurnSnapshot;
use axocoatl_session::turn_contract::{ActivationRef, LogicalTurnState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[path = "knowledge_index.rs"]
mod index;
pub use index::*;

const STATE_FILE: &str = "knowledge.json";
const STATE_LIMIT: usize = 8 * 1024 * 1024;
const DOCUMENT_LIMIT: usize = 96 * 1024;
const MAX_RECORDS: usize = 1024;
const MAX_VERSIONS: usize = 4096;
const MAX_PROPOSALS: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("knowledge I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("knowledge JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid knowledge: {0}")]
    Invalid(String),
    #[error("knowledge revision conflict: expected {expected}, current {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("knowledge not found: {0}")]
    NotFound(String),
    #[error("knowledge capacity exceeded")]
    Capacity,
    #[error("knowledge write uncertain; reopen the store before continuing")]
    RecoveryRequired,
}
pub type KnowledgeResult<T> = Result<T, KnowledgeError>;
pub type SnapshotSources = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    Decision,
    Architecture,
    Convention,
    Finding,
    Pitfall,
    Note,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeLinkKind {
    Supports,
    DependsOn,
    Supersedes,
    Related,
    UsedBy,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeLink {
    pub kind: KnowledgeLinkKind,
    pub target: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeSource {
    pub path: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// For a finding: the file that must change, or supporting evidence.
    #[serde(default, skip_serializing_if = "SourceRole::is_must_change")]
    pub role: SourceRole,
}

/// What a cited file is to a note. Only a file that must change routes a
/// finding to the Agent that watches it; evidence is shown, never routed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    #[default]
    MustChange,
    Evidence,
}

impl SourceRole {
    pub fn is_must_change(&self) -> bool {
        matches!(self, SourceRole::MustChange)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KnowledgeProvenance {
    Human {
        #[serde(default)]
        author: Option<String>,
    },
    Model {
        journal_id: String,
        activation: ActivationRef,
    },
    Observed {
        journal_id: String,
        activation: ActivationRef,
        evidence: Vec<String>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KnowledgeAcceptance {
    Human,
    AcceptedActivation {
        journal_id: String,
        activation: ActivationRef,
        closure_revision: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeDraft {
    pub id: String,
    pub title: String,
    pub body: String,
    pub kind: KnowledgeKind,
    #[serde(default)]
    pub links: Vec<KnowledgeLink>,
    #[serde(default)]
    pub sources: Vec<KnowledgeSource>,
    pub provenance: KnowledgeProvenance,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeRecord {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub kind: KnowledgeKind,
    pub revision: u64,
    pub links: Vec<KnowledgeLink>,
    pub sources: Vec<KnowledgeSource>,
    pub provenance: KnowledgeProvenance,
    pub acceptance: KnowledgeAcceptance,
}
impl KnowledgeRecord {
    pub fn draft(&self) -> KnowledgeDraft {
        KnowledgeDraft {
            id: self.id.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            kind: self.kind,
            links: self.links.clone(),
            sources: self.sources.clone(),
            provenance: self.provenance.clone(),
        }
    }
    /// Freshness is relative to this caller's exact checkout, not a workspace-global flag.
    pub fn applicability(&self, snapshot: &SnapshotSources) -> Vec<KnowledgeSourceStatus> {
        self.sources
            .iter()
            .map(|source| KnowledgeSourceStatus {
                source: source.clone(),
                status: match snapshot.get(&source.path) {
                    Some(hash) if hash == &source.sha256 => SourceApplicability::Current,
                    Some(_) => SourceApplicability::Changed,
                    None => SourceApplicability::Unavailable,
                },
            })
            .collect()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceApplicability {
    Current,
    Changed,
    Unavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeSourceStatus {
    pub source: KnowledgeSource,
    pub status: SourceApplicability,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Published,
    Rejected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeProposal {
    pub id: String,
    pub note: KnowledgeDraft,
    pub expected_revision: u64,
    pub status: ProposalStatus,
    pub published_revision: Option<u64>,
    pub journal_id: String,
    pub activation: ActivationRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeBacklink {
    pub id: String,
    pub title: String,
    pub revision: u64,
    pub kind: KnowledgeLinkKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeSearchHit {
    pub id: String,
    pub title: String,
    pub revision: u64,
    pub kind: KnowledgeKind,
    pub excerpt: String,
    pub truncated: bool,
    pub score: usize,
    pub sources: Vec<KnowledgeSourceStatus>,
    pub provenance: KnowledgeProvenance,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    revision: u64,
    file: String,
    sha256: String,
    bytes: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    workspace_id: Option<String>,
    records: BTreeMap<String, Vec<Version>>,
    proposals: BTreeMap<String, KnowledgeProposal>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema: 1,
            workspace_id: None,
            records: BTreeMap::new(),
            proposals: BTreeMap::new(),
        }
    }
}
pub struct KnowledgeStore {
    root: SecureDir,
    state: State,
    uncertain: bool,
}
impl KnowledgeStore {
    pub fn open(root: SecureDir) -> KnowledgeResult<Self> {
        // Opening a child obtains an independent descriptor without reopening an
        // ambient path. Even clones of the supplied capability cannot share a lock.
        let root = root.child("v1")?;
        #[cfg(unix)]
        root.lock_exclusive_waiting(axocoatl_core::LOCK_INHERITANCE_GRACE)?;
        let state = if root.is_file(STATE_FILE)? {
            serde_json::from_slice(&root.read_limited(STATE_FILE, STATE_LIMIT)?)?
        } else {
            State::default()
        };
        let store = Self {
            root,
            state,
            uncertain: false,
        };
        store.validate_state()?;
        Ok(store)
    }
    pub fn workspace_id(&self) -> Option<&str> {
        self.state.workspace_id.as_deref()
    }
    pub fn bind_workspace(&mut self, workspace_id: &str) -> KnowledgeResult<()> {
        self.ready()?;
        validate_text(workspace_id, 256, "workspace ID")?;
        match self.workspace_id() {
            Some(current) if current == workspace_id => Ok(()),
            Some(_) => Err(invalid("knowledge root belongs to another workspace")),
            None => {
                let mut next = self.state.clone();
                next.workspace_id = Some(workspace_id.into());
                self.commit(next)
            }
        }
    }
    pub fn list(&self) -> KnowledgeResult<Vec<KnowledgeRecord>> {
        self.ready()?;
        self.state
            .records
            .keys()
            .map(|id| self.read(id, None))
            .collect()
    }
    pub fn read(&self, id: &str, revision: Option<u64>) -> KnowledgeResult<KnowledgeRecord> {
        self.ready()?;
        validate_id(id)?;
        let versions = self
            .state
            .records
            .get(id)
            .ok_or_else(|| KnowledgeError::NotFound(id.into()))?;
        let version = match revision {
            Some(r) => versions.iter().find(|v| v.revision == r),
            None => versions.last(),
        }
        .ok_or_else(|| KnowledgeError::NotFound(format!("{id}@{revision:?}")))?;
        self.read_version(id, version)
    }
    pub fn export(&self, id: &str, revision: Option<u64>) -> KnowledgeResult<String> {
        markdown(&self.read(id, revision)?)
    }
    pub fn backlinks(&self, id: &str) -> KnowledgeResult<Vec<KnowledgeBacklink>> {
        validate_id(id)?;
        Ok(self
            .list()?
            .into_iter()
            .flat_map(|r| {
                r.links
                    .iter()
                    .filter(|link| link.target == id)
                    .map(|link| KnowledgeBacklink {
                        id: r.id.clone(),
                        title: r.title.clone(),
                        revision: r.revision,
                        kind: link.kind,
                    })
                    .collect::<Vec<_>>()
            })
            .collect())
    }
    pub fn search(
        &self,
        query: &str,
        snapshot: &SnapshotSources,
        limit: usize,
        max_bytes: usize,
    ) -> KnowledgeResult<Vec<KnowledgeSearchHit>> {
        if query.len() > 1024 || limit > 50 || max_bytes > 64 * 1024 {
            return Err(KnowledgeError::Capacity);
        }
        let query = query.trim().to_lowercase();
        let terms: BTreeSet<_> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|term| {
                term.len() > 1
                    && !matches!(
                        *term,
                        "the"
                            | "and"
                            | "for"
                            | "with"
                            | "this"
                            | "that"
                            | "from"
                            | "are"
                            | "was"
                            | "how"
                            | "should"
                            | "can"
                            | "does"
                            | "please"
                            | "explain"
                            | "what"
                            | "why"
                            | "our"
                            | "into"
                            | "about"
                            | "would"
                            | "have"
                            | "has"
                            | "not"
                            | "codebase"
                            | "repository"
                            | "of"
                            | "in"
                            | "on"
                            | "to"
                            | "an"
                            | "is"
                            | "it"
                            | "be"
                            | "as"
                            | "or"
                    )
            })
            .take(32)
            .map(|term| {
                if term.len() > 4 && term.ends_with("ies") {
                    format!("{}y", &term[..term.len() - 3])
                } else {
                    term.to_string()
                }
            })
            .collect();
        let mut hits = Vec::new();
        for record in self.list()? {
            let title = record.title.to_lowercase();
            let body = record.body.to_lowercase();
            let title_words: BTreeSet<_> = lexical_words(&title).collect();
            let body_words: BTreeSet<_> = lexical_words(&body).collect();
            let mut score: usize = terms
                .iter()
                .map(|term| {
                    let path_match = record.sources.iter().any(|s| {
                        s.path.to_lowercase().contains(term)
                            || s.symbol
                                .as_ref()
                                .is_some_and(|s| s.to_lowercase().contains(term))
                    });
                    usize::from(title_words.contains(term.as_str())) * 3
                        + usize::from(body_words.contains(term.as_str()))
                        + usize::from(path_match) * 2
                })
                .sum();
            if !query.is_empty() && (title.contains(&query) || body.contains(&query)) {
                score += 4
            }
            if !query.is_empty() && score == 0 {
                continue;
            }
            let excerpt = prefix(&record.body, 2048).to_string();
            hits.push(KnowledgeSearchHit {
                id: record.id.clone(),
                title: record.title.clone(),
                revision: record.revision,
                kind: record.kind,
                truncated: excerpt.len() < record.body.len(),
                excerpt,
                score,
                sources: record.applicability(snapshot),
                provenance: record.provenance,
            });
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        let mut result = Vec::new();
        for hit in hits {
            if result.len() >= limit {
                break;
            }
            result.push(hit);
            if serde_json::to_vec(&result)?.len() > max_bytes {
                result.pop();
                break;
            }
        }
        Ok(result)
    }
    /// Explicit human edits. Agent-originated changes must use propose/publish.
    pub fn save(
        &mut self,
        note: KnowledgeDraft,
        expected_revision: u64,
    ) -> KnowledgeResult<KnowledgeRecord> {
        if !matches!(note.provenance, KnowledgeProvenance::Human { .. }) {
            return Err(invalid("agent knowledge requires a staged proposal"));
        }
        self.write_record(note, expected_revision, KnowledgeAcceptance::Human, None)
    }
    pub fn proposals(&self) -> KnowledgeResult<Vec<KnowledgeProposal>> {
        self.ready()?;
        Ok(self.state.proposals.values().cloned().collect())
    }
    pub fn proposal(&self, id: &str) -> KnowledgeResult<KnowledgeProposal> {
        self.ready()?;
        self.state
            .proposals
            .get(id)
            .cloned()
            .ok_or_else(|| KnowledgeError::NotFound(id.into()))
    }
    /// Whether `propose` would stage this note, without staging it: the same
    /// validation, the publishable size, the store's capacity and the
    /// proposal identity. A note that could never be published is `Invalid`;
    /// a full store is `Capacity`.
    pub fn check_proposal(
        &self,
        note: &KnowledgeDraft,
        expected_revision: u64,
        activation: &ActivationRef,
        journal_id: &str,
    ) -> KnowledgeResult<()> {
        let id = self.proposal_checks(note, expected_revision, activation, journal_id)?;
        if let Some(previous) = self.state.proposals.get(&id) {
            if previous.note != *note {
                return Err(invalid(
                    "this activation already staged a different proposal for this note id",
                ));
            }
            return Ok(());
        }
        self.require_revision(&note.id, expected_revision)?;
        if self.state.proposals.len() >= MAX_PROPOSALS {
            return Err(KnowledgeError::Capacity);
        }
        Ok(())
    }
    fn proposal_checks(
        &self,
        note: &KnowledgeDraft,
        expected_revision: u64,
        activation: &ActivationRef,
        journal_id: &str,
    ) -> KnowledgeResult<String> {
        self.bound()?;
        validate_draft(note)?;
        validate_origin(&note.provenance, activation, journal_id)?;
        // Refuse now what could never be published: the rendered document,
        // with the largest acceptance record it could get, must fit.
        let prospective = KnowledgeRecord {
            id: note.id.clone(),
            title: note.title.clone(),
            body: note.body.clone(),
            kind: note.kind,
            revision: expected_revision
                .checked_add(1)
                .ok_or(KnowledgeError::Capacity)?,
            links: note.links.clone(),
            sources: note.sources.clone(),
            provenance: note.provenance.clone(),
            acceptance: KnowledgeAcceptance::AcceptedActivation {
                journal_id: journal_id.into(),
                activation: activation.clone(),
                closure_revision: u64::MAX,
            },
        };
        if markdown(&prospective)?.len() > DOCUMENT_LIMIT {
            return Err(invalid(
                "note is too large to publish; shorten its body or cite fewer files",
            ));
        }
        Ok(format!(
            "proposal-{}",
            digest(&serde_json::to_vec(&(
                journal_id,
                activation,
                &note.id,
                expected_revision
            ))?)
        ))
    }
    pub fn propose(
        &mut self,
        note: KnowledgeDraft,
        expected_revision: u64,
        activation: &ActivationRef,
        journal_id: &str,
    ) -> KnowledgeResult<KnowledgeProposal> {
        let id = self.proposal_checks(&note, expected_revision, activation, journal_id)?;
        if let Some(previous) = self.state.proposals.get(&id) {
            if previous.note != note {
                return Err(invalid("proposal identity already has different content"));
            }
            return Ok(previous.clone());
        }
        self.require_revision(&note.id, expected_revision)?;
        if self.state.proposals.len() >= MAX_PROPOSALS {
            return Err(KnowledgeError::Capacity);
        }
        let proposal = KnowledgeProposal {
            id: id.clone(),
            note,
            expected_revision,
            status: ProposalStatus::Pending,
            published_revision: None,
            journal_id: journal_id.into(),
            activation: activation.clone(),
        };
        let mut next = self.state.clone();
        next.proposals.insert(id, proposal.clone());
        self.commit(next)?;
        Ok(proposal)
    }
    pub fn reject(&mut self, id: &str) -> KnowledgeResult<KnowledgeProposal> {
        self.bound()?;
        let mut proposal = self.proposal(id)?;
        match proposal.status {
            ProposalStatus::Rejected => return Ok(proposal),
            ProposalStatus::Published => {
                return Err(invalid(
                    "published proposal cannot be rejected; supersede its note",
                ))
            }
            ProposalStatus::Pending => {}
        }
        proposal.status = ProposalStatus::Rejected;
        let mut next = self.state.clone();
        next.proposals.insert(id.into(), proposal.clone());
        self.commit(next)?;
        Ok(proposal)
    }
    /// Human acceptance retains the original provenance; it does not claim machine verification.
    pub fn accept_human(
        &mut self,
        id: &str,
        expected_revision: u64,
    ) -> KnowledgeResult<KnowledgeRecord> {
        self.bound()?;
        let proposal = self.proposal(id)?;
        if expected_revision != proposal.expected_revision {
            return Err(KnowledgeError::Conflict {
                expected: expected_revision,
                actual: proposal.expected_revision,
            });
        }
        self.publish_record(proposal, KnowledgeAcceptance::Human)
    }
    /// Only a persisted CLOSED canonical turn may publish agent proposals. A live
    /// accepted generation can still be revised, so a live snapshot is insufficient.
    pub fn publish(
        &mut self,
        id: &str,
        snapshot: &DurableTurnSnapshot,
    ) -> KnowledgeResult<KnowledgeRecord> {
        self.bound()?;
        let proposal = self.proposal(id)?;
        if self.workspace_id() != Some(snapshot.owner().workspace_id.as_str())
            || proposal.journal_id != snapshot.journal_id()
            || proposal.activation.session_id != snapshot.owner().session_id
            || proposal.activation.turn_id != *snapshot.turn_id()
            || !matches!(
                snapshot.contract().state(),
                Some(LogicalTurnState::Completed | LogicalTurnState::Finished)
            )
            || !snapshot
                .contract()
                .current_accepted_activations()
                .iter()
                .any(|a| a.activation == proposal.activation)
            || !snapshot
                .contract()
                .selected_for_finalization(&proposal.activation)
        {
            return Err(invalid(
                "proposal requires its exact accepted generation in a closed workspace turn",
            ));
        }
        let acceptance = KnowledgeAcceptance::AcceptedActivation {
            journal_id: proposal.journal_id.clone(),
            activation: proposal.activation.clone(),
            closure_revision: snapshot.contract().revision(),
        };
        self.publish_record(proposal, acceptance)
    }
    fn publish_record(
        &mut self,
        proposal: KnowledgeProposal,
        acceptance: KnowledgeAcceptance,
    ) -> KnowledgeResult<KnowledgeRecord> {
        match proposal.status {
            ProposalStatus::Rejected => Err(invalid("rejected proposal cannot be published")),
            ProposalStatus::Published => self.read(&proposal.note.id, proposal.published_revision),
            ProposalStatus::Pending => self.write_record(
                proposal.note,
                proposal.expected_revision,
                acceptance,
                Some(&proposal.id),
            ),
        }
    }
    fn write_record(
        &mut self,
        note: KnowledgeDraft,
        expected: u64,
        acceptance: KnowledgeAcceptance,
        proposal: Option<&str>,
    ) -> KnowledgeResult<KnowledgeRecord> {
        self.bound()?;
        validate_draft(&note)?;
        self.require_revision(&note.id, expected)?;
        let revision = expected.checked_add(1).ok_or(KnowledgeError::Capacity)?;
        let record = KnowledgeRecord {
            id: note.id,
            title: note.title,
            body: note.body,
            kind: note.kind,
            revision,
            links: note.links,
            sources: note.sources,
            provenance: note.provenance,
            acceptance,
        };
        let document = markdown(&record)?;
        if document.len() > DOCUMENT_LIMIT {
            return Err(KnowledgeError::Capacity);
        }
        let sha256 = digest(document.as_bytes());
        let file = format!("versions/{}-{:020}-{sha256}.md", record.id, revision);
        let mut next = self.state.clone();
        next.records
            .entry(record.id.clone())
            .or_default()
            .push(Version {
                revision,
                file: file.clone(),
                sha256,
                bytes: document.len(),
            });
        if let Some(id) = proposal {
            let p = next
                .proposals
                .get_mut(id)
                .ok_or_else(|| invalid("proposal disappeared"))?;
            p.status = ProposalStatus::Published;
            p.published_revision = Some(revision)
        }
        self.check_capacity(&next)?;
        // A document is durable before the manifest can expose it. Content-named
        // orphan documents can be reused after crash without overwriting history.
        if let Err(error) = self.write_immutable(&file, document.as_bytes()) {
            self.uncertain = true;
            return Err(error);
        }
        self.commit(next)?;
        Ok(record)
    }
    fn write_immutable(&self, path: &str, bytes: &[u8]) -> KnowledgeResult<()> {
        if self.root.is_file(path)? {
            if self.root.read_limited(path, DOCUMENT_LIMIT)? != bytes {
                return Err(invalid("immutable revision collision"));
            }
            self.root.sync_all()?;
            return Ok(());
        }
        self.root.atomic_write(path, bytes)?;
        Ok(())
    }
    fn read_version(&self, id: &str, version: &Version) -> KnowledgeResult<KnowledgeRecord> {
        let bytes = self.root.read_limited(&version.file, DOCUMENT_LIMIT)?;
        if bytes.len() != version.bytes || digest(&bytes) != version.sha256 {
            return Err(invalid(
                "immutable knowledge revision changed or is corrupt",
            ));
        }
        let record = parse_markdown(
            std::str::from_utf8(&bytes).map_err(|_| invalid("knowledge Markdown is not UTF-8"))?,
        )?;
        validate_draft(&record.draft())?;
        if record.id != id || record.revision != version.revision {
            return Err(invalid("knowledge revision identity mismatch"));
        }
        Ok(record)
    }
    fn validate_state(&self) -> KnowledgeResult<()> {
        if self.state.schema != 1 {
            return Err(invalid("unsupported knowledge schema"));
        }
        self.check_capacity(&self.state)?;
        if let Some(owner) = &self.state.workspace_id {
            validate_text(owner, 256, "workspace ID")?
        }
        if (!self.state.records.is_empty() || !self.state.proposals.is_empty())
            && self.state.workspace_id.is_none()
        {
            return Err(invalid("knowledge has no workspace owner"));
        }
        for (id, versions) in &self.state.records {
            validate_id(id)?;
            if versions.is_empty() {
                return Err(invalid("empty knowledge revision chain"));
            }
            for (i, v) in versions.iter().enumerate() {
                if v.revision != i as u64 + 1
                    || !valid_hash(&v.sha256)
                    || v.file != format!("versions/{}-{:020}-{}.md", id, v.revision, v.sha256)
                {
                    return Err(invalid("invalid knowledge revision reference"));
                }
                self.read_version(id, v)?;
            }
        }
        for (id, p) in &self.state.proposals {
            validate_draft(&p.note)?;
            validate_origin(&p.note.provenance, &p.activation, &p.journal_id)?;
            if id != &p.id
                || p.id
                    != format!(
                        "proposal-{}",
                        digest(&serde_json::to_vec(&(
                            &p.journal_id,
                            &p.activation,
                            &p.note.id,
                            p.expected_revision
                        ))?)
                    )
            {
                return Err(invalid("proposal identity mismatch"));
            }
            if p.status == ProposalStatus::Published {
                let rev = p
                    .published_revision
                    .ok_or_else(|| invalid("published proposal lacks revision"))?;
                if rev
                    != p.expected_revision
                        .checked_add(1)
                        .ok_or(KnowledgeError::Capacity)?
                    || self.read(&p.note.id, Some(rev))?.draft() != p.note
                {
                    return Err(invalid("published proposal revision mismatch"));
                }
            } else if p.published_revision.is_some() {
                return Err(invalid("unpublished proposal claims a revision"));
            }
        }
        Ok(())
    }
    fn require_revision(&self, id: &str, expected: u64) -> KnowledgeResult<()> {
        let actual = self
            .state
            .records
            .get(id)
            .and_then(|v| v.last())
            .map_or(0, |v| v.revision);
        if expected != actual {
            return Err(KnowledgeError::Conflict { expected, actual });
        }
        Ok(())
    }
    fn check_capacity(&self, state: &State) -> KnowledgeResult<()> {
        if state.records.len() > MAX_RECORDS
            || state.proposals.len() > MAX_PROPOSALS
            || state.records.values().map(Vec::len).sum::<usize>() > MAX_VERSIONS
            || serde_json::to_vec(state)?.len() > STATE_LIMIT
        {
            return Err(KnowledgeError::Capacity);
        }
        Ok(())
    }
    fn commit(&mut self, next: State) -> KnowledgeResult<()> {
        self.ready()?;
        self.check_capacity(&next)?;
        let bytes = serde_json::to_vec_pretty(&next)?;
        if bytes.len() > STATE_LIMIT {
            return Err(KnowledgeError::Capacity);
        }
        if let Err(e) = self.root.atomic_write(STATE_FILE, &bytes) {
            self.uncertain = true;
            return Err(e.into());
        }
        self.state = next;
        Ok(())
    }
    fn ready(&self) -> KnowledgeResult<()> {
        if self.uncertain {
            Err(KnowledgeError::RecoveryRequired)
        } else {
            Ok(())
        }
    }
    fn bound(&self) -> KnowledgeResult<()> {
        self.ready()?;
        if self.state.workspace_id.is_none() {
            Err(invalid("bind the workspace before knowledge mutation"))
        } else {
            Ok(())
        }
    }
}
pub fn knowledge_content_hash(content: &str) -> String {
    digest(content.as_bytes())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(message: &str) -> KnowledgeError {
    KnowledgeError::Invalid(message.into())
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_id(id: &str) -> KnowledgeResult<()> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid(
            "note ID must contain 1–100 ASCII letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}
fn validate_text(value: &str, max: usize, label: &str) -> KnowledgeResult<()> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        return Err(invalid(&format!("invalid {label}")));
    }
    Ok(())
}
fn validate_path(path: &str) -> KnowledgeResult<()> {
    if path.is_empty()
        || path.len() > 2048
        || path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(invalid("source path must be a normalized relative path"));
    }
    Ok(())
}
fn validate_draft(note: &KnowledgeDraft) -> KnowledgeResult<()> {
    validate_id(&note.id)?;
    validate_text(&note.title, 512, "title")?;
    if note.body.len() > 64 * 1024
        || note.body.contains('\0')
        || note.links.len() > 128
        || note.sources.len() > 128
    {
        return Err(KnowledgeError::Capacity);
    }
    let mut links = BTreeSet::new();
    for link in &note.links {
        validate_id(&link.target)?;
        if !links.insert(serde_json::to_string(link)?) {
            return Err(invalid("duplicate knowledge link"));
        }
    }
    let mut sources = BTreeSet::new();
    for source in &note.sources {
        validate_path(&source.path)?;
        if !valid_hash(&source.sha256) {
            return Err(invalid("invalid source SHA-256"));
        }
        if let Some(symbol) = &source.symbol {
            validate_text(symbol, 512, "symbol")?
        }
        if !sources.insert(serde_json::to_string(source)?) {
            return Err(invalid("duplicate source reference"));
        }
    }
    match &note.provenance {
        KnowledgeProvenance::Human {
            author: Some(author),
        } => validate_text(author, 256, "author")?,
        KnowledgeProvenance::Human { author: None } => {}
        KnowledgeProvenance::Model {
            journal_id,
            activation,
        }
        | KnowledgeProvenance::Observed {
            journal_id,
            activation,
            ..
        } => validate_origin(&note.provenance, activation, journal_id)?,
    }
    Ok(())
}
fn validate_origin(
    provenance: &KnowledgeProvenance,
    activation: &ActivationRef,
    journal_id: &str,
) -> KnowledgeResult<()> {
    validate_text(journal_id, 256, "journal identity")?;
    // Round-trip invokes the native identity validators, including bounded IDs.
    let _: ActivationRef = serde_json::from_value(serde_json::to_value(activation)?)?;
    let (journal, origin) = match provenance {
        KnowledgeProvenance::Model {
            journal_id,
            activation,
        } => (journal_id, activation),
        KnowledgeProvenance::Observed {
            journal_id,
            activation,
            evidence,
        } => {
            if evidence.is_empty() || evidence.len() > 128 {
                return Err(invalid(
                    "observed knowledge needs bounded evidence references",
                ));
            }
            for item in evidence {
                validate_text(item, 512, "evidence reference")?
            }
            (journal_id, activation)
        }
        KnowledgeProvenance::Human { .. } => {
            return Err(invalid(
                "agent proposal requires model or observed provenance",
            ))
        }
    };
    if journal != journal_id || origin != activation || activation.generation == 0 {
        return Err(invalid(
            "proposal provenance differs from its exact activation",
        ));
    }
    Ok(())
}
fn prefix(text: &str, max: usize) -> &str {
    let mut n = text.len().min(max);
    while !text.is_char_boundary(n) {
        n -= 1
    }
    &text[..n]
}
fn markdown(record: &KnowledgeRecord) -> KnowledgeResult<String> {
    let mut header = serde_json::to_value(record)?;
    header
        .as_object_mut()
        .expect("record object")
        .remove("body");
    Ok(format!(
        "---\n{}\n---\n\n{}",
        serde_json::to_string_pretty(&header)?,
        record.body
    ))
}
/// Read exported Markdown without trusting its acceptance or revision for writes.
/// Use the returned draft with save/propose and an explicit expected revision.
pub fn parse_knowledge_markdown(text: &str) -> KnowledgeResult<KnowledgeRecord> {
    parse_markdown(text)
}
fn parse_markdown(text: &str) -> KnowledgeResult<KnowledgeRecord> {
    if text.len() > DOCUMENT_LIMIT {
        return Err(KnowledgeError::Capacity);
    }
    let content = text
        .strip_prefix("---\n")
        .ok_or_else(|| invalid("missing knowledge front matter"))?;
    let (header, body) = content
        .split_once("\n---\n\n")
        .ok_or_else(|| invalid("missing knowledge body separator"))?;
    let mut record: KnowledgeRecord = serde_json::from_str(header)?;
    record.body = body.into();
    validate_draft(&record.draft())?;
    Ok(record)
}
fn lexical_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
#[path = "knowledge_tests.rs"]
mod tests;
