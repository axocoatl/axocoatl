//! Deterministic, turn-scoped coordination over an immutable agent graph.
//!
//! This scheduler is deliberately separate from the process-wide event feed
//! (`axocoatl_core::event_feed`); it models the causal execution of one
//! bounded multi-agent turn.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

/// The default bound permits one initial activation and one requested revision.
pub const DEFAULT_MAX_ACTIVATIONS_PER_AGENT: u32 = 2;

/// One immutable node in a turn's ordered agent graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAgentNode {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl TurnAgentNode {
    pub fn new(
        id: impl Into<String>,
        depends_on: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            depends_on: depends_on.into_iter().map(Into::into).collect(),
        }
    }
}

/// A validated agent graph whose declaration order is the scheduler's stable
/// ready order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnAgentGraph {
    nodes: Vec<TurnAgentNode>,
}

impl TurnAgentGraph {
    pub fn new(nodes: Vec<TurnAgentNode>) -> Result<Self, TurnCoordinationError> {
        if nodes.is_empty() {
            return Err(TurnCoordinationError::EmptyGraph);
        }

        let mut indices = HashMap::with_capacity(nodes.len());
        for (index, node) in nodes.iter().enumerate() {
            if node.id.trim().is_empty() {
                return Err(TurnCoordinationError::EmptyAgentId);
            }
            if indices.insert(node.id.clone(), index).is_some() {
                return Err(TurnCoordinationError::DuplicateAgent {
                    agent: node.id.clone(),
                });
            }
        }

        for node in &nodes {
            let mut dependencies = HashSet::with_capacity(node.depends_on.len());
            for dependency in &node.depends_on {
                if !indices.contains_key(dependency) {
                    return Err(TurnCoordinationError::UnknownDependency {
                        agent: node.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
                if !dependencies.insert(dependency) {
                    return Err(TurnCoordinationError::DuplicateDependency {
                        agent: node.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
        }

        let mut indegree: Vec<usize> = nodes.iter().map(|node| node.depends_on.len()).collect();
        let mut dependents = vec![Vec::new(); nodes.len()];
        for (node_index, node) in nodes.iter().enumerate() {
            for dependency in &node.depends_on {
                dependents[indices[dependency]].push(node_index);
            }
        }
        let mut ready: VecDeque<usize> = indegree
            .iter()
            .enumerate()
            .filter_map(|(index, degree)| (*degree == 0).then_some(index))
            .collect();
        let mut visited = 0;
        while let Some(index) = ready.pop_front() {
            visited += 1;
            for &dependent in &dependents[index] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 {
                    ready.push_back(dependent);
                }
            }
        }
        if visited != nodes.len() {
            let agents = indegree
                .iter()
                .enumerate()
                .filter(|(_, degree)| **degree > 0)
                .map(|(index, _)| nodes[index].id.clone())
                .collect();
            return Err(TurnCoordinationError::BaseCycle { agents });
        }

        Ok(Self { nodes })
    }

    pub fn nodes(&self) -> &[TurnAgentNode] {
        &self.nodes
    }

    pub fn node(&self, id: &str) -> Option<&TurnAgentNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.node(id).is_some()
    }

    /// Whether `possible_ancestor` appears in the transitive base dependencies
    /// of `agent`. Feedback edges never modify this immutable relation.
    pub fn is_ancestor(&self, possible_ancestor: &str, agent: &str) -> bool {
        let Some(agent) = self.node(agent) else {
            return false;
        };
        let mut pending = agent.depends_on.clone();
        let mut visited = HashSet::new();
        while let Some(candidate) = pending.pop() {
            if candidate == possible_ancestor {
                return true;
            }
            if visited.insert(candidate.clone()) {
                if let Some(node) = self.node(&candidate) {
                    pending.extend(node.depends_on.iter().cloned());
                }
            }
        }
        false
    }
}

impl<'de> Deserialize<'de> for TurnAgentGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedGraph {
            nodes: Vec<TurnAgentNode>,
        }

        let graph = SerializedGraph::deserialize(deserializer)?;
        Self::new(graph.nodes).map_err(serde::de::Error::custom)
    }
}

/// The complete runtime state of one agent in this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAgentState {
    Waiting,
    Running,
    Completed,
    Failed,
    Blocked,
    Cancelled,
}

/// Causal signal types understood by the turn scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnSignalKind {
    Completed,
    Failed,
    ChangesRequested,
}

/// One immutable causal fact emitted by an activated agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCausalSignal {
    pub id: String,
    pub kind: TurnSignalKind,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub generation: u32,
    pub summary: String,
}

impl TurnCausalSignal {
    pub fn completed(
        id: impl Into<String>,
        source: impl Into<String>,
        generation: u32,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: TurnSignalKind::Completed,
            source: source.into(),
            target: None,
            generation,
            summary: summary.into(),
        }
    }

    pub fn failed(
        id: impl Into<String>,
        source: impl Into<String>,
        generation: u32,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: TurnSignalKind::Failed,
            source: source.into(),
            target: None,
            generation,
            summary: summary.into(),
        }
    }

    pub fn changes_requested(
        id: impl Into<String>,
        source: impl Into<String>,
        target: impl Into<String>,
        generation: u32,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: TurnSignalKind::ChangesRequested,
            source: source.into(),
            target: Some(target.into()),
            generation,
            summary: summary.into(),
        }
    }
}

