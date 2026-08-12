//! The observability stream bus.
//!
//! One broadcast channel carries the live session/run state the app observes,
//! plus retained lattice/workflow/chat compatibility frames. The daemon owns
//! the sender; each WebSocket connection subscribes.

use serde::Serialize;

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
        delta: String,
    },
    /// A streamed reasoning / "thinking" chunk from a running agent.
    Reasoning {
        workflow: String,
        agent: String,
        delta: String,
    },
    /// A tool call from a running agent. `phase` is `"start"` (carries
    /// `arguments`) or `"result"` (carries `result` + `is_error`). `workflow`
    /// holds the run id — a workflow id or a session id.
    ToolCall {
        workflow: String,
        agent: String,
        call_id: String,
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
    },
    /// A workflow run failed.
    WorkflowError { workflow: String, error: String },
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
    SessionStart { session: String },
    /// A directory-session run finished.
    SessionDone {
        session: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A directory-session run failed.
    SessionError { session: String, error: String },
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

/// Fold a stream frame into the in-flight run registry. Called by the daemon's
/// run-tracker task for every frame on the bus.
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
                run_for(runs, wf).agent_mut(agent).status = "running".to_string();
            }
            "TaskCompleted" => {
                let a = run_for(runs, wf).agent_mut(agent);
                a.status = "done".to_string();
                if let Some(t) = tokens {
                    a.tokens = *t;
                }
                if let Some(o) = output {
                    a.output = o.clone();
                }
            }
            "AgentFailed" => {
                run_for(runs, wf).agent_mut(agent).status = "error".to_string();
            }
            _ => {}
        },
        StreamFrame::Token {
            workflow,
            agent,
            delta,
        } => {
            run_for(runs, workflow)
                .agent_mut(agent)
                .output
                .push_str(delta);
        }
        StreamFrame::Reasoning {
            workflow,
            agent,
            delta,
        } => {
            run_for(runs, workflow)
                .agent_mut(agent)
                .thinking
                .push_str(delta);
        }
        StreamFrame::WorkflowDone { workflow, .. }
        | StreamFrame::WorkflowError { workflow, .. } => {
            runs.remove(workflow);
        }
        StreamFrame::SessionStart { session } => {
            runs.entry(session.clone()).or_insert_with(|| RunState {
                workflow: session.clone(),
                kind: "session".to_string(),
                agents: Vec::new(),
                ..Default::default()
            });
        }
        StreamFrame::SessionDone { session, .. } | StreamFrame::SessionError { session, .. } => {
            runs.remove(session);
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
    bus: tokio::sync::broadcast::Sender<StreamFrame>,
}

impl CoordinatorStreamReporter {
    pub fn new(bus: tokio::sync::broadcast::Sender<StreamFrame>) -> Self {
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
}

#[cfg(test)]
mod supervision_tests {
    use super::*;

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
        })
        .expect("snapshot serializes");

        assert_eq!(frame["kind"], "snapshot");
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
