//! The observability stream bus.
//!
//! One broadcast channel carries the live session/run state the app observes,
//! plus retained lattice/workflow/chat compatibility frames. The daemon owns
//! the sender; each WebSocket connection subscribes.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axocoatl_coordination::{EventNotification, EventType};
use axocoatl_mcp::approval::ApprovalContext;

/// Why a run is blocked on a human, and what it needs to proceed.
#[derive(Debug, Clone, Serialize)]
pub struct AwaitingInput {
    /// Identifier to answer with.
    pub approval_id: String,
    /// A short human-readable statement of the question.
    pub question: String,
    /// Unix seconds — so a viewer can show how long it has been stuck.
    pub since: u64,
}

/// Split a scoped agent id into the run it belongs to.
///
/// Session agents are `{session}:{agent}` and variant lanes
/// `{session}#{index}:{agent}`, so the run key is everything before the final
/// colon in both cases.
pub fn run_of_scoped_agent(agent_id: &str) -> Option<String> {
    agent_id.rfind(':').map(|i| agent_id[..i].to_string())
}

/// One MCP permission decision that is still parked in the runtime gate.
///
/// Unlike [`AwaitingInput`], this is the complete decision payload. It also
/// represents approvals whose agent id is not scoped to a live run, so the
/// reconnect snapshot cannot be reconstructed from `RunState::awaiting`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PendingMcpApproval {
    pub approval_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    pub agent_id: String,
    pub server: String,
    pub tool: String,
    pub tool_display: String,
    pub arguments_preview: String,
    pub requested_at: u64,
}

impl From<ApprovalContext> for PendingMcpApproval {
    fn from(context: ApprovalContext) -> Self {
        let run = run_of_scoped_agent(&context.agent_id);
        Self {
            approval_id: context.approval_id,
            run,
            agent_id: context.agent_id,
            server: context.server,
            tool: context.tool,
            tool_display: context.tool_display,
            arguments_preview: context.arguments_preview,
            requested_at: context.requested_at,
        }
    }
}

/// A frame on the stream bus — serialized straight to the WebSocket as JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StreamFrame {
    /// A lattice coordination event (agent activation, completion, skill fire…).
    Event {
        #[serde(rename = "type")]
        event_type: String,
        agent: Option<String>,
        task: Option<String>,
        name: Option<String>,
        output: Option<String>,
        tokens: Option<u64>,
        workflow: Option<String>,
    },
    /// A streamed text chunk from a running agent.
    Token {
        workflow: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        delta: String,
    },
    /// A streamed reasoning / "thinking" chunk from a running agent.
    Reasoning {
        workflow: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        delta: String,
    },
    /// A tool call from a running agent. `phase` is `"start"` (carries
    /// `arguments`) or `"result"` (carries `result` + `is_error`). `workflow`
    /// holds the run id — a workflow id or a session id.
    ToolCall {
        workflow: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        call_id: String,
        /// Per-turn, per-agent occurrence of a provider-local call id. Some
        /// providers reuse identifiers such as `call_0` on later tool-loop
        /// iterations, so the raw id is display/correlation data, not a
        /// durable event identity by itself.
        #[serde(default)]
        occurrence: u64,
        name: String,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        is_error: bool,
    },
    /// A workflow run finished — broadcast so every connected client (incl.
    /// one that reconnected mid-run) sees the result.
    WorkflowDone {
        workflow: String,
        output: String,
        completed: Vec<String>,
        tokens: u64,
        #[serde(default)]
        token_usage_known: bool,
    },
    /// A workflow run failed. A structural error can follow paid Agent work,
    /// so the known subtotal and its completeness travel with the failure.
    WorkflowError {
        workflow: String,
        error: String,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        token_usage_known: bool,
    },
    /// A coordinator's plan for a run (Layer 2): the subtasks it decomposed the
    /// goal into and, for each, the capability+budget auction outcome. Emitted
    /// once, right after decompose + auction, before the workers run.
    CoordinatorPlan {
        workflow: String,
        coordinator: String,
        goal: String,
        subtasks: Vec<PlanSubtask>,
    },
    /// A directory-session run started.
    SessionStart {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// A durable session turn was accepted before execution started.
    SessionAccepted { session: String, turn_id: String },
    /// A directory-session run finished.
    SessionDone {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        token_usage_known: bool,
    },
    /// A session turn cooperatively stopped at a safe execution boundary.
    SessionCancelled {
        session: String,
        turn_id: String,
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        token_usage_known: bool,
    },
    /// A directory-session run failed.
    SessionError {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        error: String,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default)]
        reasoning_tokens: u64,
        #[serde(default)]
        token_usage_known: bool,
    },
    /// A Stop command could not be applied (for example the addressed turn
    /// completed just before the command arrived). This is a control response,
    /// never a terminal execution transition.
    SessionStopRejected {
        session: String,
        turn_id: String,
        error: String,
    },
    /// The exact Session runtime is about to be torn down and replaced.
    ///
    /// This is emitted only after the daemon owns every operation gate and has
    /// repeated its conflict checks, immediately before destructive teardown.
    /// Connected clients must suspend Files, Git, Preview, and task requests
    /// until the matching settled frame (or reconnect snapshot) resolves it.
    SessionEnvironmentChanging {
        session: String,
        /// Durable environment generation being invalidated.
        generation: u64,
    },
    /// A previously announced Session runtime change no longer owns teardown.
    ///
    /// The browser re-reads the canonical Session record on this edge because
    /// both success and failure can change its durable environment evidence.
    SessionEnvironmentSettled { session: String },
    /// Ways is about to take exclusive ownership of one Workspace and tear
    /// down every primary Session runtime anchored to it.
    WorkspaceAttemptChanging {
        workspace: String,
        session: String,
        attempt_set_id: String,
    },
    /// The exact attempt set no longer owns the Workspace primary runtime.
    WorkspaceAttemptSettled {
        workspace: String,
        attempt_set_id: String,
    },
    /// A variants lane started, announcing what it is.
    ///
    /// Every other frame for the lane is keyed `{session}#{index}` — an encoded
    /// convention that a client would otherwise have to parse, and which cannot
    /// say *which model* a lane runs. This is emitted once per lane so a viewer
    /// can build run-key → lane identity for itself and label live output
    /// correctly, rather than inferring from a string.
    LaneStarted {
        /// The run key subsequent frames carry — `{session}#{index}`.
        run: String,
        /// Durable identity of the attempt set that owns this lane. The legacy
        /// `run` key is reused by later sets, so clients must use this value to
        /// reject buffered frames from an older exploration.
        attempt_set_id: String,
        session: String,
        index: usize,
        /// Model this lane runs; `None` means the agent's configured default.
        model: Option<String>,
        agent: String,
    },
    /// A lane finished its check — the fan-in half, reported as it happens.
    ///
    /// Verification used to be a single blocking request that returned every
    /// verdict at once, so a roster could show nothing until all lanes were
    /// done. Emitting per lane lets survivors and eliminations appear as they
    /// resolve.
    LaneVerified {
        /// Durable identity of the attempt set whose evidence changed.
        attempt_set_id: String,
        session: String,
        index: usize,
        passed: bool,
        changed_files: usize,
        /// Test files this lane changed — a pass earned against tests the lane
        /// itself rewrote is not evidence.
        touched_tests: Vec<String>,
    },
    /// Sent once to a freshly-connected client — the state of every run
    /// currently in flight, so the dashboard can re-attach its live view.
    Snapshot {
        runs: Vec<RunState>,
        /// Complete, authoritative pending approvals from the MCP gate. This
        /// includes parallel approvals and approvals not associated with a
        /// scoped session/attempt run.
        approvals: Vec<PendingMcpApproval>,
        /// Runtime changes that owned destructive teardown at the snapshot
        /// cursor. Reconnect must retain this gate rather than infer readiness
        /// from a Session record written before teardown began.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        environment_transitions: Vec<SessionEnvironmentTransition>,
        /// Workspace-wide Ways owners. Every Session in one of these
        /// Workspaces must keep its primary runtime surfaces suspended.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attempt_ownerships: Vec<WorkspaceAttemptOwnership>,
    },
    /// An agent is about to call an MCP tool that has no recorded permission
    /// decision — the dashboard should prompt the user. Carries the data the
    /// user needs to decide: which agent, which server+tool, a preview of the
    /// arguments. Resolution comes back via `WsCommand::McpApprove`.
    McpApprovalRequired {
        approval_id: String,
        /// The run that is blocked, when the agent id identifies one — a lane
        /// (`{session}#{index}`) or a session. Derived here rather than leaving
        /// every client to parse the scoped-agent convention, and the reason a
        /// roster can say *which* lane is waiting rather than only that
        /// something is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
        agent_id: String,
        server: String,
        tool: String,
        tool_display: String,
        arguments_preview: String,
        requested_at: u64,
    },
    /// An approval was resolved (by this user or another tab). Lets every
    /// connected dashboard close the modal once a decision lands.
    McpApprovalResolved {
        approval_id: String,
        decision: String,
    },
}

