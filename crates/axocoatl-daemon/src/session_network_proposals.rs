//! Agents' requests for hosts, waiting for a person.
//!
//! Under `network: egress` a writer Agent that lists `request_network_access`
//! can ask for one exact host it was refused. The request becomes a
//! proposal: it is written to the Session's network record (`proposal`,
//! `state: pending`) and kept by the Session's decision point
//! ([`crate::session_egress::SessionEgress`]) until a person approves or
//! rejects it in the Network panel or through
//! `POST /api/sessions/{id}/network/proposals/{proposal_id}/approve|reject`.
//! Approval is the ordinary per-Session allow, recorded with the person as
//! actor and the proposal's id. Nothing approves a proposal by itself, and no
//! Agent has a path to the decision.
//!
//! [`ProposalBook`] is the decision point's list. It is rebuilt from the
//! record when the decision point opens, so a proposal still pending when the
//! daemon stopped can be decided after it starts again.

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use axocoatl_session::network_record::{
    is_proposal_id, NetworkEvent, NetworkLine, PolicySource, ProposalState, MAX_PROPOSAL_PORTS,
    MAX_PROPOSAL_REASON_BYTES,
};

use crate::session_egress::EgressPolicyError;

/// Ports a proposal names when the Agent gives none.
pub const DEFAULT_PROPOSAL_PORTS: [u16; 1] = [443];
/// How long `request_network_access` waits for a decision by default.
pub const DEFAULT_PROPOSAL_WAIT_SECS: u64 = 120;
/// The longest it may wait.
pub const MAX_PROPOSAL_WAIT_SECS: u64 = 600;
/// Proposals one Session may have waiting at once.
pub const MAX_PENDING_PROPOSALS: usize = 16;
/// Proposals one Session keeps in memory, decided ones included. The oldest
/// decided ones are dropped first; every one stays in the record.
pub const MAX_KEPT_PROPOSALS: usize = 256;

/// One proposal, as `GET /api/sessions/{id}/network` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalView {
    pub id: String,
    pub state: ProposalState,
    pub host: String,
    pub ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_id: Option<String>,
    /// The `session` policy revision its approval created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Who decided it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// `POST /api/sessions/{id}/network/proposals/{proposal_id}/approve` and
/// `/reject`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProposalDecisionRequest {
    /// The person's id for this decision; a resend with the same id after an
    /// approval is refused with 409.
    pub command_id: String,
}

/// The answer to an approval or rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkProposalDecided {
    pub proposal_id: String,
    pub state: ProposalState,
    /// The `session` policy's new revision and digest, for an approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// What an Agent asked for, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRequest {
    /// A normalized exact host name.
    pub host: String,
    /// Sorted, without duplicates.
    pub ports: Vec<u16>,
    pub reason: String,
    pub agent: String,
    pub invocation_id: String,
    pub activation_id: String,
}

/// A proposal the caller can wait on.
#[derive(Debug)]
pub struct Proposed {
    pub view: ProposalView,
    /// False when an identical pending proposal was joined.
    pub created: bool,
    pub outcome: watch::Receiver<ProposalState>,
}

/// Check the ports and the reason an Agent gave; the host is checked like
/// a person's per-Session allow.
pub fn proposal_ports(ports: Option<&[u16]>) -> Result<Vec<u16>, String> {
    let mut ports = axocoatl_config::egress::validate_ports(Some(
        ports.unwrap_or(DEFAULT_PROPOSAL_PORTS.as_slice()),
    ))?;
    if ports.len() > MAX_PROPOSAL_PORTS {
        return Err(format!(
            "a proposal names at most {MAX_PROPOSAL_PORTS} ports"
        ));
    }
    ports.sort_unstable();
    Ok(ports)
}

/// The reason, trimmed; 1 to [`MAX_PROPOSAL_REASON_BYTES`] bytes.
pub fn proposal_reason(reason: &str) -> Result<String, String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err("reason is required: say what the host is for".into());
    }
    if reason.len() > MAX_PROPOSAL_REASON_BYTES {
        return Err(format!(
            "reason must be at most {MAX_PROPOSAL_REASON_BYTES} bytes"
        ));
    }
    Ok(reason
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect())
}

