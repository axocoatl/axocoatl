//! Unified `Automation` schema.
//!
//! An automation is the single, canonical answer to "what runs without me
//! sitting on the keyboard." Every existing workflow/schedule/proactive in
//! the YAML converges into this one shape via [`Automation::from_legacy`].
//!
//! Structure:
//!
//! ```text
//! Automation
//!   ├ nodes   ─ Vec<AutomationNode>     ← what runs
//!   ├ edges   ─ Vec<AutomationEdge>     ← how outputs flow
//!   ├ trigger ─ AutomationTrigger       ← what kicks it off
//!   └ enabled ─ bool
//! ```
//!
//! Each node has its own `input` declaration so nodes can pull from the
//! trigger, an upstream node, a literal, or a template. This generalizes
//! the today-implicit rule "every agent gets the same input passed down."

use serde::{Deserialize, Serialize};

use crate::types::{ProactiveConfigYaml, ProactiveTrigger, ScheduleConfigYaml, WorkflowConfigYaml};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Automation {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub nodes: Vec<AutomationNode>,
    #[serde(default)]
    pub edges: Vec<AutomationEdge>,
    pub trigger: AutomationTrigger,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Folder path the user organized this automation under, slash-separated
    /// (e.g. `"client/spec-reviews/v2"`). `None` = at the root ("Unfiled").
    /// Folders themselves are persisted in `AutomationFolderStore`; this is
    /// just the back-pointer that lets the UI filter the card grid.
    #[serde(default)]
    pub folder: Option<String>,
}

/// A user-created organizational folder for automations. Folders survive
/// being empty so the user can scaffold a hierarchy before populating it.
/// The `path` is the unique key (slash-separated); `name` overrides the
/// default display name (which is the path's last segment).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutomationFolder {
    /// Slash-separated path. Cannot start or end with `/`, cannot contain
    /// empty segments. Examples: `"client"`, `"client/spec-reviews"`.
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl AutomationFolder {
    /// Display name: explicit override if set, else the last path segment.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| self.path.rsplit('/').next().unwrap_or(""))
    }
    /// Parent path, or `None` for top-level folders.
    pub fn parent(&self) -> Option<String> {
        self.path
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutomationNode {
    pub id: String,
    pub kind: AutomationNodeKind,
    #[serde(default)]
    pub position: Option<Position>,
}

/// Open enum — every node type implements its own execution.  The visual
/// editor renders each kind differently; the executor dispatches on `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationNodeKind {
    /// Runs an agent with a configurable input.
    Agent {
        agent_id: String,
        #[serde(default = "NodeInput::from_trigger")]
        input: NodeInput,
    },
    /// Calls a tool from the registry directly — no LLM in the loop.
    /// Output is the tool's stringified result. Deterministic, fast,
    /// cheap; great for pre/post processing inside an agent workflow.
    Tool {
        tool_id: String,
        #[serde(default = "NodeInput::from_trigger")]
        input: NodeInput,
    },
    /// Routes execution to one of N labeled branches based on an
    /// expression evaluated against the node's resolved input. Edges
    /// leaving this node carry a `label` matching one of the branch
    /// names; only the matching branch's edges are activated.
    Conditional {
        #[serde(default = "NodeInput::from_trigger")]
        input: NodeInput,
        /// Ordered; first match wins. The node's own output is set to
        /// the matched branch name (useful for downstream debugging).
        branches: Vec<ConditionalBranch>,
        /// Branch to follow when no `branches` expression matched.
        /// `None` = no branch fires; downstream of this conditional
        /// halts entirely.
        #[serde(default)]
        default: Option<String>,
    },
    /// Fan-out: resolve `input` to a list, then run `body_node` once per
    /// item. The Map's own output is a JSON array of the body's outputs.
    /// Within the body, `NodeInput::FromMapItem` resolves to the current
    /// item. Downstream of the Map (via its outgoing edges) runs once
    /// after all iterations complete — same model as LangGraph's `Send`.
    Map {
        #[serde(default = "NodeInput::from_trigger")]
        input: NodeInput,
        /// Node id (within this automation) that runs once per item.
        body_node: String,
    },
    /// Call another automation as a node. The referenced automation runs
    /// to completion; the Subgraph's output is the referenced automation's
    /// `final_content`. Recursive composition; depth-limited at executor
    /// level to prevent infinite loops.
    Subgraph {
        automation_id: String,
        #[serde(default = "NodeInput::from_trigger")]
        input: NodeInput,
    },
    /// A first-class input slot on the canvas. Replaces the implicit
    /// "trigger input" concept with an explicit, visible-in-the-graph
    /// node that has a label + optional default value.
    ///
    /// For manual triggers: at run time the dashboard surfaces every
    /// `TextInput` in the automation as a form field. The operator fills
    /// them in; each node's output becomes the supplied value.
    ///
    /// For scheduled / proactive triggers: `default_value` is what fires
    /// every time. The form is never shown.
    TextInput {
        /// What the operator sees as the field name.
        label: String,
        /// Pre-filled value. Used as the fallback when no value is
        /// supplied at run time (scheduled/proactive automations rely on
        /// this).
        #[serde(default)]
        default_value: Option<String>,
        /// Optional UI hint shown when the field is empty.
        #[serde(default)]
        placeholder: Option<String>,
        /// `true` renders a multi-line textarea, `false` a single-line input.
        #[serde(default)]
        multiline: bool,
    },
    /// Pause execution and wait for human approval/input. Resumable via
    /// `POST /api/automations/{id}/runs/{run_id}/resume`. While paused, the
    /// dashboard lists the interrupt with its message + payload so the
    /// operator can decide what to do.
    Interrupt {
        /// Message shown to the operator. Resolves NodeInputs at pause time.
        #[serde(default = "NodeInput::from_trigger")]
        input: NodeInput,
        /// What the operator's resume value replaces / supplements.
        /// `Replace` overwrites this node's output with the resume value;
        /// `Append` concatenates after the resolved message.
        #[serde(default = "default_resume_strategy")]
        resume_strategy: ResumeStrategy,
    },
    // Reserved for v0.2+:
    //
    // Skill { skill_id: String },
}