/// A pending verification edge introduced by an accepted feedback signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRevisionWait {
    pub target: String,
    pub required_generation: u32,
    pub signal_id: String,
}

#[derive(Debug, Clone)]
struct AgentRuntime {
    state: TurnAgentState,
    activation_count: u32,
    revision_wait: Option<TurnRevisionWait>,
    feedback_signal_id: Option<String>,
}

/// The complete causal input for one ready activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnActivation {
    pub agent_id: String,
    pub generation: u32,
    pub inputs: Vec<TurnCausalSignal>,
}

/// Serializable per-agent state, ordered exactly like the immutable graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAgentSnapshot {
    pub id: String,
    pub depends_on: Vec<String>,
    pub state: TurnAgentState,
    /// The most recently started generation. Zero means never activated.
    pub generation: u32,
    pub activation_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for_revision: Option<TurnRevisionWait>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_feedback_signal_id: Option<String>,
}

/// Durable event kinds suitable for a Session ledger or live UI fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCoordinationEventKind {
    AgentStarted,
    AgentCompleted,
    AgentFailed,
    AgentBlocked,
    ChangesRequested,
    AgentReactivated,
    AgentCancelled,
}

/// One ordered state transition. Signal-bearing events embed the causal signal
/// so an append-only ledger does not need an out-of-band lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCoordinationEvent {
    pub sequence: u64,
    pub kind: TurnCoordinationEventKind,
    pub agent_id: String,
    pub generation: u32,
    pub state: TurnAgentState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cause_signal_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<TurnCausalSignal>,
}

/// Full ordered state for persistence, API responses, and UI reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCoordinationSnapshot {
    pub graph: TurnAgentGraph,
    pub agents: Vec<TurnAgentSnapshot>,
    pub ready: Vec<TurnActivation>,
    pub signals: Vec<TurnCausalSignal>,
    pub events: Vec<TurnCoordinationEvent>,
    pub max_activations_per_agent: u32,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TurnCoordinationError {
    #[error("turn coordination graph must contain at least one agent")]
    EmptyGraph,
    #[error("agent id must not be empty")]
    EmptyAgentId,
    #[error("agent '{agent}' appears more than once")]
    DuplicateAgent { agent: String },
    #[error("agent '{agent}' names unknown dependency '{dependency}'")]
    UnknownDependency { agent: String, dependency: String },
    #[error("agent '{agent}' names dependency '{dependency}' more than once")]
    DuplicateDependency { agent: String, dependency: String },
    #[error("base dependency graph contains a cycle involving {agents:?}")]
    BaseCycle { agents: Vec<String> },
    #[error("max activations per agent must be at least one")]
    InvalidActivationLimit,
    #[error("unknown agent '{agent}'")]
    UnknownAgent { agent: String },
    #[error("signal id must not be empty")]
    EmptySignalId,
    #[error("signal '{signal_id}' has already been accepted")]
    DuplicateSignal { signal_id: String },
    #[error("{kind:?} signal from '{signal_source}' must not have a target")]
    UnexpectedSignalTarget {
        kind: TurnSignalKind,
        signal_source: String,
    },
    #[error("changes_requested signal from '{signal_source}' must name a target")]
    MissingSignalTarget { signal_source: String },
    #[error(
        "signal from '{agent}' is for generation {actual}, but its running generation is {expected}"
    )]
    GenerationMismatch {
        agent: String,
        expected: u32,
        actual: u32,
    },
    #[error("cannot {action} agent '{agent}' while it is {state:?}")]
    InvalidState {
        agent: String,
        state: TurnAgentState,
        action: &'static str,
    },
    #[error("agent '{agent}' is waiting for unsatisfied dependencies")]
    AgentNotReady { agent: String },
    #[error("agent '{agent}' reached its activation limit of {max}")]
    ActivationLimit { agent: String, max: u32 },
    #[error("agent '{target}' is not an ancestor of feedback requester '{requester}'")]
    NonAncestorRevision { requester: String, target: String },
    #[error("revision target '{target}' must be completed, not {state:?}")]
    RevisionTargetNotCompleted {
        target: String,
        state: TurnAgentState,
    },
}

/// A deterministic scheduler for one Session turn.
///
/// It runs a route's Agents in the order of their exact named dependencies;
/// its "signals" are causal records, and nothing here decays or crosses a
/// threshold.
pub struct TurnCoordinationScheduler {
    graph: TurnAgentGraph,
    agents: Vec<AgentRuntime>,
    indices: HashMap<String, usize>,
    signals: Vec<TurnCausalSignal>,
    signal_ids: HashSet<String>,
    events: Vec<TurnCoordinationEvent>,
    max_activations_per_agent: u32,
    next_sequence: u64,
}

impl TurnCoordinationScheduler {
    pub fn new(graph: TurnAgentGraph) -> Self {
        // The constant is non-zero by construction.
        Self::with_max_activations(graph, DEFAULT_MAX_ACTIVATIONS_PER_AGENT)
            .expect("default activation limit must be valid")
    }