/// One Session whose primary runtime is currently being replaced.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionEnvironmentTransition {
    pub session: String,
    /// Durable generation that was current when teardown ownership began.
    pub generation: u64,
}

/// One unresolved Ways set that exclusively owns a Workspace.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceAttemptOwnership {
    pub workspace: String,
    pub session: String,
    pub attempt_set_id: String,
}

/// One planned subtask in a coordinator run: what it is, which worker won the
/// capability+budget auction (with the runner-up bids), and whether it fell
/// back to an ad-hoc worker because no declared worker bid.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PlanSubtask {
    pub name: String,
    pub description: String,
    pub winner: String,
    pub score: f32,
    pub adhoc: bool,
    pub bids: Vec<PlanBid>,
}

/// One worker's bid on a subtask in the capability+budget auction.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PlanBid {
    pub worker: String,
    pub score: f32,
}

/// Live state of one agent within an in-flight run.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunAgent {
    pub agent: String,
    /// "running" | "done" | "error"
    pub status: String,
    pub output: String,
    pub thinking: String,
    pub tokens: u64,
}

/// Live state of one in-flight run (workflow, session, or attempt lane), rebuilt
/// purely from stream frames.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunState {
    /// What this run is blocked on, if anything.
    ///
    /// Blocked-on-human is a state of the run, not a notification that happened
    /// once: a client that connects late, or reloads, must still be able to see
    /// that a lane is waiting. Carrying it in the snapshot is what makes that
    /// true — the failure mode of parallel agents is not being unable to see
    /// them, it is not noticing one has stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<AwaitingInput>,
    /// The run id — a workflow id or a session id.
    pub workflow: String,
    /// Exact durable turn currently executing for a Session. Required for a
    /// reloaded client to address Stop safely instead of cancelling by the
    /// broader Session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// The durable attempt-set identity when this is an attempt lane. The
    /// legacy lane run key (`{session}#{index}`) is not globally unique across
    /// successive explorations, so snapshots must carry both values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_set_id: Option<String>,
    /// `"workflow"`, `"session"`, or `"attempt"` — lets the dashboard
    /// re-attach the right view.
    #[serde(default)]
    pub kind: String,
    pub agents: Vec<RunAgent>,
    /// Set when this run is a coordinator run — the coordinator agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<String>,
    /// The coordinator's goal (the run's input).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    /// The coordinator's decomposed subtasks + auction outcomes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtasks: Vec<PlanSubtask>,
}

#[derive(Debug, Clone)]
pub struct SequencedStreamFrame {
    pub sequence: u64,
    pub frame: StreamFrame,
}

#[derive(Default)]
struct StreamBusState {
    sequence: u64,
    runs: HashMap<String, RunState>,
    environment_transitions: HashMap<String, SessionEnvironmentTransition>,
    attempt_ownerships: HashMap<String, WorkspaceAttemptOwnership>,
}

struct StreamBusInner {
    sender: tokio::sync::broadcast::Sender<SequencedStreamFrame>,
    state: Mutex<StreamBusState>,
}

/// Ordered live-frame broker with an atomic reconnect cursor.
///
/// A raw broadcast subscription cannot tell whether a frame queued before a
/// reconnect snapshot was already folded into that snapshot. `StreamBus`
/// serializes sequence assignment, state folding, and publication under one
/// short lock. A subscriber can therefore discard queued frames at or below
/// the snapshot cursor without either duplicating or losing a delta.
#[derive(Clone)]
pub struct StreamBus {
    inner: Arc<StreamBusInner>,
}