fn default_resume_strategy() -> ResumeStrategy {
    ResumeStrategy::Replace
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeStrategy {
    /// The operator's value becomes the node's output (overrides the
    /// resolved input). Default — fits "approve and provide a value" UX.
    Replace,
    /// The operator's value is appended after the resolved input.
    Append,
}

/// One labeled branch in a [`AutomationNodeKind::Conditional`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConditionalBranch {
    /// Branch label — referenced by outgoing edges' `label` field.
    pub name: String,
    /// Predicate to test against the conditional's resolved input.
    pub when: BranchExpr,
}

/// Predicates for [`ConditionalBranch::when`]. Intentionally limited to
/// patterns we can implement without a real expression engine. Promote
/// to a proper DSL when we feel the constraint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BranchExpr {
    /// Always matches — catch-all for the "default route".
    Always,
    /// String equality (case-sensitive).
    Equals { value: String },
    /// Substring containment.
    Contains { value: String },
    /// Regex match (Rust regex crate syntax).
    Matches { pattern: String },
    /// Input has any non-whitespace content.
    NotEmpty,
}

impl BranchExpr {
    /// Evaluate this predicate against an input string. Regex failures
    /// silently fall through (don't match) — we don't want a bad regex
    /// to kill a whole automation run.
    pub fn matches(&self, input: &str) -> bool {
        match self {
            BranchExpr::Always => true,
            BranchExpr::Equals { value } => input == value,
            BranchExpr::Contains { value } => input.contains(value),
            BranchExpr::Matches { pattern } => regex::Regex::new(pattern)
                .map(|re| re.is_match(input))
                .unwrap_or(false),
            BranchExpr::NotEmpty => !input.trim().is_empty(),
        }
    }
}

