//! Unified executor for [`Automation`].
//!
//! One function — [`execute_automation`] — runs every automation regardless
//! of trigger source. Scheduled fires, proactive event triggers, and
//! user-initiated workflow runs all converge here.
//!
//! The executor uses an **active-edge** model. Every edge starts inactive.
//! When a node finishes:
//!
//!   • **Agent / Tool** — every outgoing edge becomes active.
//!   • **Conditional** — only the outgoing edges whose `label` matches the
//!     branch that was selected become active. Branches that don't match
//!     leave their edges inactive; downstream nodes that have no active
//!     incoming edge are *skipped*.
//!
//! A node runs when it has either no incoming edges (root) or at least one
//! active incoming edge.
//!
//! Per-node input resolution ([`NodeInput`]) is unchanged: FromTrigger,
//! Literal, FromUpstream, Template.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axocoatl_config::{
    Automation, AutomationNode, AutomationNodeKind, BranchExpr, ConditionalBranch, NodeInput,
    ResumeStrategy,
};
use axocoatl_core::{AgentOutput, TokenUsageStats};

use crate::bootstrap::AxocoatlDaemon;
use crate::error::DaemonError;
use crate::interrupt::PendingInterrupt;
use crate::workflow::WorkflowOutput;

/// Cap on Subgraph recursion depth so a misconfigured automation
/// (A calls B calls A) doesn't blow the stack or hang forever.
const MAX_SUBGRAPH_DEPTH: usize = 8;

/// Run an automation end-to-end. Returns a `WorkflowOutput` so existing
/// callers (scheduler, proactive runner, /api/automations/{id}/run) don't
/// need to change their return-type expectations.
pub async fn execute_automation(
    daemon: &AxocoatlDaemon,
    automation: &Automation,
    trigger_input: &str,
) -> Result<WorkflowOutput, DaemonError> {
    execute_automation_with_inputs(daemon, automation, trigger_input, &HashMap::new()).await
}