impl StreamBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(capacity);
        Self {
            inner: Arc::new(StreamBusInner {
                sender,
                state: Mutex::new(StreamBusState::default()),
            }),
        }
    }

    // Preserve the broadcast sender's concrete error so callers can recover
    // the original frame; boxing it would change this public boundary.
    #[allow(clippy::result_large_err)]
    pub fn send(
        &self,
        frame: StreamFrame,
    ) -> Result<usize, tokio::sync::broadcast::error::SendError<StreamFrame>> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sequence = state
            .sequence
            .checked_add(1)
            .expect("stream sequence exhausted");
        apply_frame(&mut state.runs, &frame);
        match &frame {
            StreamFrame::SessionEnvironmentChanging {
                session,
                generation,
            } => {
                state.environment_transitions.insert(
                    session.clone(),
                    SessionEnvironmentTransition {
                        session: session.clone(),
                        generation: *generation,
                    },
                );
            }
            StreamFrame::SessionEnvironmentSettled { session } => {
                state.environment_transitions.remove(session);
            }
            StreamFrame::WorkspaceAttemptChanging {
                workspace,
                session,
                attempt_set_id,
            } => {
                state.attempt_ownerships.insert(
                    workspace.clone(),
                    WorkspaceAttemptOwnership {
                        workspace: workspace.clone(),
                        session: session.clone(),
                        attempt_set_id: attempt_set_id.clone(),
                    },
                );
            }
            StreamFrame::WorkspaceAttemptSettled {
                workspace,
                attempt_set_id,
            } if state
                .attempt_ownerships
                .get(workspace)
                .is_some_and(|owner| owner.attempt_set_id == *attempt_set_id) =>
            {
                state.attempt_ownerships.remove(workspace);
            }
            _ => {}
        }
        state.sequence = sequence;
        self.inner
            .sender
            .send(SequencedStreamFrame { sequence, frame })
            .map_err(|error| tokio::sync::broadcast::error::SendError(error.0.frame))
    }

    pub fn subscribe(&self) -> StreamSubscription {
        StreamSubscription {
            receiver: self.inner.sender.subscribe(),
        }
    }

    /// Return one exact state/cursor cut. Every foldable live-state effect at
    /// or below `cursor` is represented in `runs`; every later frame must be
    /// replayed by the subscriber. Session tool evidence is hydrated from the
    /// durable turn ledger rather than duplicated in this live projection.
    pub fn snapshot(&self) -> (u64, Vec<RunState>) {
        let (cursor, runs, _, _) = self.snapshot_with_runtime_ownership();
        (cursor, runs)
    }

    /// Return the exact run and Session-runtime ownership state at one cursor.
    pub fn snapshot_with_environment_transitions(
        &self,
    ) -> (u64, Vec<RunState>, Vec<SessionEnvironmentTransition>) {
        let (cursor, runs, transitions, _) = self.snapshot_with_runtime_ownership();
        (cursor, runs, transitions)
    }

    /// Return one exact cut of runs and every destructive runtime owner.
    pub fn snapshot_with_runtime_ownership(
        &self,
    ) -> (
        u64,
        Vec<RunState>,
        Vec<SessionEnvironmentTransition>,
        Vec<WorkspaceAttemptOwnership>,
    ) {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut transitions = state
            .environment_transitions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        transitions.sort_by(|left, right| left.session.cmp(&right.session));
        let mut attempt_ownerships = state
            .attempt_ownerships
            .values()
            .cloned()
            .collect::<Vec<_>>();
        attempt_ownerships.sort_by(|left, right| left.workspace.cmp(&right.workspace));
        (
            state.sequence,
            state.runs.values().cloned().collect(),
            transitions,
            attempt_ownerships,
        )
    }

    pub fn run(&self, id: &str) -> Option<RunState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs
            .get(id)
            .cloned()
    }

    pub fn contains_run(&self, id: &str) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs
            .contains_key(id)
    }

    pub fn remove_run(&self, id: &str) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runs
            .remove(id);
    }
}

/// Drop-backed lifecycle owner so cancellation and early errors always settle
/// the broadcast gate after a destructive environment transition was announced.
pub struct SessionEnvironmentChangeGuard {
    bus: StreamBus,
    session: String,
}

/// Drop-backed owner for the pre-persistence Ways teardown window. Once the
/// durable current-set pointer exists, `retain` transfers settlement to exact
/// attempt cleanup.
pub struct WorkspaceAttemptChangeGuard {
    bus: StreamBus,
    workspace: String,
    attempt_set_id: String,
    settle_on_drop: bool,
}

impl WorkspaceAttemptChangeGuard {
    pub fn retain(mut self) {
        self.settle_on_drop = false;
    }
}

impl Drop for WorkspaceAttemptChangeGuard {
    fn drop(&mut self) {
        if self.settle_on_drop {
            let _ = self.bus.send(StreamFrame::WorkspaceAttemptSettled {
                workspace: self.workspace.clone(),
                attempt_set_id: self.attempt_set_id.clone(),
            });
        }
    }
}

impl Drop for SessionEnvironmentChangeGuard {
    fn drop(&mut self) {
        let _ = self.bus.send(StreamFrame::SessionEnvironmentSettled {
            session: self.session.clone(),
        });
    }
}

impl StreamBus {
    /// Announce destructive Session-runtime ownership until the returned guard
    /// leaves scope, including when the owning future is cancelled.
    pub fn begin_session_environment_change(
        &self,
        session: impl Into<String>,
        generation: u64,
    ) -> SessionEnvironmentChangeGuard {
        let session = session.into();
        let _ = self.send(StreamFrame::SessionEnvironmentChanging {
            session: session.clone(),
            generation,
        });
        SessionEnvironmentChangeGuard {
            bus: self.clone(),
            session,
        }
    }

    /// Announce Workspace-wide primary-runtime suspension before Ways begins
    /// destructive quiescence.
    pub fn begin_workspace_attempt_change(
        &self,
        workspace: impl Into<String>,
        session: impl Into<String>,
        attempt_set_id: impl Into<String>,
    ) -> WorkspaceAttemptChangeGuard {
        let workspace = workspace.into();
        let session = session.into();
        let attempt_set_id = attempt_set_id.into();
        let _ = self.send(StreamFrame::WorkspaceAttemptChanging {
            workspace: workspace.clone(),
            session,
            attempt_set_id: attempt_set_id.clone(),
        });
        WorkspaceAttemptChangeGuard {
            bus: self.clone(),
            workspace,
            attempt_set_id,
            settle_on_drop: true,
        }
    }
}

pub struct StreamSubscription {
    receiver: tokio::sync::broadcast::Receiver<SequencedStreamFrame>,
}

impl StreamSubscription {
    /// Compatibility receive path for consumers that do not create reconnect
    /// snapshots and therefore do not need the cursor.
    pub async fn recv(&mut self) -> Result<StreamFrame, tokio::sync::broadcast::error::RecvError> {
        self.receiver.recv().await.map(|envelope| envelope.frame)
    }

    pub fn try_recv(&mut self) -> Result<StreamFrame, tokio::sync::broadcast::error::TryRecvError> {
        self.receiver.try_recv().map(|envelope| envelope.frame)
    }

    pub async fn recv_sequenced(
        &mut self,
    ) -> Result<SequencedStreamFrame, tokio::sync::broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

/// Assign an unambiguous occurrence to start/result pairs in one agent
/// stream. Results consume starts FIFO so even duplicate provider ids cannot
/// overwrite earlier durable Route evidence.
#[derive(Debug, Default)]
pub struct ToolCallOccurrences {
    next: u64,
    active: HashMap<String, VecDeque<u64>>,
}

impl ToolCallOccurrences {
    pub fn start(&mut self, call_id: &str) -> u64 {
        let occurrence = self.next;
        self.next = self.next.saturating_add(1);
        self.active
            .entry(call_id.to_string())
            .or_default()
            .push_back(occurrence);
        occurrence
    }