/// A new proposal id: `prop_` and 8 random bytes in hex.
pub fn new_proposal_id() -> Result<String, String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("no randomness for a proposal id: {error}"))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("prop_{hex}"))
}

struct Entry {
    view: ProposalView,
    /// A person's decision is being recorded.
    deciding: bool,
    outcome: watch::Sender<ProposalState>,
}

impl Entry {
    fn new(view: ProposalView) -> Self {
        let (outcome, _) = watch::channel(view.state);
        Self {
            view,
            deciding: false,
            outcome,
        }
    }
}

/// A person's decision on one proposal while it is being recorded. Dropped
/// before [`Deciding::finish`], because the decision failed, panicked or
/// was cancelled, it puts the proposal back to pending, so it is never left
/// "being decided".
pub struct Deciding<'a> {
    book: &'a std::sync::Mutex<ProposalBook>,
    id: String,
    finished: bool,
}

impl<'a> Deciding<'a> {
    /// Start deciding a pending proposal: the guard, its host and its ports.
    pub fn begin(
        book: &'a std::sync::Mutex<ProposalBook>,
        id: &str,
    ) -> Result<(Self, String, Vec<u16>), EgressPolicyError> {
        let (host, ports) = book
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .begin_decision(id)?;
        Ok((
            Self {
                book,
                id: id.to_string(),
                finished: false,
            },
            host,
            ports,
        ))
    }

    /// The decision is recorded: keep it and wake whoever waits on it.
    pub fn finish(mut self, state: ProposalState, actor: &str, revision: Option<u64>) {
        self.book
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .finish(&self.id, state, actor, revision);
        self.finished = true;
    }
}

impl Drop for Deciding<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.book
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .abort_decision(&self.id);
        }
    }
}

/// One Session's proposals, oldest first.
#[derive(Default)]
pub struct ProposalBook {
    entries: Vec<Entry>,
}

impl std::fmt::Debug for ProposalBook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProposalBook")
            .field("proposals", &self.entries.len())
            .finish()
    }
}