    pub fn with_max_activations(
        graph: TurnAgentGraph,
        max_activations_per_agent: u32,
    ) -> Result<Self, TurnCoordinationError> {
        if max_activations_per_agent == 0 {
            return Err(TurnCoordinationError::InvalidActivationLimit);
        }
        let indices = graph
            .nodes()
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index))
            .collect();
        let agents = graph
            .nodes()
            .iter()
            .map(|_| AgentRuntime {
                state: TurnAgentState::Waiting,
                activation_count: 0,
                revision_wait: None,
                feedback_signal_id: None,
            })
            .collect();
        Ok(Self {
            graph,
            agents,
            indices,
            signals: Vec::new(),
            signal_ids: HashSet::new(),
            events: Vec::new(),
            max_activations_per_agent,
            next_sequence: 1,
        })
    }

    pub fn graph(&self) -> &TurnAgentGraph {
        &self.graph
    }

    pub fn max_activations_per_agent(&self) -> u32 {
        self.max_activations_per_agent
    }

    pub fn state(&self, agent_id: &str) -> Result<TurnAgentState, TurnCoordinationError> {
        Ok(self.runtime(agent_id)?.state)
    }

    pub fn activation_count(&self, agent_id: &str) -> Result<u32, TurnCoordinationError> {
        Ok(self.runtime(agent_id)?.activation_count)
    }

    pub fn signals(&self) -> &[TurnCausalSignal] {
        &self.signals
    }

    pub fn events(&self) -> &[TurnCoordinationEvent] {
        &self.events
    }

    /// Ready activations are returned in the graph's immutable declaration
    /// order. Querying readiness has no side effects.
    pub fn ready_activations(&self) -> Vec<TurnActivation> {
        self.graph
            .nodes()
            .iter()
            .enumerate()
            .filter(|(index, _)| self.is_ready(*index))
            .map(|(index, node)| self.activation_for(index, node))
            .collect()
    }

    /// Transition one ready agent to Running and return its exact causal input.
    pub fn start(&mut self, agent_id: &str) -> Result<TurnActivation, TurnCoordinationError> {
        let index = self.index(agent_id)?;
        if self.agents[index].state != TurnAgentState::Waiting {
            return Err(TurnCoordinationError::InvalidState {
                agent: agent_id.to_string(),
                state: self.agents[index].state,
                action: "start",
            });
        }
        if self.agents[index].activation_count >= self.max_activations_per_agent {
            return Err(TurnCoordinationError::ActivationLimit {
                agent: agent_id.to_string(),
                max: self.max_activations_per_agent,
            });
        }
        if !self.is_ready(index) {
            return Err(TurnCoordinationError::AgentNotReady {
                agent: agent_id.to_string(),
            });
        }

        let activation = self.activation_for(index, &self.graph.nodes()[index]);
        self.agents[index].activation_count += 1;
        self.agents[index].state = TurnAgentState::Running;
        self.agents[index].revision_wait = None;
        self.agents[index].feedback_signal_id = None;
        self.push_event(
            TurnCoordinationEventKind::AgentStarted,
            agent_id,
            activation.generation,
            TurnAgentState::Running,
            activation
                .inputs
                .iter()
                .map(|signal| signal.id.clone())
                .collect(),
            None,
        );
        Ok(activation)
    }

    /// Apply a completion, failure, or bounded feedback signal.
    pub fn accept_signal(&mut self, signal: TurnCausalSignal) -> Result<(), TurnCoordinationError> {
        self.validate_signal_identity(&signal)?;
        let source_index = self.index(&signal.source)?;
        if let Some(target) = signal.target.as_deref() {
            self.index(target)?;
        }
        self.validate_running_generation(source_index, &signal)?;

        match signal.kind {
            TurnSignalKind::Completed => self.accept_terminal(
                source_index,
                signal,
                TurnAgentState::Completed,
                TurnCoordinationEventKind::AgentCompleted,
            ),
            TurnSignalKind::Failed => self.accept_terminal(
                source_index,
                signal,
                TurnAgentState::Failed,
                TurnCoordinationEventKind::AgentFailed,
            ),
            TurnSignalKind::ChangesRequested => self.accept_changes_requested(source_index, signal),
        }
    }

    pub fn complete(
        &mut self,
        agent_id: &str,
        signal_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<(), TurnCoordinationError> {
        let generation = self.runtime(agent_id)?.activation_count;
        self.accept_signal(TurnCausalSignal::completed(
            signal_id, agent_id, generation, summary,
        ))
    }

    pub fn fail(
        &mut self,
        agent_id: &str,
        signal_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<(), TurnCoordinationError> {
        let generation = self.runtime(agent_id)?.activation_count;
        self.accept_signal(TurnCausalSignal::failed(
            signal_id, agent_id, generation, summary,
        ))
    }

    pub fn request_changes(
        &mut self,
        requester: &str,
        target: &str,
        signal_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<(), TurnCoordinationError> {
        let generation = self.runtime(requester)?.activation_count;
        self.accept_signal(TurnCausalSignal::changes_requested(
            signal_id, requester, target, generation, summary,
        ))
    }

    /// Finalize a stopped turn. Already-terminal work remains truthful, while
    /// every running or never-run agent becomes explicitly Cancelled. Repeating
    /// cancellation is idempotent and emits no duplicate events.
    pub fn cancel(&mut self) -> usize {
        let cancelled: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter_map(|(index, runtime)| {
                matches!(
                    runtime.state,
                    TurnAgentState::Waiting | TurnAgentState::Running
                )
                .then_some(index)
            })
            .collect();
        for index in &cancelled {
            self.agents[*index].state = TurnAgentState::Cancelled;
            self.agents[*index].revision_wait = None;
            self.agents[*index].feedback_signal_id = None;
            let agent_id = self.graph.nodes()[*index].id.clone();
            self.push_event(
                TurnCoordinationEventKind::AgentCancelled,
                &agent_id,
                self.agents[*index].activation_count,
                TurnAgentState::Cancelled,
                Vec::new(),
                None,
            );
        }
        cancelled.len()
    }

    pub fn snapshot(&self) -> TurnCoordinationSnapshot {
        let agents = self
            .graph
            .nodes()
            .iter()
            .enumerate()
            .map(|(index, node)| TurnAgentSnapshot {
                id: node.id.clone(),
                depends_on: node.depends_on.clone(),
                state: self.agents[index].state,
                generation: self.agents[index].activation_count,
                activation_count: self.agents[index].activation_count,
                waiting_for_revision: self.agents[index].revision_wait.clone(),
                pending_feedback_signal_id: self.agents[index].feedback_signal_id.clone(),
            })
            .collect();
        TurnCoordinationSnapshot {
            graph: self.graph.clone(),
            agents,
            ready: self.ready_activations(),
            signals: self.signals.clone(),
            events: self.events.clone(),
            max_activations_per_agent: self.max_activations_per_agent,
        }
    }

    fn validate_signal_identity(
        &self,
        signal: &TurnCausalSignal,
    ) -> Result<(), TurnCoordinationError> {
        if signal.id.trim().is_empty() {
            return Err(TurnCoordinationError::EmptySignalId);
        }
        if self.signal_ids.contains(&signal.id) {
            return Err(TurnCoordinationError::DuplicateSignal {
                signal_id: signal.id.clone(),
            });
        }
        Ok(())
    }

    fn validate_running_generation(
        &self,
        source_index: usize,
        signal: &TurnCausalSignal,
    ) -> Result<(), TurnCoordinationError> {
        let runtime = &self.agents[source_index];
        if runtime.state != TurnAgentState::Running {
            return Err(TurnCoordinationError::InvalidState {
                agent: signal.source.clone(),
                state: runtime.state,
                action: "emit a signal for",
            });
        }
        if signal.generation != runtime.activation_count {
            return Err(TurnCoordinationError::GenerationMismatch {
                agent: signal.source.clone(),
                expected: runtime.activation_count,
                actual: signal.generation,
            });
        }
        Ok(())
    }

    fn accept_terminal(
        &mut self,
        source_index: usize,
        signal: TurnCausalSignal,
        state: TurnAgentState,
        event_kind: TurnCoordinationEventKind,
    ) -> Result<(), TurnCoordinationError> {
        if signal.target.is_some() {
            return Err(TurnCoordinationError::UnexpectedSignalTarget {
                kind: signal.kind,
                signal_source: signal.source,
            });
        }
        self.agents[source_index].state = state;
        self.record_signal(signal.clone());
        self.push_event(
            event_kind,
            &signal.source,
            signal.generation,
            state,
            Vec::new(),
            Some(signal.clone()),
        );
        if state == TurnAgentState::Failed {
            self.block_failed_descendants(&signal);
        }
        Ok(())
    }

    fn accept_changes_requested(
        &mut self,
        requester_index: usize,
        signal: TurnCausalSignal,
    ) -> Result<(), TurnCoordinationError> {
        let Some(target) = signal.target.clone() else {
            return Err(TurnCoordinationError::MissingSignalTarget {
                signal_source: signal.source,
            });
        };
        if !self.graph.is_ancestor(&target, &signal.source) {
            return Err(TurnCoordinationError::NonAncestorRevision {
                requester: signal.source,
                target,
            });
        }
        let target_index = self.index(&target)?;
        if self.agents[target_index].state != TurnAgentState::Completed {
            return Err(TurnCoordinationError::RevisionTargetNotCompleted {
                target,
                state: self.agents[target_index].state,
            });
        }
        // Reset every completed result causally derived from the revised target,
        // not only the path ending at the requester. Otherwise a completed
        // sibling could remain falsely current after the target advances.
        let affected = self.revision_affected_indices(target_index, requester_index);
        for &index in &affected {
            if self.agents[index].activation_count >= self.max_activations_per_agent {
                return Err(TurnCoordinationError::ActivationLimit {
                    agent: self.graph.nodes()[index].id.clone(),
                    max: self.max_activations_per_agent,
                });
            }
        }

        let required_generation = self.agents[target_index].activation_count + 1;
        for &index in &affected {
            self.agents[index].state = TurnAgentState::Waiting;
            self.agents[index].revision_wait = None;
            self.agents[index].feedback_signal_id = None;
        }
        self.agents[target_index].feedback_signal_id = Some(signal.id.clone());
        self.agents[requester_index].revision_wait = Some(TurnRevisionWait {
            target: target.clone(),
            required_generation,
            signal_id: signal.id.clone(),
        });
        self.record_signal(signal.clone());
        self.push_event(
            TurnCoordinationEventKind::ChangesRequested,
            &signal.source,
            signal.generation,
            TurnAgentState::Waiting,
            Vec::new(),
            Some(signal.clone()),
        );
        for index in affected {
            let agent_id = self.graph.nodes()[index].id.clone();
            self.push_event(
                TurnCoordinationEventKind::AgentReactivated,
                &agent_id,
                self.agents[index].activation_count + 1,
                TurnAgentState::Waiting,
                vec![signal.id.clone()],
                Some(signal.clone()),
            );
        }
        Ok(())
    }

    fn revision_affected_indices(&self, target_index: usize, requester_index: usize) -> Vec<usize> {
        let target = &self.graph.nodes()[target_index].id;
        self.graph
            .nodes()
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                (index == target_index
                    || index == requester_index
                    || (matches!(
                        self.agents[index].state,
                        TurnAgentState::Completed | TurnAgentState::Running
                    ) && self.graph.is_ancestor(target, &node.id)))
                .then_some(index)
            })
            .collect()
    }

    fn block_failed_descendants(&mut self, failure: &TurnCausalSignal) {
        loop {
            let mut blocked = Vec::new();
            for (index, node) in self.graph.nodes().iter().enumerate() {
                if self.agents[index].state != TurnAgentState::Waiting {
                    continue;
                }
                let failed_dependency = node.depends_on.iter().any(|dependency| {
                    let state = self.agents[self.indices[dependency]].state;
                    matches!(state, TurnAgentState::Failed | TurnAgentState::Blocked)
                });
                let failed_revision =
                    self.agents[index]
                        .revision_wait
                        .as_ref()
                        .is_some_and(|revision| {
                            let state = self.agents[self.indices[&revision.target]].state;
                            matches!(state, TurnAgentState::Failed | TurnAgentState::Blocked)
                        });
                if failed_dependency || failed_revision {
                    blocked.push(index);
                }
            }
            if blocked.is_empty() {
                break;
            }
            for index in blocked {
                self.agents[index].state = TurnAgentState::Blocked;
                let agent_id = self.graph.nodes()[index].id.clone();
                self.push_event(
                    TurnCoordinationEventKind::AgentBlocked,
                    &agent_id,
                    self.agents[index].activation_count,
                    TurnAgentState::Blocked,
                    vec![failure.id.clone()],
                    Some(failure.clone()),
                );
            }
        }
    }

    fn is_ready(&self, index: usize) -> bool {
        let runtime = &self.agents[index];
        if runtime.state != TurnAgentState::Waiting
            || runtime.activation_count >= self.max_activations_per_agent
        {
            return false;
        }
        let node = &self.graph.nodes()[index];
        let base_ready = node.depends_on.iter().all(|dependency| {
            self.agents[self.indices[dependency]].state == TurnAgentState::Completed
        });
        let revision_ready = runtime.revision_wait.as_ref().is_none_or(|revision| {
            let target = &self.agents[self.indices[&revision.target]];
            target.state == TurnAgentState::Completed
                && target.activation_count == revision.required_generation
        });
        base_ready && revision_ready
    }

    fn activation_for(&self, index: usize, node: &TurnAgentNode) -> TurnActivation {
        let runtime = &self.agents[index];
        let mut inputs = Vec::new();
        let mut input_ids = HashSet::new();

        // Exact all-of inputs contain one latest completion from each named
        // direct parent, in declaration order. Transitive ancestors and sibling
        // branch signals are intentionally excluded.
        for dependency in &node.depends_on {
            if let Some(signal) = self.latest_completion(dependency) {
                input_ids.insert(signal.id.clone());
                inputs.push(signal.clone());
            }
        }
        if let Some(feedback_signal_id) = &runtime.feedback_signal_id {
            if let Some(feedback) = self
                .signals
                .iter()
                .find(|signal| signal.id == *feedback_signal_id)
            {
                if input_ids.insert(feedback.id.clone()) {
                    inputs.push(feedback.clone());
                }
            }
        }
        if let Some(revision) = &runtime.revision_wait {
            // The feedback itself explains the retry. Revised work still flows
            // exclusively through direct-parent completion signals above.
            if let Some(request) = self
                .signals
                .iter()
                .find(|signal| signal.id == revision.signal_id)
            {
                if input_ids.insert(request.id.clone()) {
                    inputs.push(request.clone());
                }
            }
        }

        TurnActivation {
            agent_id: node.id.clone(),
            generation: runtime.activation_count + 1,
            inputs,
        }
    }

    fn latest_completion(&self, agent_id: &str) -> Option<&TurnCausalSignal> {
        let generation = self.runtime(agent_id).ok()?.activation_count;
        self.signals.iter().rev().find(|signal| {
            signal.source == agent_id
                && signal.kind == TurnSignalKind::Completed
                && signal.generation == generation
        })
    }

    fn record_signal(&mut self, signal: TurnCausalSignal) {
        self.signal_ids.insert(signal.id.clone());
        self.signals.push(signal);
    }

    fn push_event(
        &mut self,
        kind: TurnCoordinationEventKind,
        agent_id: &str,
        generation: u32,
        state: TurnAgentState,
        cause_signal_ids: Vec<String>,
        signal: Option<TurnCausalSignal>,
    ) {
        self.events.push(TurnCoordinationEvent {
            sequence: self.next_sequence,
            kind,
            agent_id: agent_id.to_string(),
            generation,
            state,
            cause_signal_ids,
            signal,
        });
        self.next_sequence += 1;
    }

    fn index(&self, agent_id: &str) -> Result<usize, TurnCoordinationError> {
        self.indices
            .get(agent_id)
            .copied()
            .ok_or_else(|| TurnCoordinationError::UnknownAgent {
                agent: agent_id.to_string(),
            })
    }

    fn runtime(&self, agent_id: &str) -> Result<&AgentRuntime, TurnCoordinationError> {
        Ok(&self.agents[self.index(agent_id)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, depends_on: &[&str]) -> TurnAgentNode {
        TurnAgentNode::new(id, depends_on.iter().copied())
    }

    fn graph(nodes: Vec<TurnAgentNode>) -> TurnAgentGraph {
        TurnAgentGraph::new(nodes).unwrap()
    }

    fn ids(ready: &[TurnActivation]) -> Vec<&str> {
        ready
            .iter()
            .map(|activation| activation.agent_id.as_str())
            .collect()
    }

    #[test]
    fn diamond_requires_each_direct_parent_once_without_double_counting_root() {
        let graph = graph(vec![
            node("root", &[]),
            node("left", &["root"]),
            node("right", &["root"]),
            node("join", &["left", "right"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);

        assert_eq!(ids(&scheduler.ready_activations()), vec!["root"]);
        scheduler.start("root").unwrap();
        scheduler.complete("root", "root-1", "root result").unwrap();
        assert_eq!(ids(&scheduler.ready_activations()), vec!["left", "right"]);

        scheduler.start("left").unwrap();
        scheduler.complete("left", "left-1", "left result").unwrap();
        assert_eq!(ids(&scheduler.ready_activations()), vec!["right"]);
        scheduler.start("right").unwrap();
        scheduler
            .complete("right", "right-1", "right result")
            .unwrap();

        let join = scheduler.ready_activations();
        assert_eq!(ids(&join), vec!["join"]);
        assert_eq!(
            join[0]
                .inputs
                .iter()
                .map(|signal| signal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["left-1", "right-1"]
        );
        assert_eq!(scheduler.activation_count("join").unwrap(), 0);
        scheduler.start("join").unwrap();
        assert_eq!(scheduler.activation_count("join").unwrap(), 1);
    }

    #[test]
    fn failed_dependency_blocks_all_waiting_descendants() {
        let graph = graph(vec![
            node("source", &[]),
            node("child", &["source"]),
            node("sink", &["child"]),
            node("independent", &[]),
            node("independent_child", &["independent"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);

        scheduler.start("source").unwrap();
        scheduler.fail("source", "source-failed", "boom").unwrap();

        assert_eq!(scheduler.state("source").unwrap(), TurnAgentState::Failed);
        assert_eq!(scheduler.state("child").unwrap(), TurnAgentState::Blocked);
        assert_eq!(scheduler.state("sink").unwrap(), TurnAgentState::Blocked);
        assert_eq!(
            scheduler.state("independent").unwrap(),
            TurnAgentState::Waiting
        );
        assert_eq!(
            scheduler.state("independent_child").unwrap(),
            TurnAgentState::Waiting
        );
        assert_eq!(ids(&scheduler.ready_activations()), vec!["independent"]);
        assert_eq!(
            scheduler
                .events()
                .iter()
                .filter(|event| event.kind == TurnCoordinationEventKind::AgentBlocked)
                .map(|event| event.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["child", "sink"]
        );
    }

    #[test]
    fn cancellation_finalizes_running_and_never_run_agents_distinctly() {
        let graph = graph(vec![
            node("finished", &[]),
            node("running", &[]),
            node("waiting", &["running"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("finished").unwrap();
        scheduler
            .complete("finished", "finished-1", "done")
            .unwrap();
        scheduler.start("running").unwrap();

        assert_eq!(scheduler.cancel(), 2);
        assert_eq!(
            scheduler.state("finished").unwrap(),
            TurnAgentState::Completed
        );
        assert_eq!(
            scheduler.state("running").unwrap(),
            TurnAgentState::Cancelled
        );
        assert_eq!(
            scheduler.state("waiting").unwrap(),
            TurnAgentState::Cancelled
        );
        assert!(scheduler.ready_activations().is_empty());
        assert_eq!(
            scheduler
                .events()
                .iter()
                .filter(|event| event.kind == TurnCoordinationEventKind::AgentCancelled)
                .map(|event| event.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["running", "waiting"]
        );
        assert_eq!(scheduler.cancel(), 0);
    }

    #[test]
    fn activation_inputs_are_only_named_direct_parents() {
        let graph = graph(vec![
            node("root", &[]),
            node("left", &["root"]),
            node("right", &["root"]),
            node("join", &["left", "right"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("root").unwrap();
        scheduler.complete("root", "root-output", "root").unwrap();
        scheduler.start("left").unwrap();
        scheduler.complete("left", "left-output", "left").unwrap();
        scheduler.start("right").unwrap();
        scheduler
            .complete("right", "right-output", "right")
            .unwrap();

        let activation = scheduler
            .ready_activations()
            .into_iter()
            .find(|activation| activation.agent_id == "join")
            .unwrap();
        assert_eq!(
            activation
                .inputs
                .iter()
                .map(|signal| signal.source.as_str())
                .collect::<Vec<_>>(),
            vec!["left", "right"]
        );
        assert!(!activation
            .inputs
            .iter()
            .any(|signal| signal.source == "root"));
    }

    #[test]
    fn bounded_revision_reactivates_ancestor_then_requester_for_verification() {
        let graph = graph(vec![node("author", &[]), node("reviewer", &["author"])]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);

        scheduler.start("author").unwrap();
        scheduler
            .complete("author", "author-v1", "first draft")
            .unwrap();
        scheduler.start("reviewer").unwrap();
        scheduler
            .request_changes("reviewer", "author", "needs-fix", "handle the edge case")
            .unwrap();

        assert_eq!(scheduler.state("author").unwrap(), TurnAgentState::Waiting);
        assert_eq!(
            scheduler.state("reviewer").unwrap(),
            TurnAgentState::Waiting
        );
        let retry = scheduler.ready_activations();
        assert_eq!(ids(&retry), vec!["author"]);
        assert_eq!(retry[0].generation, 2);
        assert_eq!(
            retry[0]
                .inputs
                .iter()
                .map(|signal| signal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["needs-fix"]
        );

        scheduler.start("author").unwrap();
        scheduler
            .complete("author", "author-v2", "fixed draft")
            .unwrap();
        let verify = scheduler.ready_activations();
        assert_eq!(ids(&verify), vec!["reviewer"]);
        assert_eq!(verify[0].generation, 2);
        assert_eq!(
            verify[0]
                .inputs
                .iter()
                .map(|signal| signal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["author-v2", "needs-fix"]
        );

        scheduler.start("reviewer").unwrap();
        scheduler
            .complete("reviewer", "review-v2", "verified")
            .unwrap();
        assert_eq!(scheduler.activation_count("author").unwrap(), 2);
        assert_eq!(scheduler.activation_count("reviewer").unwrap(), 2);
        assert!(scheduler.ready_activations().is_empty());
    }

    #[test]
    fn ancestor_revision_recomputes_the_full_path_using_direct_parent_inputs() {
        let graph = graph(vec![
            node("investigator", &[]),
            node("implementer", &["investigator"]),
            node("verifier", &["implementer"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("investigator").unwrap();
        scheduler
            .complete("investigator", "investigation-v1", "first finding")
            .unwrap();
        scheduler.start("implementer").unwrap();
        scheduler
            .complete("implementer", "implementation-v1", "first change")
            .unwrap();
        scheduler.start("verifier").unwrap();
        scheduler
            .request_changes(
                "verifier",
                "investigator",
                "incorrect-assumption",
                "recheck the premise",
            )
            .unwrap();

        let investigation_retry = scheduler.ready_activations();
        assert_eq!(ids(&investigation_retry), vec!["investigator"]);
        assert_eq!(
            investigation_retry[0]
                .inputs
                .iter()
                .map(|signal| signal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["incorrect-assumption"]
        );
        scheduler.start("investigator").unwrap();
        scheduler
            .complete("investigator", "investigation-v2", "correct finding")
            .unwrap();
        let implementer = scheduler.ready_activations();
        assert_eq!(ids(&implementer), vec!["implementer"]);
        assert_eq!(
            implementer[0]
                .inputs
                .iter()
                .map(|signal| signal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["investigation-v2"]
        );
        scheduler.start("implementer").unwrap();
        scheduler
            .complete("implementer", "implementation-v2", "correct change")
            .unwrap();

        let verifier = scheduler.ready_activations();
        assert_eq!(ids(&verifier), vec!["verifier"]);
        assert_eq!(
            verifier[0]
                .inputs
                .iter()
                .map(|signal| signal.id.as_str())
                .collect::<Vec<_>>(),
            vec!["implementation-v2", "incorrect-assumption"]
        );
        assert!(!verifier[0]
            .inputs
            .iter()
            .any(|signal| signal.id == "investigation-v2"));
    }

    #[test]
    fn ancestor_revision_invalidates_completed_sibling_descendants() {
        let graph = graph(vec![
            node("source", &[]),
            node("implementation", &["source"]),
            node("documentation", &["source"]),
            node("published_docs", &["documentation"]),
            node("reviewer", &["implementation"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("source").unwrap();
        scheduler.complete("source", "source-v1", "first").unwrap();
        scheduler.start("implementation").unwrap();
        scheduler
            .complete("implementation", "implementation-v1", "first")
            .unwrap();
        scheduler.start("documentation").unwrap();
        scheduler
            .complete("documentation", "documentation-v1", "first")
            .unwrap();
        scheduler.start("published_docs").unwrap();
        scheduler
            .complete("published_docs", "published-v1", "first")
            .unwrap();
        scheduler.start("reviewer").unwrap();

        scheduler
            .request_changes(
                "reviewer",
                "source",
                "source-needs-revision",
                "fix the source premise",
            )
            .unwrap();

        for agent in [
            "source",
            "implementation",
            "documentation",
            "published_docs",
            "reviewer",
        ] {
            assert_eq!(scheduler.state(agent).unwrap(), TurnAgentState::Waiting);
        }
        assert_eq!(ids(&scheduler.ready_activations()), vec!["source"]);

        scheduler.start("source").unwrap();
        scheduler.complete("source", "source-v2", "fixed").unwrap();
        assert_eq!(
            ids(&scheduler.ready_activations()),
            vec!["implementation", "documentation"]
        );
    }

    #[test]
    fn rejects_invalid_cycles_unknown_nodes_and_unbounded_feedback() {
        let cycle = TurnAgentGraph::new(vec![node("a", &["b"]), node("b", &["a"])]);
        assert!(matches!(
            cycle,
            Err(TurnCoordinationError::BaseCycle { .. })
        ));
        let unknown = TurnAgentGraph::new(vec![node("a", &["missing"])]);
        assert!(matches!(
            unknown,
            Err(TurnCoordinationError::UnknownDependency { .. })
        ));

        let graph = graph(vec![node("author", &[]), node("reviewer", &["author"])]);
        assert!(matches!(
            TurnCoordinationScheduler::with_max_activations(graph.clone(), 0),
            Err(TurnCoordinationError::InvalidActivationLimit)
        ));
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("author").unwrap();
        scheduler.complete("author", "a1", "one").unwrap();
        scheduler.start("reviewer").unwrap();
        scheduler
            .request_changes("reviewer", "author", "rev1", "again")
            .unwrap();
        let duplicate =
            TurnCausalSignal::changes_requested("rev1", "reviewer", "author", 1, "again");
        assert!(matches!(
            scheduler.accept_signal(duplicate),
            Err(TurnCoordinationError::DuplicateSignal { .. })
        ));
        scheduler.start("author").unwrap();
        scheduler.complete("author", "a2", "two").unwrap();
        scheduler.start("reviewer").unwrap();
        assert!(matches!(
            scheduler.request_changes("reviewer", "author", "rev2", "one more"),
            Err(TurnCoordinationError::ActivationLimit { agent, max: 2 }) if agent == "author"
        ));
    }

    #[test]
    fn rejects_feedback_to_a_non_ancestor() {
        let graph = graph(vec![
            node("source", &[]),
            node("reviewer", &["source"]),
            node("sibling", &["source"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("source").unwrap();
        scheduler.complete("source", "source-1", "done").unwrap();
        scheduler.start("reviewer").unwrap();

        assert!(matches!(
            scheduler.request_changes("reviewer", "sibling", "bad", "not an ancestor"),
            Err(TurnCoordinationError::NonAncestorRevision { requester, target })
                if requester == "reviewer" && target == "sibling"
        ));
    }

    #[test]
    fn rejects_revision_before_mutation_when_a_completed_descendant_is_at_limit() {
        let graph = graph(vec![
            node("source", &[]),
            node("implementation", &["source"]),
            node("reviewer", &["implementation"]),
        ]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("source").unwrap();
        scheduler.complete("source", "source-v1", "source").unwrap();
        scheduler.start("implementation").unwrap();
        scheduler
            .complete("implementation", "implementation-v1", "first")
            .unwrap();
        scheduler.start("reviewer").unwrap();
        scheduler
            .request_changes(
                "reviewer",
                "implementation",
                "implementation-fix",
                "revise implementation",
            )
            .unwrap();
        scheduler.start("implementation").unwrap();
        scheduler
            .complete("implementation", "implementation-v2", "second")
            .unwrap();
        scheduler.start("reviewer").unwrap();

        assert!(matches!(
            scheduler.request_changes(
                "reviewer",
                "source",
                "source-fix",
                "this would require a third implementation activation",
            ),
            Err(TurnCoordinationError::ActivationLimit { agent, max: 2 })
                if agent == "implementation"
        ));
        assert_eq!(
            scheduler.state("source").unwrap(),
            TurnAgentState::Completed
        );
        assert_eq!(
            scheduler.state("implementation").unwrap(),
            TurnAgentState::Completed
        );
        assert_eq!(
            scheduler.state("reviewer").unwrap(),
            TurnAgentState::Running
        );
        assert!(!scheduler
            .signals()
            .iter()
            .any(|signal| signal.id == "source-fix"));
    }

    #[test]
    fn snapshots_and_events_round_trip_through_json() {
        let graph = graph(vec![node("agent", &[])]);
        let mut scheduler = TurnCoordinationScheduler::new(graph);
        scheduler.start("agent").unwrap();
        scheduler.complete("agent", "done", "answer").unwrap();

        let snapshot = scheduler.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: TurnCoordinationSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, snapshot);
        assert_eq!(restored.events[0].sequence, 1);
        assert_eq!(restored.events[1].sequence, 2);
    }
}