    pub fn finish(&mut self, call_id: &str) -> u64 {
        if let Some(occurrence) = self.active.get_mut(call_id).and_then(VecDeque::pop_front) {
            if self.active.get(call_id).is_some_and(VecDeque::is_empty) {
                self.active.remove(call_id);
            }
            return occurrence;
        }
        // A provider adapter should emit starts first, but preserving an
        // orphan result under a fresh identity is safer than colliding with an
        // earlier completed call.
        let occurrence = self.next;
        self.next = self.next.saturating_add(1);
        occurrence
    }
}

impl RunState {
    fn agent_mut(&mut self, name: &str) -> &mut RunAgent {
        if let Some(i) = self.agents.iter().position(|a| a.agent == name) {
            return &mut self.agents[i];
        }
        self.agents.push(RunAgent {
            agent: name.to_string(),
            status: "running".to_string(),
            ..Default::default()
        });
        self.agents.last_mut().unwrap()
    }
}

/// Fold one frame into the in-flight run registry synchronously inside
/// [`StreamBus::send`]'s ordered state/publication lock.
pub fn apply_frame(runs: &mut std::collections::HashMap<String, RunState>, frame: &StreamFrame) {
    fn run_for<'a>(
        runs: &'a mut std::collections::HashMap<String, RunState>,
        wf: &str,
    ) -> &'a mut RunState {
        runs.entry(wf.to_string()).or_insert_with(|| RunState {
            workflow: wf.to_string(),
            kind: "workflow".to_string(),
            agents: Vec::new(),
            ..Default::default()
        })
    }
    match frame {
        // Blocked-on-human is folded into run state, not merely announced, so a
        // client that connects after the prompt fired still sees the lane is
        // waiting rather than watching a row that appears idle forever.
        StreamFrame::McpApprovalRequired {
            approval_id,
            run: Some(key),
            tool_display,
            server,
            requested_at,
            ..
        } => {
            run_for(runs, key).awaiting = Some(AwaitingInput {
                approval_id: approval_id.clone(),
                question: format!("approve {tool_display} on {server}?"),
                since: *requested_at,
            });
        }
        StreamFrame::McpApprovalResolved { approval_id, .. } => {
            // Clear whichever run was parked on this approval.
            for r in runs.values_mut() {
                if r.awaiting
                    .as_ref()
                    .is_some_and(|a| &a.approval_id == approval_id)
                {
                    r.awaiting = None;
                }
            }
        }
        StreamFrame::Event {
            event_type,
            agent: Some(agent),
            workflow: Some(wf),
            tokens,
            output,
            ..
        } => match event_type.as_str() {
            "AgentActivated" => {
                let a = run_for(runs, wf).agent_mut(agent);
                a.status = "running".to_string();
                a.output.clear();
                a.thinking.clear();
                a.tokens = 0;
            }
            "TaskCompleted" => {
                let a = run_for(runs, wf).agent_mut(agent);
                a.status = "done".to_string();
                if let Some(t) = tokens {
                    a.tokens = *t;
                }
                if let Some(o) = output {
                    // Multi-Agent completion events carry a bounded preview.
                    // Never replace the full streamed projection with that
                    // shorter terminal summary during reconnect tracking.
                    if o.len() >= a.output.len() {
                        a.output = o.clone();
                    }
                }
            }
            "AgentFailed" => {
                let a = run_for(runs, wf).agent_mut(agent);
                a.status = "error".to_string();
                if let Some(o) = output {
                    a.output = o.clone();
                }
            }
            "AgentCancelled" => {
                let a = run_for(runs, wf).agent_mut(agent);
                a.status = "cancelled".to_string();
                if let Some(t) = tokens {
                    a.tokens = *t;
                }
                if let Some(o) = output {
                    a.output = o.clone();
                }
            }
            "AgentPanicked" => {
                let a = run_for(runs, wf).agent_mut(agent);
                a.status = "error".to_string();
                if let Some(o) = output {
                    a.output = o.clone();
                }
            }
            _ => {}
        },
        StreamFrame::Token {
            workflow,
            agent,
            turn_id,
            delta,
        } => {
            if turn_id.is_some() && !runs.contains_key(workflow) {
                return;
            }
            let run = run_for(runs, workflow);
            if turn_id
                .as_ref()
                .is_some_and(|turn_id| run.turn_id.as_ref() != Some(turn_id))
            {
                return;
            }
            run.agent_mut(agent).output.push_str(delta);
        }
        StreamFrame::Reasoning {
            workflow,
            agent,
            turn_id,
            delta,
        } => {
            if turn_id.is_some() && !runs.contains_key(workflow) {
                return;
            }
            let run = run_for(runs, workflow);
            if turn_id
                .as_ref()
                .is_some_and(|turn_id| run.turn_id.as_ref() != Some(turn_id))
            {
                return;
            }
            run.agent_mut(agent).thinking.push_str(delta);
        }
        StreamFrame::ToolCall {
            workflow, turn_id, ..
        } if turn_id.is_some()
            && (!runs.contains_key(workflow)
                || turn_id.as_ref().is_some_and(|turn_id| {
                    runs.get(workflow).and_then(|run| run.turn_id.as_ref()) != Some(turn_id)
                })) =>
        {
            // Ignore stale turn-scoped tool frames after terminal cleanup.
        }
        StreamFrame::WorkflowDone { workflow, .. }
        | StreamFrame::WorkflowError { workflow, .. } => {
            runs.remove(workflow);
        }
        StreamFrame::SessionStart { session, turn_id } => {
            if turn_id.as_ref().is_some_and(|turn_id| {
                runs.get(session)
                    .and_then(|run| run.turn_id.as_ref())
                    .is_some_and(|current| current != turn_id)
            }) {
                runs.remove(session);
            }
            let state = runs.entry(session.clone()).or_insert_with(|| RunState {
                workflow: session.clone(),
                kind: "session".to_string(),
                agents: Vec::new(),
                ..Default::default()
            });
            if turn_id.is_some() {
                state.turn_id = turn_id.clone();
            }
        }
        StreamFrame::SessionAccepted { session, turn_id } => {
            if runs
                .get(session)
                .and_then(|run| run.turn_id.as_ref())
                .is_some_and(|current| current != turn_id)
            {
                runs.remove(session);
            }
            let state = runs.entry(session.clone()).or_insert_with(|| RunState {
                workflow: session.clone(),
                kind: "session".to_string(),
                agents: Vec::new(),
                ..Default::default()
            });
            state.turn_id = Some(turn_id.clone());
        }
        #[allow(clippy::collapsible_match)]
        StreamFrame::SessionDone {
            session, turn_id, ..
        }
        | StreamFrame::SessionError {
            session, turn_id, ..
        } => {
            if (turn_id.is_none()
                && runs
                    .get(session)
                    .and_then(|run| run.turn_id.as_ref())
                    .is_none())
                || (turn_id.is_some()
                    && runs.get(session).and_then(|run| run.turn_id.as_ref()) == turn_id.as_ref())
            {
                runs.remove(session);
            }
        }
        #[allow(clippy::collapsible_match)]
        StreamFrame::SessionCancelled {
            session, turn_id, ..
        } => {
            if runs.get(session).and_then(|run| run.turn_id.as_ref()) == Some(turn_id) {
                runs.remove(session);
            }
        }
        StreamFrame::LaneStarted {
            run,
            attempt_set_id,
            ..
        } => {
            let state = run_for(runs, run);
            state.kind = "attempt".to_string();
            state.attempt_set_id = Some(attempt_set_id.clone());
        }
        StreamFrame::LaneVerified {
            attempt_set_id,
            session,
            index,
            ..
        } => {
            // Verification normally happens after the live run has been
            // removed. Do not resurrect a completed lane just to snapshot its
            // verdict, but preserve set identity when a live state remains.
            let run = format!("{session}#{index}");
            if let Some(state) = runs.get_mut(&run) {
                state.kind = "attempt".to_string();
                state.attempt_set_id = Some(attempt_set_id.clone());
            }
        }
        StreamFrame::CoordinatorPlan {
            workflow,
            coordinator,
            goal,
            subtasks,
        } => {
            let run = run_for(runs, workflow);
            run.coordinator = Some(coordinator.clone());
            run.goal = goal.clone();
            run.subtasks = subtasks.clone();
        }
        _ => {}
    }
}