impl ProposalBook {
    /// Rebuild the list from a Session's record, one line at a time, in
    /// order: each `proposal` line, and an approval's `policy` line, which
    /// decides its proposal even when the `proposal` line after it was not
    /// written. Lines of other kinds change nothing.
    pub fn apply(&mut self, line: &NetworkLine) {
        match &line.event {
            NetworkEvent::Proposal {
                id,
                state: ProposalState::Pending,
                host,
                ports,
                reason,
                agent,
                invocation_id,
                activation_id,
                ..
            } if is_proposal_id(id)
                && self.position(id).is_none()
                // Two calls that raced recorded the same request twice;
                // the first one kept is the one they waited on.
                && self.joinable(host, ports).is_none() =>
            {
                self.push(ProposalView {
                    id: id.clone(),
                    state: ProposalState::Pending,
                    host: host.clone(),
                    ports: ports.clone(),
                    reason: reason.clone(),
                    agent: agent.clone(),
                    invocation_id: invocation_id.clone(),
                    activation_id: activation_id.clone(),
                    revision: None,
                    actor: None,
                });
            }
            NetworkEvent::Proposal {
                id,
                state,
                actor,
                revision,
                ..
            } if *state != ProposalState::Pending => {
                self.settle(id, *state, actor.clone(), *revision);
            }
            NetworkEvent::Policy {
                source: PolicySource::SessionAllow,
                change: Some(change),
                revision,
                actor,
                ..
            } => {
                if let Some(id) = &change.proposal_id {
                    self.settle(id, ProposalState::Approved, actor.clone(), Some(*revision));
                }
            }
            _ => {}
        }
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.view.id == id)
    }

    fn push(&mut self, view: ProposalView) {
        self.entries.push(Entry::new(view));
        while self.entries.len() > MAX_KEPT_PROPOSALS {
            match self
                .entries
                .iter()
                .position(|entry| entry.view.state != ProposalState::Pending)
            {
                Some(oldest) => {
                    self.entries.remove(oldest);
                }
                None => break,
            }
        }
    }

    fn settle(
        &mut self,
        id: &str,
        state: ProposalState,
        actor: Option<String>,
        revision: Option<u64>,
    ) {
        if let Some(index) = self.position(id) {
            let entry = &mut self.entries[index];
            if entry.view.state == ProposalState::Pending {
                entry.view.state = state;
                entry.view.actor = actor;
                entry.view.revision = revision.or(entry.view.revision);
                entry.deciding = false;
                entry.outcome.send_replace(state);
            }
        }
    }

    /// Pending proposals.
    pub fn pending(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.view.state == ProposalState::Pending)
            .count()
    }

    /// The pending proposal for exactly this host and these ports, to join.
    pub fn joinable(
        &self,
        host: &str,
        ports: &[u16],
    ) -> Option<(ProposalView, watch::Receiver<ProposalState>)> {
        self.entries
            .iter()
            .find(|entry| {
                entry.view.state == ProposalState::Pending
                    && entry.view.host == host
                    && entry.view.ports == ports
            })
            .map(|entry| (entry.view.clone(), entry.outcome.subscribe()))
    }

    /// Add a pending proposal that is already in the record.
    pub fn insert(&mut self, view: ProposalView) -> watch::Receiver<ProposalState> {
        self.push(view.clone());
        self.entries
            .iter()
            .find(|entry| entry.view.id == view.id)
            .map(|entry| entry.outcome.subscribe())
            .expect("a proposal just added is kept")
    }

    /// Start a person's decision on a pending proposal: its host and ports.
    pub fn begin_decision(&mut self, id: &str) -> Result<(String, Vec<u16>), EgressPolicyError> {
        let index = self
            .position(id)
            .ok_or_else(|| EgressPolicyError::NotFound(format!("proposal '{id}' not found")))?;
        let entry = &mut self.entries[index];
        if entry.view.state != ProposalState::Pending {
            return Err(EgressPolicyError::Conflict(format!(
                "proposal {id} was already {}",
                entry.view.state.as_str()
            )));
        }
        if entry.deciding {
            return Err(EgressPolicyError::Conflict(format!(
                "proposal {id} is being decided"
            )));
        }
        entry.deciding = true;
        Ok((entry.view.host.clone(), entry.view.ports.clone()))
    }

    /// The decision could not be recorded: the proposal is pending again.
    pub fn abort_decision(&mut self, id: &str) {
        if let Some(index) = self.position(id) {
            self.entries[index].deciding = false;
        }
    }

    /// Record the outcome in memory and wake whoever waits on it.
    pub fn finish(
        &mut self,
        id: &str,
        state: ProposalState,
        actor: &str,
        revision: Option<u64>,
    ) -> Option<ProposalView> {
        self.settle(id, state, Some(actor.to_string()), revision);
        self.position(id)
            .map(|index| self.entries[index].view.clone())
    }

    /// Every proposal kept, pending ones first, then newest first.
    pub fn views(&self) -> Vec<ProposalView> {
        let mut pending: Vec<ProposalView> = Vec::new();
        let mut decided: Vec<ProposalView> = Vec::new();
        for entry in self.entries.iter().rev() {
            if entry.view.state == ProposalState::Pending {
                pending.push(entry.view.clone());
            } else {
                decided.push(entry.view.clone());
            }
        }
        pending.extend(decided);
        pending
    }

    /// One proposal, if kept.
    pub fn view(&self, id: &str) -> Option<ProposalView> {
        self.position(id)
            .map(|index| self.entries[index].view.clone())
    }

    /// How many calls wait on a proposal.
    pub fn waiting(&self, id: &str) -> usize {
        self.position(id)
            .map_or(0, |index| self.entries[index].outcome.receiver_count())
    }
}