/// How a node figures out what to feed its agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeInput {
    /// Use the trigger's input — user prompt (manual), schedule's `input`,
    /// event payload (on_event), etc. The default for newly-added nodes.
    FromTrigger,
    /// A fixed string set at design time. Useful for "always summarize" or
    /// "always check security" subordinates that don't need a fresh prompt.
    Literal { value: String },
    /// Concatenate outputs from named upstream node ids — the implicit rule
    /// for today's workflows (every agent sees prior outputs).
    FromUpstream { nodes: Vec<String> },
    /// Mix literal text with `{{trigger}}` and `{{node:id}}` placeholders.
    /// The executor substitutes references at run time. Most expressive.
    /// Within a Map's body, `{{item}}` substitutes the current Map item.
    Template { template: String },
    /// Current item from a Map node's iteration. Outside a Map, resolves
    /// to empty string. Only meaningful as a body-node input.
    FromMapItem,
}

impl NodeInput {
    pub fn from_trigger() -> Self {
        Self::FromTrigger
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutomationEdge {
    pub from: String,
    pub to: String,
    /// Branch label — meaningful only when `from` is a `Conditional`
    /// node. The edge fires only when that conditional's match selects
    /// this branch. `None` on edges out of regular nodes (they always
    /// fire when their source completes).
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTrigger {
    /// User clicks Run. Input comes from the UI prompt.
    Manual,
    /// Fires on a cron-like cadence. Stored `input` is reused every fire.
    Schedule {
        every: String,
        #[serde(default)]
        input: Option<String>,
    },
    /// Fires when a lattice event matches. Event name is the match key;
    /// `input` is the fallback prompt if the event payload doesn't carry one.
    OnEvent {
        event: String,
        #[serde(default)]
        input: Option<String>,
    },
    /// Fires when a specific skill is published.
    OnSkill { skill_id: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

// ── Backwards-compat: convert legacy YAML sections → Vec<Automation> ──

impl Automation {
    /// Take the three legacy YAML sections (`workflows:`, `schedules:`,
    /// `proactive:`) and project them into a single `Vec<Automation>`. The
    /// daemon calls this once at boot and from then on operates on the
    /// unified shape.
    ///
    /// Convention:
    /// * **Each workflow** becomes a Manual automation. Nodes = the workflow's
    ///   agents; edges follow `depends_on` chains discovered by the caller
    ///   (we pass them in as `agent_deps`).
    /// * **Each schedule** becomes a separate automation: same nodes/edges
    ///   as the referenced workflow, but trigger = `Schedule { every, input }`.
    ///   ID prefixed with `sched:` so it doesn't collide.
    /// * **Each proactive** becomes a single-node automation with the trigger
    ///   matching its `trigger:` block.
    pub fn from_legacy(
        workflows: &[WorkflowConfigYaml],
        schedules: &[ScheduleConfigYaml],
        proactives: &[ProactiveConfigYaml],
        agent_deps: &dyn Fn(&str) -> Vec<String>,
    ) -> Vec<Automation> {
        let mut out: Vec<Automation> = Vec::new();

        // 1) Workflows → Manual automations
        for wf in workflows {
            out.push(Automation {
                id: wf.id.clone(),
                name: wf.name.clone(),
                description: None,
                nodes: workflow_nodes(&wf.agents),
                edges: workflow_edges(&wf.agents, agent_deps),
                trigger: AutomationTrigger::Manual,
                enabled: true,
                folder: None,
            });
        }

        // 2) Schedules → time-triggered automations (cloning the workflow's
        //    structure under a new id).
        for sch in schedules {
            let agents = workflows
                .iter()
                .find(|w| w.id == sch.workflow)
                .map(|w| w.agents.clone())
                .unwrap_or_default();
            out.push(Automation {
                id: format!("sched:{}", sch.id),
                name: sch.name.clone(),
                description: None,
                nodes: workflow_nodes(&agents),
                edges: workflow_edges(&agents, agent_deps),
                trigger: AutomationTrigger::Schedule {
                    every: sch.every.clone(),
                    input: nonempty(&sch.input),
                },
                enabled: sch.enabled,
                folder: None,
            });
        }

        // 3) Proactives → event/schedule/skill triggered automations.
        //    Single-agent today; v0.2 will let users add downstream nodes
        //    in the visual editor.
        for pr in proactives {
            let agent_id = pr.agent.clone();
            let node = AutomationNode {
                id: agent_id.clone(),
                kind: AutomationNodeKind::Agent {
                    agent_id,
                    input: NodeInput::FromTrigger,
                },
                position: None,
            };
            let trigger = match &pr.trigger {
                ProactiveTrigger::Schedule { every } => AutomationTrigger::Schedule {
                    every: every.clone(),
                    input: nonempty(&pr.input),
                },
                ProactiveTrigger::OnEvent { event } => AutomationTrigger::OnEvent {
                    event: event.clone(),
                    input: nonempty(&pr.input),
                },
            };
            out.push(Automation {
                id: format!("pro:{}", pr.id),
                name: pr.name.clone(),
                description: None,
                nodes: vec![node],
                edges: vec![],
                trigger,
                enabled: pr.enabled,
                folder: None,
            });
        }
        out
    }

    /// Topological execution order of the automation's nodes.  Nodes with
    /// no incoming edges go first.  Cycles are tolerated by appending any
    /// remaining nodes at the end (the caller surfaces them as warnings).
    pub fn execution_order(&self) -> Vec<String> {
        use std::collections::{HashMap, HashSet, VecDeque};
        let mut indegree: HashMap<&str, usize> =
            self.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
        for e in &self.edges {
            *indegree.entry(e.to.as_str()).or_insert(0) += 1;
        }
        let mut q: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::with_capacity(self.nodes.len());
        while let Some(n) = q.pop_front() {
            if !seen.insert(n.to_string()) {
                continue;
            }
            order.push(n.to_string());
            for e in self.edges.iter().filter(|e| e.from == n) {
                if let Some(d) = indegree.get_mut(e.to.as_str()) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        q.push_back(e.to.as_str());
                    }
                }
            }
        }
        // Cycle salvage — any node not visited gets appended.
        for n in &self.nodes {
            if !seen.contains(&n.id) {
                order.push(n.id.clone());
            }
        }
        order
    }
}

/// One node per agent, with FromTrigger input (today's implicit behavior).
/// `String` → `Option<String>` where empty maps to `None`. Legacy YAML
/// uses empty string for "no input" because of `#[serde(default)]` on
/// String fields. The new schema uses Option which reads better.
fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn workflow_nodes(agents: &[String]) -> Vec<AutomationNode> {
    agents
        .iter()
        .map(|aid| AutomationNode {
            id: aid.clone(),
            kind: AutomationNodeKind::Agent {
                agent_id: aid.clone(),
                input: NodeInput::FromTrigger,
            },
            position: None,
        })
        .collect()
}

/// Build edges from each agent's `depends_on` set, intersected with the
/// agents that are actually in this workflow.
fn workflow_edges(
    agents: &[String],
    agent_deps: &dyn Fn(&str) -> Vec<String>,
) -> Vec<AutomationEdge> {
    let set: std::collections::HashSet<&str> = agents.iter().map(|s| s.as_str()).collect();
    let mut edges = Vec::new();
    for aid in agents {
        for dep in agent_deps(aid) {
            if set.contains(dep.as_str()) {
                edges.push(AutomationEdge {
                    from: dep,
                    to: aid.clone(),
                    label: None,
                });
            }
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProactiveConfigYaml, ScheduleConfigYaml, WorkflowConfigYaml};

    fn no_deps(_: &str) -> Vec<String> {
        Vec::new()
    }

    fn wf(id: &str, agents: &[&str]) -> WorkflowConfigYaml {
        WorkflowConfigYaml {
            id: id.into(),
            name: id.into(),
            coordination: "stigmergic_lattice".into(),
            agents: agents.iter().map(|s| s.to_string()).collect(),
            entry_point: agents.first().map(|s| s.to_string()),
            htn_methods_file: None,
        }
    }

    #[test]
    fn workflow_becomes_manual_automation() {
        let a = Automation::from_legacy(
            &[wf("feat-dev", &["architect", "coder"])],
            &[],
            &[],
            &no_deps,
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].id, "feat-dev");
        assert!(matches!(a[0].trigger, AutomationTrigger::Manual));
        assert_eq!(a[0].nodes.len(), 2);
        assert_eq!(a[0].edges.len(), 0); // no deps
    }

    #[test]
    fn schedule_carries_its_workflow_structure() {
        let sch = ScheduleConfigYaml {
            id: "morn".into(),
            name: "Morning Briefing".into(),
            workflow: "brief".into(),
            every: "1h".into(),
            input: "Brief me".into(),
            enabled: true,
        };
        let a = Automation::from_legacy(&[wf("brief", &["secretary"])], &[sch], &[], &no_deps);
        assert_eq!(a.len(), 2); // workflow + schedule
        let scheduled = a.iter().find(|x| x.id.starts_with("sched:")).unwrap();
        match &scheduled.trigger {
            AutomationTrigger::Schedule { every, input } => {
                assert_eq!(every, "1h");
                assert_eq!(input.as_deref(), Some("Brief me"));
            }
            _ => panic!("expected Schedule"),
        }
        assert_eq!(scheduled.nodes.len(), 1);
    }

    #[test]
    fn proactive_with_on_event_trigger() {
        let pr = ProactiveConfigYaml {
            id: "fail-watch".into(),
            name: "Failure Watcher".into(),
            agent: "ops".into(),
            trigger: ProactiveTrigger::OnEvent {
                event: "AgentFailed".into(),
            },
            input: "Diagnose".into(),
            enabled: true,
        };
        let a = Automation::from_legacy(&[], &[], &[pr], &no_deps);
        assert_eq!(a.len(), 1);
        match &a[0].trigger {
            AutomationTrigger::OnEvent { event, input } => {
                assert_eq!(event, "AgentFailed");
                assert_eq!(input.as_deref(), Some("Diagnose"));
            }
            _ => panic!("expected OnEvent"),
        }
    }

    #[test]
    fn branch_expr_predicates() {
        assert!(BranchExpr::Always.matches(""));
        assert!(BranchExpr::Always.matches("anything"));

        assert!(BranchExpr::Equals {
            value: "yes".into()
        }
        .matches("yes"));
        assert!(!BranchExpr::Equals {
            value: "yes".into()
        }
        .matches("YES"));

        assert!(BranchExpr::Contains {
            value: "ERROR".into()
        }
        .matches("got ERROR: boom"));
        assert!(!BranchExpr::Contains {
            value: "ERROR".into()
        }
        .matches("all good"));

        assert!(BranchExpr::Matches {
            pattern: r"^\d+$".into()
        }
        .matches("42"));
        assert!(!BranchExpr::Matches {
            pattern: r"^\d+$".into()
        }
        .matches("4 2"));
        // Bad regex → never matches, doesn't panic
        assert!(!BranchExpr::Matches {
            pattern: "[invalid".into()
        }
        .matches("anything"));

        assert!(BranchExpr::NotEmpty.matches("x"));
        assert!(!BranchExpr::NotEmpty.matches("   \t  \n"));
    }

    #[test]
    fn topo_order_respects_edges() {
        let auto = Automation {
            id: "x".into(),
            name: "x".into(),
            description: None,
            nodes: vec![
                AutomationNode {
                    id: "a".into(),
                    kind: AutomationNodeKind::Agent {
                        agent_id: "a".into(),
                        input: NodeInput::FromTrigger,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "b".into(),
                    kind: AutomationNodeKind::Agent {
                        agent_id: "b".into(),
                        input: NodeInput::FromTrigger,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "c".into(),
                    kind: AutomationNodeKind::Agent {
                        agent_id: "c".into(),
                        input: NodeInput::FromTrigger,
                    },
                    position: None,
                },
            ],
            edges: vec![
                AutomationEdge {
                    from: "a".into(),
                    to: "b".into(),
                    label: None,
                },
                AutomationEdge {
                    from: "b".into(),
                    to: "c".into(),
                    label: None,
                },
            ],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };
        let order = auto.execution_order();
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }
}