/// Flatten a lattice notification into an `Event` frame. This is the single
/// place lattice `EventType`s are mapped to the WebSocket wire shape.
pub fn event_frame(notif: &EventNotification) -> StreamFrame {
    let (kind, mut agent, task, name) = match &notif.event_type {
        EventType::TaskAvailable { task_type } => {
            ("TaskAvailable", None, Some(task_type.clone()), None)
        }
        EventType::TaskCompleted { task_id } => {
            ("TaskCompleted", None, Some(task_id.clone()), None)
        }
        EventType::AgentActivated { agent_id } => {
            ("AgentActivated", Some(agent_id.clone()), None, None)
        }
        EventType::AgentFailed { agent_id, .. } => {
            ("AgentFailed", Some(agent_id.clone()), None, None)
        }
        EventType::ToolResult { tool_name } => ("ToolResult", None, None, Some(tool_name.clone())),
        EventType::UserInput => ("UserInput", None, None, None),
        EventType::WorkflowCompleted => ("WorkflowCompleted", None, None, None),
        EventType::Custom(s) => ("Custom", None, None, Some(s.clone())),
    };
    // Observability detail rides on the event payload.
    let p = &notif.payload;
    if agent.is_none() {
        if let Some(a) = p.get("agent_id").and_then(|v| v.as_str()) {
            agent = Some(a.to_string());
        }
    }
    StreamFrame::Event {
        event_type: kind.to_string(),
        agent,
        task,
        name,
        output: p.get("output").and_then(|v| v.as_str()).map(String::from),
        tokens: p.get("tokens").and_then(|v| v.as_u64()),
        workflow: p
            .get("workflow_id")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

/// Bridges a coordinator's run-progress callbacks (Layer 2) onto the stream bus
/// as frames the dashboard already understands: the decomposition + auction as a
/// `CoordinatorPlan`, and each worker's start/finish as `AgentActivated` /
/// `TaskCompleted` events scoped to the run. Built once per daemon, shared by
/// every coordinator.
pub struct CoordinatorStreamReporter {
    bus: StreamBus,
}

impl CoordinatorStreamReporter {
    pub fn new(bus: StreamBus) -> Self {
        Self { bus }
    }
}

impl axocoatl_actor::CoordinatorReporter for CoordinatorStreamReporter {
    fn plan(
        &self,
        workflow: &str,
        coordinator: &str,
        goal: &str,
        subtasks: &[axocoatl_actor::ReportedSubtask],
    ) {
        let subtasks = subtasks
            .iter()
            .map(|s| PlanSubtask {
                name: s.name.clone(),
                description: s.description.clone(),
                winner: s.winner.clone(),
                score: s.score,
                adhoc: s.adhoc,
                bids: s
                    .bids
                    .iter()
                    .map(|b| PlanBid {
                        worker: b.worker.clone(),
                        score: b.score,
                    })
                    .collect(),
            })
            .collect();
        let _ = self.bus.send(StreamFrame::CoordinatorPlan {
            workflow: workflow.to_string(),
            coordinator: coordinator.to_string(),
            goal: goal.to_string(),
            subtasks,
        });
    }

    fn worker_started(&self, workflow: &str, worker: &str) {
        let _ = self.bus.send(StreamFrame::Event {
            event_type: "AgentActivated".to_string(),
            agent: Some(worker.to_string()),
            task: None,
            name: None,
            output: None,
            tokens: None,
            workflow: Some(workflow.to_string()),
        });
    }

    fn worker_done(&self, workflow: &str, worker: &str, output: &str, tokens: u64) {
        let _ = self.bus.send(StreamFrame::Event {
            event_type: "TaskCompleted".to_string(),
            agent: Some(worker.to_string()),
            task: None,
            name: None,
            output: Some(output.to_string()),
            tokens: Some(tokens),
            workflow: Some(workflow.to_string()),
        });
    }

    fn worker_failed(&self, workflow: &str, worker: &str, error: &str) {
        let _ = self.bus.send(StreamFrame::Event {
            event_type: "AgentFailed".to_string(),
            agent: Some(worker.to_string()),
            task: None,
            name: None,
            output: Some(error.to_string()),
            tokens: None,
            workflow: Some(workflow.to_string()),
        });
    }

    fn worker_cancelled(&self, workflow: &str, worker: &str, partial_output: &str, tokens: u64) {
        let _ = self.bus.send(StreamFrame::Event {
            event_type: "AgentCancelled".to_string(),
            agent: Some(worker.to_string()),
            task: None,
            name: None,
            output: Some(partial_output.to_string()),
            tokens: Some(tokens),
            workflow: Some(workflow.to_string()),
        });
    }

    fn worker_panicked(&self, workflow: &str, worker: &str, error: &str) {
        let _ = self.bus.send(StreamFrame::Event {
            event_type: "AgentPanicked".to_string(),
            agent: Some(worker.to_string()),
            task: None,
            name: None,
            output: Some(error.to_string()),
            tokens: None,
            workflow: Some(workflow.to_string()),
        });
    }
}

#[cfg(test)]
mod supervision_tests {
    use super::*;

    #[tokio::test]
    async fn reconnect_cursor_partitions_snapshot_from_queued_frames_exactly_once() {
        let bus = StreamBus::new(16);
        let mut receiver = bus.subscribe();
        let activated = StreamFrame::Event {
            event_type: "AgentActivated".to_string(),
            agent: Some("coder".to_string()),
            task: None,
            name: None,
            output: None,
            tokens: None,
            workflow: Some("session-a".to_string()),
        };
        bus.send(activated).unwrap();
        bus.send(StreamFrame::Token {
            workflow: "session-a".to_string(),
            agent: "coder".to_string(),
            turn_id: None,
            delta: "one".to_string(),
        })
        .unwrap();

        let (cursor, snapshot) = bus.snapshot();
        assert_eq!(snapshot[0].agents[0].output, "one");
        for _ in 0..2 {
            let queued = receiver.recv_sequenced().await.unwrap();
            assert!(queued.sequence <= cursor, "already represented by snapshot");
        }

        bus.send(StreamFrame::Token {
            workflow: "session-a".to_string(),
            agent: "coder".to_string(),
            turn_id: None,
            delta: "two".to_string(),
        })
        .unwrap();
        let live = receiver.recv_sequenced().await.unwrap();
        assert!(live.sequence > cursor, "must replay after snapshot");
        let StreamFrame::Token { delta, .. } = live.frame else {
            panic!("expected live token");
        };
        assert_eq!(
            format!("{}{}", snapshot[0].agents[0].output, delta),
            "onetwo"
        );
    }

    #[tokio::test]
    async fn environment_change_guard_is_broadcast_and_retained_until_every_exit_settles_it() {
        let bus = StreamBus::new(16);
        let mut receiver = bus.subscribe();

        let guard = bus.begin_session_environment_change("session-a", 7);
        let changing = receiver.recv_sequenced().await.unwrap();
        assert!(matches!(
            changing.frame,
            StreamFrame::SessionEnvironmentChanging {
                ref session,
                generation: 7,
            } if session == "session-a"
        ));
        let (cursor, _, transitions) = bus.snapshot_with_environment_transitions();
        assert_eq!(cursor, changing.sequence);
        assert_eq!(
            transitions,
            vec![SessionEnvironmentTransition {
                session: "session-a".to_string(),
                generation: 7,
            }]
        );

        drop(guard);
        let settled = receiver.recv_sequenced().await.unwrap();
        assert!(settled.sequence > cursor);
        assert!(matches!(
            settled.frame,
            StreamFrame::SessionEnvironmentSettled { ref session }
                if session == "session-a"
        ));
        assert!(bus.snapshot_with_environment_transitions().2.is_empty());
    }

    #[tokio::test]
    async fn workspace_attempt_owner_is_retained_and_only_exact_settlement_clears_it() {
        let bus = StreamBus::new(16);
        let mut receiver = bus.subscribe();
        let guard = bus.begin_workspace_attempt_change("workspace-a", "session-a", "set-a");
        assert!(matches!(
            receiver.recv_sequenced().await.unwrap().frame,
            StreamFrame::WorkspaceAttemptChanging {
                ref workspace,
                ref session,
                ref attempt_set_id,
            } if workspace == "workspace-a" && session == "session-a" && attempt_set_id == "set-a"
        ));
        let ownerships = bus.snapshot_with_runtime_ownership().3;
        assert_eq!(
            ownerships,
            vec![WorkspaceAttemptOwnership {
                workspace: "workspace-a".to_string(),
                session: "session-a".to_string(),
                attempt_set_id: "set-a".to_string(),
            }]
        );

        guard.retain();
        bus.send(StreamFrame::WorkspaceAttemptSettled {
            workspace: "workspace-a".to_string(),
            attempt_set_id: "stale-set".to_string(),
        })
        .unwrap();
        let _ = receiver.recv_sequenced().await.unwrap();
        assert_eq!(bus.snapshot_with_runtime_ownership().3, ownerships);

        bus.send(StreamFrame::WorkspaceAttemptSettled {
            workspace: "workspace-a".to_string(),
            attempt_set_id: "set-a".to_string(),
        })
        .unwrap();
        let _ = receiver.recv_sequenced().await.unwrap();
        assert!(bus.snapshot_with_runtime_ownership().3.is_empty());
    }

    #[tokio::test]
    async fn stream_commit_gate_never_exposes_ledger_ahead_of_broker_cursor() {
        let bus = StreamBus::new(16);
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let ledger = Arc::new(tokio::sync::Mutex::new(String::new()));
        let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let writer = {
            let bus = bus.clone();
            let gate = gate.clone();
            let ledger = ledger.clone();
            tokio::spawn(async move {
                let _commit = gate.lock().await;
                ledger.lock().await.push_str("delta");
                let _ = persisted_tx.send(());
                let _ = release_rx.await;
                let _ = bus.send(StreamFrame::Token {
                    workflow: "session-a".to_string(),
                    agent: "coder".to_string(),
                    turn_id: None,
                    delta: "delta".to_string(),
                });
            })
        };
        persisted_rx.await.unwrap();

        let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
        let snapshot = {
            let bus = bus.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                let _ = attempted_tx.send(());
                let _commit = gate.lock().await;
                bus.snapshot()
            })
        };
        attempted_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            !snapshot.is_finished(),
            "snapshot must wait for publish commit"
        );
        release_tx.send(()).unwrap();
        writer.await.unwrap();
        let (cursor, runs) = snapshot.await.unwrap();
        assert_eq!(&*ledger.lock().await, "delta");
        assert_eq!(cursor, 1);
        assert_eq!(runs[0].agents[0].output, "delta");
    }

    #[test]
    fn bounded_completion_preview_never_replaces_longer_streamed_output() {
        let mut runs = HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::Event {
                event_type: "AgentActivated".to_string(),
                agent: Some("coder".to_string()),
                task: None,
                name: None,
                output: None,
                tokens: None,
                workflow: Some("session-a".to_string()),
            },
        );
        let full = "x".repeat(512);
        apply_frame(
            &mut runs,
            &StreamFrame::Token {
                workflow: "session-a".to_string(),
                agent: "coder".to_string(),
                turn_id: None,
                delta: full.clone(),
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::Event {
                event_type: "TaskCompleted".to_string(),
                agent: Some("coder".to_string()),
                task: None,
                name: None,
                output: Some("x".repeat(200)),
                tokens: Some(41),
                workflow: Some("session-a".to_string()),
            },
        );
        let agent = &runs["session-a"].agents[0];
        assert_eq!(agent.output, full);
        assert_eq!(agent.status, "done");
        assert_eq!(agent.tokens, 41);
    }

    #[tokio::test]
    async fn coordinator_worker_terminals_are_distinct_and_never_remain_running() {
        let bus = StreamBus::new(16);
        let mut receiver = bus.subscribe();
        let reporter = CoordinatorStreamReporter::new(bus);
        let workflow = "session-a";

        axocoatl_actor::CoordinatorReporter::worker_started(&reporter, workflow, "reviewer");
        axocoatl_actor::CoordinatorReporter::worker_failed(
            &reporter,
            workflow,
            "reviewer",
            "provider failed",
        );
        axocoatl_actor::CoordinatorReporter::worker_started(&reporter, workflow, "tester");
        axocoatl_actor::CoordinatorReporter::worker_cancelled(
            &reporter,
            workflow,
            "tester",
            "partial checks",
            17,
        );
        axocoatl_actor::CoordinatorReporter::worker_started(&reporter, workflow, "builder");
        axocoatl_actor::CoordinatorReporter::worker_panicked(
            &reporter,
            workflow,
            "builder",
            "worker task panicked",
        );

        let mut runs = std::collections::HashMap::new();
        for _ in 0..6 {
            let frame = receiver.recv().await.expect("reporter frame");
            apply_frame(&mut runs, &frame);
        }

        let run = &runs[workflow];
        let reviewer = run
            .agents
            .iter()
            .find(|agent| agent.agent == "reviewer")
            .unwrap();
        assert_eq!(reviewer.status, "error");
        assert_eq!(reviewer.output, "provider failed");
        let tester = run
            .agents
            .iter()
            .find(|agent| agent.agent == "tester")
            .unwrap();
        assert_eq!(tester.status, "cancelled");
        assert_eq!(tester.output, "partial checks");
        assert_eq!(tester.tokens, 17);
        let builder = run
            .agents
            .iter()
            .find(|agent| agent.agent == "builder")
            .unwrap();
        assert_eq!(builder.status, "error");
        assert_eq!(builder.output, "worker task panicked");
        assert!(run
            .agents
            .iter()
            .all(|agent| !agent.agent.contains(":worker:") && agent.status != "running"));
    }

    #[test]
    fn coordinator_worker_reactivation_clears_stale_terminal_payload() {
        let event =
            |event_type: &str, output: Option<&str>, tokens: Option<u64>| StreamFrame::Event {
                event_type: event_type.to_string(),
                agent: Some("reviewer".to_string()),
                task: None,
                name: None,
                output: output.map(String::from),
                tokens,
                workflow: Some("session-a".to_string()),
            };
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &event("TaskCompleted", Some("old result"), Some(29)),
        );
        runs.get_mut("session-a").unwrap().agents[0].thinking = "old reasoning".to_string();
        apply_frame(&mut runs, &event("AgentActivated", None, None));

        let worker = &runs["session-a"].agents[0];
        assert_eq!(worker.status, "running");
        assert!(worker.output.is_empty());
        assert!(worker.thinking.is_empty());
        assert_eq!(worker.tokens, 0);
    }

    #[test]
    fn derives_the_run_from_a_scoped_agent_id() {
        // A variant lane: the run is the lane, not the session.
        assert_eq!(
            run_of_scoped_agent("ses-abc#2:coder").as_deref(),
            Some("ses-abc#2")
        );
        // A plain session agent.
        assert_eq!(
            run_of_scoped_agent("ses-abc:coder").as_deref(),
            Some("ses-abc")
        );
        // An unscoped agent belongs to no run.
        assert_eq!(run_of_scoped_agent("coder"), None);
    }

    #[test]
    fn a_blocked_run_stays_blocked_until_resolved() {
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::McpApprovalRequired {
                approval_id: "ap-1".into(),
                run: Some("ses-x#1".into()),
                agent_id: "ses-x#1:coder".into(),
                server: "fs".into(),
                tool: "fs__write".into(),
                tool_display: "write".into(),
                arguments_preview: "{}".into(),
                requested_at: 42,
            },
        );
        // The state lives on the run, so a late client still learns of it.
        let a = runs["ses-x#1"].awaiting.as_ref().expect("run is blocked");
        assert_eq!(a.approval_id, "ap-1");
        assert_eq!(a.since, 42);

        apply_frame(
            &mut runs,
            &StreamFrame::McpApprovalResolved {
                approval_id: "ap-1".into(),
                decision: "allow".into(),
            },
        );
        assert!(runs["ses-x#1"].awaiting.is_none(), "resolving unblocks it");
    }

    #[test]
    fn resolving_one_approval_leaves_other_blocked_lanes_alone() {
        let mut runs = std::collections::HashMap::new();
        for (id, run) in [("ap-1", "ses-x#0"), ("ap-2", "ses-x#1")] {
            apply_frame(
                &mut runs,
                &StreamFrame::McpApprovalRequired {
                    approval_id: id.into(),
                    run: Some(run.into()),
                    agent_id: format!("{run}:coder"),
                    server: "fs".into(),
                    tool: "t".into(),
                    tool_display: "t".into(),
                    arguments_preview: "{}".into(),
                    requested_at: 1,
                },
            );
        }
        apply_frame(
            &mut runs,
            &StreamFrame::McpApprovalResolved {
                approval_id: "ap-1".into(),
                decision: "allow".into(),
            },
        );
        assert!(runs["ses-x#0"].awaiting.is_none());
        assert!(
            runs["ses-x#1"].awaiting.is_some(),
            "the other lane is still waiting"
        );
    }

    #[test]
    fn lane_frames_serialize_the_durable_attempt_set_identity() {
        let started = serde_json::to_value(StreamFrame::LaneStarted {
            run: "ses-x#0".into(),
            attempt_set_id: "set-new".into(),
            session: "ses-x".into(),
            index: 0,
            model: Some("model-a".into()),
            agent: "coder".into(),
        })
        .unwrap();
        assert_eq!(started["kind"], "lane-started");
        assert_eq!(started["attempt_set_id"], "set-new");

        let verified = serde_json::to_value(StreamFrame::LaneVerified {
            attempt_set_id: "set-new".into(),
            session: "ses-x".into(),
            index: 0,
            passed: true,
            changed_files: 1,
            touched_tests: Vec::new(),
        })
        .unwrap();
        assert_eq!(verified["kind"], "lane-verified");
        assert_eq!(verified["attempt_set_id"], "set-new");
    }

    #[test]
    fn a_late_terminal_for_an_old_turn_does_not_remove_the_new_turn() {
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::SessionAccepted {
                session: "session-a".to_string(),
                turn_id: "turn-new".to_string(),
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::SessionDone {
                session: "session-a".to_string(),
                turn_id: Some("turn-old".to_string()),
                input_tokens: 1,
                output_tokens: 1,
                reasoning_tokens: 0,
                token_usage_known: true,
            },
        );
        assert_eq!(
            runs.get("session-a").and_then(|run| run.turn_id.as_deref()),
            Some("turn-new")
        );
    }

    #[test]
    fn accepting_a_new_turn_resets_the_prior_turn_projection() {
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::SessionAccepted {
                session: "session-a".to_string(),
                turn_id: "turn-old".to_string(),
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::Token {
                workflow: "session-a".to_string(),
                agent: "coder".to_string(),
                turn_id: Some("turn-old".to_string()),
                delta: "stale output".to_string(),
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::CoordinatorPlan {
                workflow: "session-a".to_string(),
                coordinator: "lead".to_string(),
                goal: "old goal".to_string(),
                subtasks: Vec::new(),
            },
        );

        apply_frame(
            &mut runs,
            &StreamFrame::SessionAccepted {
                session: "session-a".to_string(),
                turn_id: "turn-new".to_string(),
            },
        );

        let run = &runs["session-a"];
        assert_eq!(run.turn_id.as_deref(), Some("turn-new"));
        assert!(run.agents.is_empty());
        assert!(run.coordinator.is_none());
        assert!(run.goal.is_empty());
        assert!(run.subtasks.is_empty());
    }

    #[test]
    fn matching_terminal_clears_a_direct_session_run() {
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::SessionAccepted {
                session: "session-a".to_string(),
                turn_id: "turn-a".to_string(),
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::SessionDone {
                session: "session-a".to_string(),
                turn_id: Some("turn-a".to_string()),
                input_tokens: 1,
                output_tokens: 1,
                reasoning_tokens: 0,
                token_usage_known: true,
            },
        );
        assert!(!runs.contains_key("session-a"));
    }

    #[test]
    fn stop_rejection_after_done_does_not_resurrect_or_error_the_run() {
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::SessionAccepted {
                session: "session-a".to_string(),
                turn_id: "turn-a".to_string(),
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::SessionDone {
                session: "session-a".to_string(),
                turn_id: Some("turn-a".to_string()),
                input_tokens: 1,
                output_tokens: 1,
                reasoning_tokens: 0,
                token_usage_known: true,
            },
        );
        apply_frame(
            &mut runs,
            &StreamFrame::SessionStopRejected {
                session: "session-a".to_string(),
                turn_id: "turn-a".to_string(),
                error: "already completed".to_string(),
            },
        );
        assert!(!runs.contains_key("session-a"));
    }

    #[test]
    fn provider_local_call_ids_get_distinct_fifo_occurrences() {
        let mut occurrences = ToolCallOccurrences::default();
        let first = occurrences.start("call_0");
        assert_eq!(occurrences.finish("call_0"), first);
        let second = occurrences.start("call_0");
        assert_ne!(second, first);
        assert_eq!(occurrences.finish("call_0"), second);

        let parallel_a = occurrences.start("call_0");
        let parallel_b = occurrences.start("call_0");
        assert_eq!(occurrences.finish("call_0"), parallel_a);
        assert_eq!(occurrences.finish("call_0"), parallel_b);

        let empty_a = occurrences.start("");
        let empty_b = occurrences.start("");
        let positions = HashMap::from([
            ((String::new(), empty_a), 4_usize),
            ((String::new(), empty_b), 5_usize),
        ]);
        let finished_a = occurrences.finish("");
        let finished_b = occurrences.finish("");
        assert_eq!(positions.get(&(String::new(), finished_a)), Some(&4));
        assert_eq!(positions.get(&(String::new(), finished_b)), Some(&5));
    }

    #[test]
    fn snapshot_serializes_complete_scoped_and_unscoped_approvals() {
        fn approval(id: &str, agent_id: &str, requested_at: u64) -> PendingMcpApproval {
            ApprovalContext {
                approval_id: id.into(),
                agent_id: agent_id.into(),
                server: "filesystem".into(),
                tool: "mcp__filesystem__write".into(),
                tool_display: "write".into(),
                arguments_preview: format!(r#"{{"approval":"{id}"}}"#),
                requested_at,
            }
            .into()
        }

        let frame = serde_json::to_value(StreamFrame::Snapshot {
            runs: Vec::new(),
            approvals: vec![
                approval("ap-lane-0", "ses-x#0:coder", 10),
                approval("ap-lane-1", "ses-x#1:reviewer", 11),
                approval("ap-unscoped", "automation-agent", 12),
            ],
            environment_transitions: vec![SessionEnvironmentTransition {
                session: "ses-x".to_string(),
                generation: 9,
            }],
            attempt_ownerships: vec![WorkspaceAttemptOwnership {
                workspace: "ws-x".to_string(),
                session: "ses-x".to_string(),
                attempt_set_id: "set-x".to_string(),
            }],
        })
        .expect("snapshot serializes");

        assert_eq!(frame["kind"], "snapshot");
        assert_eq!(frame["environment_transitions"][0]["session"], "ses-x");
        assert_eq!(frame["environment_transitions"][0]["generation"], 9);
        assert_eq!(frame["attempt_ownerships"][0]["workspace"], "ws-x");
        assert_eq!(frame["attempt_ownerships"][0]["attempt_set_id"], "set-x");
        let approvals = frame["approvals"].as_array().expect("approval list");
        assert_eq!(approvals.len(), 3, "parallel approvals are not collapsed");
        assert_eq!(approvals[0]["run"], "ses-x#0");
        assert_eq!(approvals[1]["run"], "ses-x#1");
        assert!(
            approvals[2].get("run").is_none(),
            "unscoped approvals remain present without a fabricated owner"
        );
        assert_eq!(approvals[2]["agent_id"], "automation-agent");
        assert_eq!(
            approvals[2]["arguments_preview"],
            r#"{"approval":"ap-unscoped"}"#
        );
    }

    #[test]
    fn lane_folding_keeps_set_identity_without_resurrecting_finished_runs() {
        let mut runs = std::collections::HashMap::new();
        apply_frame(
            &mut runs,
            &StreamFrame::LaneStarted {
                run: "ses-x#0".into(),
                attempt_set_id: "set-new".into(),
                session: "ses-x".into(),
                index: 0,
                model: None,
                agent: "coder".into(),
            },
        );
        assert_eq!(runs["ses-x#0"].kind, "attempt");
        assert_eq!(runs["ses-x#0"].attempt_set_id.as_deref(), Some("set-new"));

        apply_frame(
            &mut runs,
            &StreamFrame::LaneVerified {
                attempt_set_id: "set-new".into(),
                session: "ses-x".into(),
                index: 0,
                passed: true,
                changed_files: 1,
                touched_tests: Vec::new(),
            },
        );
        assert_eq!(runs["ses-x#0"].attempt_set_id.as_deref(), Some("set-new"));

        runs.remove("ses-x#0");
        apply_frame(
            &mut runs,
            &StreamFrame::LaneVerified {
                attempt_set_id: "set-old".into(),
                session: "ses-x".into(),
                index: 0,
                passed: false,
                changed_files: 0,
                touched_tests: Vec::new(),
            },
        );
        assert!(!runs.contains_key("ses-x#0"));
    }
}
