//! The observability stream bus.
//!
//! One broadcast channel carries everything the dashboard's WebSocket needs:
//! flattened lattice coordination events plus live, token-by-token agent
//! output. The daemon owns the sender; each WebSocket connection subscribes.

use serde::Serialize;

use axocoatl_coordination::{EventNotification, EventType};

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
    /// Sent once to a freshly-connected client — the state of every run
    /// currently in flight, so the dashboard can re-attach its live view.
    Snapshot { runs: Vec<RunState> },
    /// An agent is about to call an MCP tool that has no recorded permission
    /// decision — the dashboard should prompt the user. Carries the data the
    /// user needs to decide: which agent, which server+tool, a preview of the
    /// arguments. Resolution comes back via `WsCommand::McpApprove`.
    McpApprovalRequired {
        approval_id: String,
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

/// Live state of one in-flight run (workflow or session), rebuilt purely from
/// stream frames.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunState {
    /// The run id — a workflow id or a session id.
    pub workflow: String,
    /// `"workflow"` or `"session"` — lets the dashboard re-attach the right view.
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
/// place lattice `EventType`s are mapped to the wire shape the dashboard sees.
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