/// Run an automation with explicit per-node TextInput values. Used by the
/// dashboard's run-form modal; legacy callers (workflows from the command
/// palette, scheduler) keep using `execute_automation`.
pub async fn execute_automation_with_inputs(
    daemon: &AxocoatlDaemon,
    automation: &Automation,
    trigger_input: &str,
    text_inputs: &HashMap<String, String>,
) -> Result<WorkflowOutput, DaemonError> {
    let run_id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = daemon
        .run_store
        .start(&automation.id, &run_id, trigger_input, None)
        .await
    {
        tracing::warn!("could not record run start: {e}");
    }
    let result = execute_automation_inner_with_inputs(
        daemon,
        automation,
        trigger_input,
        text_inputs,
        &run_id,
        0,
    )
    .await;
    let status = match &result {
        Ok(_) => crate::automation_runs::RunStatus::Completed,
        Err(_) => crate::automation_runs::RunStatus::Failed,
    };
    if let Err(e) = daemon
        .run_store
        .finish(&automation.id, &run_id, status)
        .await
    {
        tracing::warn!("could not finalize run: {e}");
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub fn execute_automation_inner<'a>(
    daemon: &'a AxocoatlDaemon,
    automation: &'a Automation,
    trigger_input: &'a str,
    run_id: &'a str,
    depth: usize,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<WorkflowOutput, DaemonError>> + Send + 'a>,
> {
    Box::pin(async move {
        execute_automation_inner_with_inputs(
            daemon,
            automation,
            trigger_input,
            &HashMap::new(),
            run_id,
            depth,
        )
        .await
    })
}

#[allow(clippy::too_many_arguments)]
pub fn execute_automation_inner_with_inputs<'a>(
    daemon: &'a AxocoatlDaemon,
    automation: &'a Automation,
    trigger_input: &'a str,
    text_inputs: &'a HashMap<String, String>,
    run_id: &'a str,
    depth: usize,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<WorkflowOutput, DaemonError>> + Send + 'a>,
> {
    Box::pin(async move {
        if depth > MAX_SUBGRAPH_DEPTH {
            return Err(DaemonError::WorkflowExecution(format!(
            "automation '{}' exceeded max subgraph depth ({MAX_SUBGRAPH_DEPTH}) — likely a recursive call cycle",
            automation.id
        )));
        }

        let order = automation.execution_order();

        // Map nodes own their body nodes — those are NOT in the top-level
        // execution order. Collect them so we skip them in the main loop.
        let body_nodes: HashSet<String> = automation
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                AutomationNodeKind::Map { body_node, .. } => Some(body_node.clone()),
                _ => None,
            })
            .collect();

        // Edge activation set. We key by (from, to) — labels live on the edge
        // itself; we don't need them in the key because a single (from, to)
        // pair is unique.
        let mut active: HashSet<(String, String)> = HashSet::new();

        let mut outputs: HashMap<String, String> = HashMap::new();
        let mut agent_outputs: Vec<(String, AgentOutput)> = Vec::new();
        let mut completed: Vec<String> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut total_tokens = TokenUsageStats::default();
        let mut step_idx: usize = 0;

        for node_id in &order {
            // Body nodes only run via Map's executor.
            if body_nodes.contains(node_id) {
                continue;
            }
            let Some(node) = automation.nodes.iter().find(|n| n.id == *node_id) else {
                tracing::warn!(
                    "automation '{}' has edge pointing to unknown node '{}'",
                    automation.id,
                    node_id
                );
                continue;
            };

            // Decide whether this node should run at all.
            let incoming: Vec<&axocoatl_config::AutomationEdge> = automation
                .edges
                .iter()
                .filter(|e| e.to == *node_id)
                .collect();
            if !incoming.is_empty()
                && !incoming
                    .iter()
                    .any(|e| active.contains(&(e.from.clone(), e.to.clone())))
            {
                // Every upstream branch decided not to fire to us.
                tracing::debug!(
                    "automation '{}' skipping node '{}' — no active incoming edge",
                    automation.id,
                    node_id
                );
                continue;
            }

            let resolved_input = resolve_node_input(node, trigger_input, &outputs);

            match &node.kind {
                AutomationNodeKind::Agent { agent_id, .. } => {
                    match run_agent_node(daemon, &automation.id, node_id, agent_id, &resolved_input)
                        .await
                    {
                        Ok(output) => {
                            outputs.insert(node_id.clone(), output.content.clone());
                            total_tokens.input_tokens = total_tokens
                                .input_tokens
                                .saturating_add(output.token_usage.input_tokens);
                            total_tokens.output_tokens = total_tokens
                                .output_tokens
                                .saturating_add(output.token_usage.output_tokens);
                            agent_outputs.push((agent_id.clone(), output));
                            completed.push(agent_id.clone());
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                        Err(e) => {
                            record_failure(&mut failed, &mut outputs, node_id, agent_id, e);
                            // Failed agents still activate outgoing edges so the user
                            // can route to a "handle failure" branch if they want.
                            // Whether the cascade continues is up to those downstream.
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                    }
                }
                AutomationNodeKind::Tool { tool_id, .. } => {
                    match run_tool_node(daemon, tool_id, &resolved_input).await {
                        Ok(output_str) => {
                            outputs.insert(node_id.clone(), output_str.clone());
                            completed.push(format!("tool:{tool_id}"));
                            activate_all_outgoing(automation, node_id, &mut active);
                            // Emit a TaskCompleted-style event for Studio so the
                            // node pulses just like an agent.
                            emit_event(
                                daemon,
                                &automation.id,
                                None, // tool nodes have no Studio agent counterpart
                                node_id,
                                "TaskCompleted",
                                Some(&output_str),
                                None,
                            );
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            failed.push((format!("tool:{tool_id}"), msg.clone()));
                            outputs.insert(node_id.clone(), String::new());
                            tracing::warn!(
                                "automation '{}' tool node '{}' (tool {}) failed: {}",
                                automation.id,
                                node_id,
                                tool_id,
                                msg
                            );
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                    }
                }
                AutomationNodeKind::Conditional {
                    branches, default, ..
                } => {
                    let chosen = pick_branch(&resolved_input, branches, default.as_deref());
                    outputs.insert(node_id.clone(), chosen.clone().unwrap_or_default());
                    // Activate only the matching-labeled outgoing edges.
                    if let Some(branch) = chosen.as_deref() {
                        for e in automation.edges.iter().filter(|e| e.from == *node_id) {
                            if e.label.as_deref() == Some(branch) {
                                active.insert((e.from.clone(), e.to.clone()));
                            }
                        }
                    }
                    emit_event(
                        daemon,
                        &automation.id,
                        None,
                        node_id,
                        "Branched",
                        chosen.as_deref(),
                        None,
                    );
                }
                AutomationNodeKind::Map { body_node, .. } => {
                    let items = parse_list(&resolved_input);
                    let mut collected: Vec<String> = Vec::with_capacity(items.len());
                    let Some(body) = automation.nodes.iter().find(|n| n.id == *body_node) else {
                        tracing::warn!(
                            "automation '{}' Map node '{}' references unknown body '{body_node}'",
                            automation.id,
                            node_id
                        );
                        activate_all_outgoing(automation, node_id, &mut active);
                        continue;
                    };
                    emit_event(
                        daemon,
                        &automation.id,
                        None,
                        node_id,
                        "MapStarted",
                        Some(&format!("{} item(s)", items.len())),
                        None,
                    );
                    for (idx, item) in items.iter().enumerate() {
                        // Body sees current item via FromMapItem; FromUpstream
                        // / FromTrigger / Literal / Template all work normally.
                        let body_input =
                            resolve_node_input_with_item(body, trigger_input, &outputs, Some(item));
                        let result: Result<String, DaemonError> = match &body.kind {
                            AutomationNodeKind::Agent { agent_id, .. } => run_agent_node(
                                daemon,
                                &automation.id,
                                &format!("{node_id}#{idx}"),
                                agent_id,
                                &body_input,
                            )
                            .await
                            .map(|o| o.content),
                            AutomationNodeKind::Tool { tool_id, .. } => {
                                run_tool_node(daemon, tool_id, &body_input).await
                            }
                            AutomationNodeKind::Subgraph { automation_id, .. } => {
                                run_subgraph_node(daemon, automation_id, &body_input, depth + 1)
                                    .await
                            }
                            _ => Err(DaemonError::WorkflowExecution(format!(
                                "Map body node '{body_node}' has unsupported kind for iteration"
                            ))),
                        };
                        match result {
                            Ok(out) => collected.push(out),
                            Err(e) => {
                                tracing::warn!("Map iteration {idx} failed: {e}");
                                collected.push(String::new());
                                failed.push((format!("map:{node_id}#{idx}"), e.to_string()));
                            }
                        }
                    }
                    // Output is a JSON array of body results — downstream can
                    // parse via Template or FromUpstream.
                    let arr = serde_json::Value::Array(
                        collected
                            .iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    );
                    outputs.insert(node_id.clone(), arr.to_string());
                    completed.push(format!("map:{node_id} ({} items)", items.len()));
                    activate_all_outgoing(automation, node_id, &mut active);
                    emit_event(
                        daemon,
                        &automation.id,
                        None,
                        node_id,
                        "MapCompleted",
                        Some(&format!("{} item(s)", items.len())),
                        None,
                    );
                }
                AutomationNodeKind::Subgraph { automation_id, .. } => {
                    match run_subgraph_node(daemon, automation_id, &resolved_input, depth + 1).await
                    {
                        Ok(out) => {
                            outputs.insert(node_id.clone(), out);
                            completed.push(format!("subgraph:{automation_id}"));
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                        Err(e) => {
                            failed.push((format!("subgraph:{automation_id}"), e.to_string()));
                            outputs.insert(node_id.clone(), String::new());
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                    }
                }
                AutomationNodeKind::TextInput { default_value, .. } => {
                    // Look up the operator-supplied value for this node id;
                    // fall back to the saved default; finally to empty.
                    let value = text_inputs
                        .get(node_id)
                        .cloned()
                        .or_else(|| default_value.clone())
                        .unwrap_or_default();
                    outputs.insert(node_id.clone(), value);
                    completed.push(format!("input:{node_id}"));
                    activate_all_outgoing(automation, node_id, &mut active);
                    emit_event(
                        daemon,
                        &automation.id,
                        None,
                        node_id,
                        "TaskCompleted",
                        None,
                        None,
                    );
                }
                AutomationNodeKind::Interrupt {
                    resume_strategy, ..
                } => {
                    // Mark the run paused before blocking so the Runs UI shows
                    // status=interrupted; also write a parked-checkpoint so the
                    // current outputs are persisted.
                    let _ = daemon
                        .run_store
                        .mark_interrupted(&automation.id, run_id)
                        .await;
                    write_checkpoint(
                        daemon,
                        &automation.id,
                        run_id,
                        step_idx,
                        node_id,
                        crate::automation_runs::CheckpointEvent::InterruptParked,
                        &outputs,
                        &active,
                    )
                    .await;
                    let pi =
                        park_interrupt(daemon, &automation.id, run_id, node_id, &resolved_input)
                            .await;
                    pi.notify.notified().await;
                    let resume_value = pi.resume_value.lock().await.clone().unwrap_or_default();
                    let final_out = match resume_strategy {
                        ResumeStrategy::Replace => resume_value,
                        ResumeStrategy::Append => format!("{resolved_input}\n\n{resume_value}"),
                    };
                    daemon.pending_interrupts.write().await.remove(&pi.key());
                    outputs.insert(node_id.clone(), final_out);
                    completed.push(format!("interrupt:{node_id}"));
                    activate_all_outgoing(automation, node_id, &mut active);
                    let _ = daemon.run_store.mark_running(&automation.id, run_id).await;
                    emit_event(daemon, &automation.id, None, node_id, "Resumed", None, None);
                    write_checkpoint(
                        daemon,
                        &automation.id,
                        run_id,
                        step_idx,
                        node_id,
                        crate::automation_runs::CheckpointEvent::InterruptResumed,
                        &outputs,
                        &active,
                    )
                    .await;
                    step_idx += 1;
                    continue;
                }
            }

            // Standard checkpoint after Agent / Tool / Conditional / Map / Subgraph.
            // Interrupt has its own checkpointing above (parked + resumed).
            let event = if failed
                .iter()
                .any(|(_, _)| false /* per-node fail tracking */)
            {
                crate::automation_runs::CheckpointEvent::NodeFailed
            } else {
                crate::automation_runs::CheckpointEvent::NodeCompleted
            };
            write_checkpoint(
                daemon,
                &automation.id,
                run_id,
                step_idx,
                node_id,
                event,
                &outputs,
                &active,
            )
            .await;
            step_idx += 1;
        }

        let final_content = agent_outputs
            .last()
            .map(|(_, o)| o.content.clone())
            .unwrap_or_else(|| {
                // If no agent ran, return the last node's output (helps Subgraph
                // composers see something useful).
                outputs.values().last().cloned().unwrap_or_default()
            });

        Ok(WorkflowOutput {
            workflow_id: automation.id.clone(),
            agent_outputs,
            final_content,
            total_token_usage: total_tokens,
            completed_agents: completed,
            failed_agents: failed,
        })
    })
}

/// Resolve a list-like input into discrete items. Tries JSON array first
/// (the standard format produced by Map's own output and many tool calls);
/// falls back to newline-delimited splitting; finally treats the whole
/// thing as a single item.
fn parse_list(s: &str) -> Vec<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return arr
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .collect();
    }
    let lines: Vec<String> = trimmed
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > 1 {
        return lines;
    }
    vec![trimmed.to_string()]
}

/// Map-aware input resolver. When `item` is `Some`, FromMapItem resolves
/// to that string and `{{item}}` in templates substitutes it.
pub fn resolve_node_input_with_item(
    node: &AutomationNode,
    trigger_input: &str,
    upstream: &HashMap<String, String>,
    item: Option<&String>,
) -> String {
    let input = match &node.kind {
        AutomationNodeKind::Agent { input, .. } => input,
        AutomationNodeKind::Tool { input, .. } => input,
        AutomationNodeKind::Conditional { input, .. } => input,
        AutomationNodeKind::Map { input, .. } => input,
        AutomationNodeKind::Subgraph { input, .. } => input,
        AutomationNodeKind::Interrupt { input, .. } => input,
        // TextInput is a source — it doesn't resolve an input, it IS one.
        // Callers should never hit this path.
        AutomationNodeKind::TextInput { default_value, .. } => {
            return default_value.clone().unwrap_or_default();
        }
    };
    match input {
        NodeInput::FromTrigger => trigger_input.to_string(),
        NodeInput::Literal { value } => value.clone(),
        NodeInput::FromUpstream { nodes } => nodes
            .iter()
            .filter_map(|nid| upstream.get(nid).cloned())
            .collect::<Vec<_>>()
            .join("\n\n"),
        NodeInput::Template { template } => {
            let mut out = template.replace("{{trigger}}", trigger_input);
            if let Some(it) = item {
                out = out.replace("{{item}}", it);
            }
            for (id, val) in upstream {
                out = out.replace(&format!("{{{{node:{id}}}}}"), val);
            }
            out
        }
        NodeInput::FromMapItem => item.cloned().unwrap_or_default(),
    }
}

/// Run a Subgraph — recursive call into `execute_automation_inner`.
async fn run_subgraph_node(
    daemon: &AxocoatlDaemon,
    automation_id: &str,
    input: &str,
    depth: usize,
) -> Result<String, DaemonError> {
    let inner = daemon.get_automation(automation_id).await.ok_or_else(|| {
        DaemonError::WorkflowExecution(format!(
            "subgraph references unknown automation '{automation_id}'"
        ))
    })?;
    let run_id = uuid::Uuid::new_v4().to_string();
    let out = execute_automation_inner(daemon, &inner, input, &run_id, depth).await?;
    Ok(out.final_content)
}

/// Park a HITL interrupt and return the handle the caller awaits on.
async fn park_interrupt(
    daemon: &AxocoatlDaemon,
    automation_id: &str,
    run_id: &str,
    node_id: &str,
    message: &str,
) -> PendingInterrupt {
    let pi = PendingInterrupt {
        automation_id: automation_id.to_string(),
        run_id: run_id.to_string(),
        node_id: node_id.to_string(),
        message: message.to_string(),
        payload: serde_json::Value::Null,
        created_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        notify: Arc::new(tokio::sync::Notify::new()),
        resume_value: Arc::new(tokio::sync::Mutex::new(None)),
        cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    daemon
        .pending_interrupts
        .write()
        .await
        .insert(pi.key(), pi.clone());
    emit_event(
        daemon,
        automation_id,
        None,
        node_id,
        "Interrupted",
        Some(message),
        None,
    );
    pi
}

fn activate_all_outgoing(
    automation: &Automation,
    node_id: &str,
    active: &mut HashSet<(String, String)>,
) {
    for e in automation.edges.iter().filter(|e| e.from == *node_id) {
        active.insert((e.from.clone(), e.to.clone()));
    }
}

/// Snapshot the executor's current state to the run store. Called after
/// each node finishes. Non-fatal on error — execution proceeds.
async fn write_checkpoint(
    daemon: &AxocoatlDaemon,
    automation_id: &str,
    run_id: &str,
    step_idx: usize,
    node_id: &str,
    event: crate::automation_runs::CheckpointEvent,
    outputs: &HashMap<String, String>,
    active: &HashSet<(String, String)>,
) {
    let flat: HashSet<String> = active.iter().map(|(a, b)| format!("{a}→{b}")).collect();
    let cp = crate::automation_runs::Checkpoint {
        step_idx,
        node_id: node_id.to_string(),
        event,
        outputs: outputs.clone(),
        active_edges: flat,
        at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    if let Err(e) = daemon.run_store.checkpoint(automation_id, run_id, cp).await {
        tracing::warn!("checkpoint write failed: {e}");
    }
}

fn record_failure(
    failed: &mut Vec<(String, String)>,
    outputs: &mut HashMap<String, String>,
    node_id: &str,
    agent_id: &str,
    e: DaemonError,
) {
    let msg = e.to_string();
    failed.push((agent_id.to_string(), msg.clone()));
    outputs.insert(node_id.to_string(), String::new());
    tracing::warn!(
        "automation node '{}' (agent {}) failed: {}",
        node_id,
        agent_id,
        msg
    );
}

fn pick_branch(
    input: &str,
    branches: &[ConditionalBranch],
    default: Option<&str>,
) -> Option<String> {
    for b in branches {
        if b.when.matches(input) {
            return Some(b.name.clone());
        }
    }
    default.map(|s| s.to_string())
}

/// Compute the actual string prompt for a node by walking its `NodeInput`
/// declaration. Pure function — easy to unit-test. Outside a Map context;
/// for Map body nodes use [`resolve_node_input_with_item`] directly.
pub fn resolve_node_input(
    node: &AutomationNode,
    trigger_input: &str,
    upstream: &HashMap<String, String>,
) -> String {
    resolve_node_input_with_item(node, trigger_input, upstream, None)
}

async fn run_agent_node(
    daemon: &AxocoatlDaemon,
    automation_id: &str,
    node_id: &str,
    agent_id: &str,
    input: &str,
) -> Result<AgentOutput, DaemonError> {
    let actor = daemon
        .agent_registry
        .get(&axocoatl_core::AgentId::new(agent_id))
        .await
        .ok_or_else(|| {
            DaemonError::AgentSpawn(format!(
                "automation '{automation_id}' references unknown agent '{agent_id}'"
            ))
        })?;

    emit_event(
        daemon,
        automation_id,
        Some(agent_id),
        node_id,
        "AgentActivated",
        None,
        None,
    );

    let out = axocoatl_actor::execute_agent(&actor, axocoatl_core::AgentInput::text(input))
        .await
        .map_err(DaemonError::AgentSpawn)?;

    emit_event(
        daemon,
        automation_id,
        Some(agent_id),
        node_id,
        "TaskCompleted",
        Some(&out.content),
        Some(out.token_usage.total() as u64),
    );

    Ok(out)
}

/// Run a registered tool. The node's resolved input is parsed as JSON; if
/// that fails we wrap it as `{"input": "<raw>"}` — common-case ergonomics
/// so users don't have to JSON-encode every literal string.
async fn run_tool_node(
    daemon: &AxocoatlDaemon,
    tool_id: &str,
    input: &str,
) -> Result<String, DaemonError> {
    let args: serde_json::Value =
        serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({ "input": input }));
    let result = daemon
        .tool_executor
        .execute(tool_id, args)
        .await
        .map_err(|e| DaemonError::WorkflowExecution(format!("tool '{tool_id}': {e}")))?;
    Ok(result.to_string())
}

/// Emit a lattice event. `agent_id` populates `frame.agent` so the
/// **Studio** tab's by-agent node graph pulses (Studio's nodes are keyed
/// `agent-<agent_id>`).  `node_id` populates `frame.task` so the
/// **Automation editor** can pulse the matching `autonode-<node_id>` on
/// its own canvas. For non-agent node kinds (Tool/Conditional/etc) pass
/// `None` for `agent_id` — there's no Studio counterpart to pulse.
fn emit_event(
    daemon: &AxocoatlDaemon,
    automation_id: &str,
    agent_id: Option<&str>,
    node_id: &str,
    event_type: &str,
    output: Option<&str>,
    tokens: Option<u64>,
) {
    let _ = daemon.stream_bus.send(crate::stream::StreamFrame::Event {
        event_type: event_type.to_string(),
        agent: agent_id.map(|s| s.to_string()),
        task: Some(node_id.to_string()),
        name: None,
        output: output.map(|s| s.chars().take(200).collect()),
        tokens,
        workflow: Some(automation_id.to_string()),
    });
}

// Marker — BranchExpr is part of the public re-export and used by the
// conditional path. Silences the "unused" warning on the std::sync::Arc
// import that some downstream consumers expected.
#[allow(dead_code)]
fn _branch_expr_referenced(_: &BranchExpr) {}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::{
        AutomationEdge, AutomationNodeKind as Kind, AutomationTrigger, NodeInput,
    };

    fn agent(id: &str, input: NodeInput) -> AutomationNode {
        AutomationNode {
            id: id.into(),
            kind: Kind::Agent {
                agent_id: id.into(),
                input,
            },
            position: None,
        }
    }

    #[test]
    fn from_trigger_returns_the_trigger() {
        let n = agent("a", NodeInput::FromTrigger);
        let map = HashMap::new();
        assert_eq!(resolve_node_input(&n, "hi", &map), "hi");
    }

    #[test]
    fn literal_ignores_trigger() {
        let n = agent(
            "a",
            NodeInput::Literal {
                value: "always this".into(),
            },
        );
        assert_eq!(
            resolve_node_input(&n, "ignored", &HashMap::new()),
            "always this"
        );
    }

    #[test]
    fn from_upstream_joins_named_outputs() {
        let n = agent(
            "a",
            NodeInput::FromUpstream {
                nodes: vec!["b".into(), "c".into()],
            },
        );
        let mut map = HashMap::new();
        map.insert("b".to_string(), "first".to_string());
        map.insert("c".to_string(), "second".to_string());
        map.insert("ignored".to_string(), "nope".to_string());
        assert_eq!(resolve_node_input(&n, "x", &map), "first\n\nsecond");
    }

    #[test]
    fn template_substitutes_trigger_and_nodes() {
        let n = agent(
            "a",
            NodeInput::Template {
                template: "trigger: {{trigger}}\nb said: {{node:b}}".into(),
            },
        );
        let mut map = HashMap::new();
        map.insert("b".to_string(), "hello".to_string());
        assert_eq!(
            resolve_node_input(&n, "do thing", &map),
            "trigger: do thing\nb said: hello"
        );
    }

    #[test]
    fn pick_branch_first_match_wins() {
        let branches = vec![
            ConditionalBranch {
                name: "ok".into(),
                when: BranchExpr::Contains {
                    value: "good".into(),
                },
            },
            ConditionalBranch {
                name: "err".into(),
                when: BranchExpr::Contains {
                    value: "error".into(),
                },
            },
        ];
        assert_eq!(
            pick_branch("all good", &branches, None).as_deref(),
            Some("ok")
        );
        assert_eq!(
            pick_branch("got error", &branches, None).as_deref(),
            Some("err")
        );
        assert_eq!(pick_branch("nothing", &branches, None), None);
        assert_eq!(
            pick_branch("nothing", &branches, Some("default")).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn execution_order_threads_topologically() {
        let auto = Automation {
            id: "x".into(),
            name: "x".into(),
            description: None,
            nodes: vec![
                agent("planner", NodeInput::FromTrigger),
                agent("coder", NodeInput::FromTrigger),
                agent("reviewer", NodeInput::FromTrigger),
            ],
            edges: vec![
                AutomationEdge {
                    from: "planner".into(),
                    to: "coder".into(),
                    label: None,
                },
                AutomationEdge {
                    from: "coder".into(),
                    to: "reviewer".into(),
                    label: None,
                },
            ],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };
        let order = auto.execution_order();
        assert_eq!(
            order,
            vec![
                "planner".to_string(),
                "coder".to_string(),
                "reviewer".to_string()
            ]
        );
    }
}
