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
use crate::workflow::{AgentActivationOutput, WorkflowOutput};

/// Owned, clonable dependencies for an Automation run.
///
/// Server/background callers build this under the daemon's outer `RwLock` and
/// then release that guard before provider or tool execution. Every field is
/// already internally synchronized, so a run never needs to borrow the daemon
/// composition root.
#[derive(Clone)]
pub struct AutomationExecutionContext {
    agent_registry: axocoatl_actor::AgentRegistry,
    automation_store: Arc<tokio::sync::RwLock<crate::automation_store::AutomationStore>>,
    pending_interrupts: Arc<
        tokio::sync::RwLock<std::collections::HashMap<String, crate::interrupt::PendingInterrupt>>,
    >,
    run_store: Arc<crate::automation_runs::AutomationRunStore>,
    tool_executor: Arc<axocoatl_tools::ToolExecutor>,
    stream_bus: crate::stream::StreamBus,
    pub(crate) schedule_table: crate::scheduler::ScheduleTable,
    pub(crate) proactive_table: crate::proactive::ProactiveTable,
}

impl AutomationExecutionContext {
    pub fn from_daemon(daemon: &AxocoatlDaemon) -> Self {
        Self {
            agent_registry: daemon.agent_registry.clone(),
            automation_store: daemon.automation_store.clone(),
            pending_interrupts: daemon.pending_interrupts.clone(),
            run_store: daemon.run_store.clone(),
            tool_executor: daemon.tool_executor.clone(),
            stream_bus: daemon.stream_bus.clone(),
            schedule_table: daemon.schedule_table.clone(),
            proactive_table: daemon.proactive_table.clone(),
        }
    }

    pub async fn get_automation(&self, id: &str) -> Option<Automation> {
        self.automation_store.read().await.get(id)
    }
}

/// Whether resolving an Interrupt woke the original executor or started a
/// continuation reconstructed from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptResolution {
    Live,
    Recovered,
}

/// Rebuild the operator-visible pending list from durable run checkpoints.
/// Invalid or legacy-incompatible entries remain in run history but are not
/// advertised as resumable.
pub async fn rehydrate_pending_interrupts(
    automation_store: &Arc<tokio::sync::RwLock<crate::automation_store::AutomationStore>>,
    run_store: &Arc<crate::automation_runs::AutomationRunStore>,
) -> HashMap<String, PendingInterrupt> {
    let current: HashMap<String, Automation> = automation_store
        .read()
        .await
        .list()
        .into_iter()
        .map(|automation| (automation.id.clone(), automation))
        .collect();
    let runs = match run_store.list_all().await {
        Ok(runs) => runs,
        Err(error) => {
            tracing::warn!(error = %error, "could not scan Automation runs for pending Interrupts");
            return HashMap::new();
        }
    };

    let mut pending = HashMap::new();
    for run in runs {
        if run.status != crate::automation_runs::RunStatus::Interrupted {
            continue;
        }
        let Some(checkpoint) = run.checkpoints.last() else {
            tracing::warn!(automation = %run.automation_id, run = %run.run_id, "interrupted Automation run has no checkpoint");
            continue;
        };
        if checkpoint.event != crate::automation_runs::CheckpointEvent::InterruptParked {
            tracing::warn!(automation = %run.automation_id, run = %run.run_id, event = ?checkpoint.event, "interrupted Automation run is not parked at its latest checkpoint");
            continue;
        }
        let Some(automation) = run
            .automation_snapshot
            .as_ref()
            .or_else(|| current.get(&run.automation_id))
        else {
            tracing::warn!(automation = %run.automation_id, run = %run.run_id, "legacy interrupted run has no current Automation to recover from");
            continue;
        };
        if automation.id != run.automation_id {
            tracing::warn!(automation = %run.automation_id, run = %run.run_id, "interrupted run's Automation snapshot has a mismatched id");
            continue;
        }
        let Some(node) = automation
            .nodes
            .iter()
            .find(|node| node.id == checkpoint.node_id)
        else {
            tracing::warn!(automation = %run.automation_id, run = %run.run_id, node = %checkpoint.node_id, "parked Interrupt no longer exists");
            continue;
        };
        if !matches!(&node.kind, AutomationNodeKind::Interrupt { .. }) {
            tracing::warn!(automation = %run.automation_id, run = %run.run_id, node = %checkpoint.node_id, "parked node is no longer an Interrupt");
            continue;
        }
        let message = resolve_node_input(node, &run.trigger_input, &checkpoint.outputs);
        let interrupt = PendingInterrupt {
            automation_id: run.automation_id.clone(),
            run_id: run.run_id.clone(),
            node_id: checkpoint.node_id.clone(),
            message,
            payload: serde_json::Value::Null,
            created_at_unix: checkpoint.at_unix,
            notify: Arc::new(tokio::sync::Notify::new()),
            resume_value: Arc::new(tokio::sync::Mutex::new(None)),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recovered: true,
        };
        pending.insert(interrupt.key(), interrupt);
    }
    pending
}

/// Resolve a pending Interrupt. Live runs receive a notification; rehydrated
/// runs atomically claim the entry and continue from the parked checkpoint.
pub async fn resolve_pending_interrupt(
    context: &AutomationExecutionContext,
    automation_id: &str,
    run_id: &str,
    node_id: &str,
    value: String,
    cancelled: bool,
) -> Result<InterruptResolution, crate::interrupt::InterruptResolutionError> {
    use crate::interrupt::InterruptResolutionError;

    let key = format!("{automation_id}:{run_id}:{node_id}");
    let pending = context
        .pending_interrupts
        .read()
        .await
        .get(&key)
        .cloned()
        .ok_or_else(|| InterruptResolutionError::NotFound(key.clone()))?;

    // Resolve and validate every durable dependency before claiming the entry,
    // so malformed legacy state remains visible instead of disappearing.
    let recovered = if pending.recovered {
        let run = context
            .run_store
            .load(automation_id, run_id)
            .map_err(|error| InterruptResolutionError::Recovery {
                key: key.clone(),
                reason: error.to_string(),
            })?;
        let automation = match run.automation_snapshot.clone() {
            Some(automation) => automation,
            None => context.get_automation(automation_id).await.ok_or_else(|| {
                InterruptResolutionError::Recovery {
                    key: key.clone(),
                    reason: "legacy run has no Automation snapshot and the current Automation is missing"
                        .to_string(),
                }
            })?,
        };
        let checkpoint = run
            .checkpoints
            .last()
            .cloned()
            .filter(|checkpoint| {
                checkpoint.node_id == node_id
                    && checkpoint.event == crate::automation_runs::CheckpointEvent::InterruptParked
            })
            .ok_or_else(|| InterruptResolutionError::Recovery {
                key: key.clone(),
                reason: "latest checkpoint is not the requested parked Interrupt".to_string(),
            })?;
        let valid_node = automation.nodes.iter().any(|node| {
            node.id == node_id && matches!(&node.kind, AutomationNodeKind::Interrupt { .. })
        });
        if automation.id != automation_id || !valid_node {
            return Err(InterruptResolutionError::Recovery {
                key,
                reason: "Automation snapshot does not contain the requested Interrupt".to_string(),
            });
        }
        Some((run, automation, checkpoint))
    } else {
        None
    };

    let pending = context
        .pending_interrupts
        .write()
        .await
        .remove(&key)
        .ok_or_else(|| InterruptResolutionError::NotFound(key.clone()))?;
    if let Some((run, automation, checkpoint)) = recovered {
        let context = context.clone();
        tokio::spawn(async move {
            let had_recorded_failure = run.checkpoints.iter().any(|checkpoint| {
                checkpoint.event == crate::automation_runs::CheckpointEvent::NodeFailed
            });
            let result = execute_automation_from_state(
                &context,
                &automation,
                &run.trigger_input,
                &run.text_inputs,
                &run.run_id,
                0,
                Some(RecoveredInterruptState {
                    checkpoint,
                    resume_value: value,
                    cancelled,
                }),
            )
            .await;
            let status = final_run_status(&result, had_recorded_failure);
            let final_content = result
                .as_ref()
                .ok()
                .map(|output| output.final_content.clone());
            if let Err(error) = context
                .run_store
                .finish_with_content(&automation.id, &run.run_id, status, final_content)
                .await
            {
                tracing::warn!(automation = %automation.id, run = %run.run_id, error = %error, "could not finalize recovered Automation run");
            }
            crate::automation_runtime::record_automation_outcome(&context, &automation, &result);
            if let Err(error) = result {
                tracing::warn!(automation = %automation.id, run = %run.run_id, error = %error, "recovered Automation continuation failed");
            }
        });
        Ok(InterruptResolution::Recovered)
    } else {
        if cancelled {
            pending
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        *pending.resume_value.lock().await = Some(value);
        pending.notify.notify_one();
        Ok(InterruptResolution::Live)
    }
}

/// Cap on Subgraph recursion depth so a misconfigured automation
/// (A calls B calls A) doesn't blow the stack or hang forever.
const MAX_SUBGRAPH_DEPTH: usize = 8;

fn final_run_status(
    result: &Result<WorkflowOutput, DaemonError>,
    had_recorded_failure: bool,
) -> crate::automation_runs::RunStatus {
    match result {
        Err(_) => crate::automation_runs::RunStatus::Failed,
        Ok(output) if had_recorded_failure || !output.failed_agents.is_empty() => {
            crate::automation_runs::RunStatus::Failed
        }
        Ok(_) => crate::automation_runs::RunStatus::Completed,
    }
}

/// Run an automation end-to-end. Returns a `WorkflowOutput` so existing
/// compatibility and `/api/automations/{id}/run` callers keep their existing
/// return-type expectations.
pub async fn execute_automation(
    daemon: &AxocoatlDaemon,
    automation: &Automation,
    trigger_input: &str,
) -> Result<WorkflowOutput, DaemonError> {
    let context = AutomationExecutionContext::from_daemon(daemon);
    execute_automation_in_context(&context, automation, trigger_input).await
}

/// Run with an owned execution context. Callers holding the daemon's outer
/// state lock must prefer this function and drop their guard first.
pub async fn execute_automation_in_context(
    context: &AutomationExecutionContext,
    automation: &Automation,
    trigger_input: &str,
) -> Result<WorkflowOutput, DaemonError> {
    execute_automation_with_inputs_in_context(context, automation, trigger_input, &HashMap::new())
        .await
}

/// Run an automation with explicit per-node TextInput values. Used by the
/// dashboard's run-form modal; string-input callers use `execute_automation`.
pub async fn execute_automation_with_inputs(
    daemon: &AxocoatlDaemon,
    automation: &Automation,
    trigger_input: &str,
    text_inputs: &HashMap<String, String>,
) -> Result<WorkflowOutput, DaemonError> {
    let context = AutomationExecutionContext::from_daemon(daemon);
    execute_automation_with_inputs_in_context(&context, automation, trigger_input, text_inputs)
        .await
}

pub async fn execute_automation_with_inputs_in_context(
    daemon: &AutomationExecutionContext,
    automation: &Automation,
    trigger_input: &str,
    text_inputs: &HashMap<String, String>,
) -> Result<WorkflowOutput, DaemonError> {
    let run_id =
        start_automation_run_in_context(daemon, automation, trigger_input, text_inputs, None)
            .await?;
    execute_started_automation_run_in_context(
        daemon,
        automation,
        trigger_input,
        text_inputs,
        &run_id,
    )
    .await
}

/// Persist the immutable inputs and ancestry for a run before any provider or
/// tool side effect begins. HTTP callers use this boundary so a successful
/// "started" response always names a durable run record.
pub async fn start_automation_run_in_context(
    daemon: &AutomationExecutionContext,
    automation: &Automation,
    trigger_input: &str,
    text_inputs: &HashMap<String, String>,
    forked_from: Option<crate::automation_runs::ForkSource>,
) -> Result<String, DaemonError> {
    let run_id = uuid::Uuid::new_v4().to_string();
    daemon
        .run_store
        .start_for_automation(automation, &run_id, trigger_input, text_inputs, forked_from)
        .await
        .map_err(|error| {
            DaemonError::WorkflowExecution(format!(
                "could not persist Automation run '{run_id}' before execution: {error}"
            ))
        })?;
    Ok(run_id)
}

/// Execute and finalize a run whose durable start record already exists.
/// Keeping this separate lets an immediate HTTP endpoint persist identity and
/// ancestry synchronously, then move only the actual work into a background
/// task.
pub async fn execute_started_automation_run_in_context(
    daemon: &AutomationExecutionContext,
    automation: &Automation,
    trigger_input: &str,
    text_inputs: &HashMap<String, String>,
    run_id: &str,
) -> Result<WorkflowOutput, DaemonError> {
    let result = execute_automation_inner_with_inputs(
        daemon,
        automation,
        trigger_input,
        text_inputs,
        run_id,
        0,
    )
    .await;
    let status = final_run_status(&result, false);
    let final_content = result
        .as_ref()
        .ok()
        .map(|output| output.final_content.clone());
    if let Err(e) = daemon
        .run_store
        .finish_with_content(&automation.id, run_id, status, final_content)
        .await
    {
        tracing::warn!("could not finalize run: {e}");
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub fn execute_automation_inner<'a>(
    daemon: &'a AutomationExecutionContext,
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
    daemon: &'a AutomationExecutionContext,
    automation: &'a Automation,
    trigger_input: &'a str,
    text_inputs: &'a HashMap<String, String>,
    run_id: &'a str,
    depth: usize,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<WorkflowOutput, DaemonError>> + Send + 'a>,
> {
    execute_automation_from_state(
        daemon,
        automation,
        trigger_input,
        text_inputs,
        run_id,
        depth,
        None,
    )
}

#[derive(Debug, Clone)]
struct RecoveredInterruptState {
    checkpoint: crate::automation_runs::Checkpoint,
    resume_value: String,
    cancelled: bool,
}

#[derive(Debug, Clone)]
struct WorkflowProgress {
    agent_outputs: Vec<(String, AgentOutput)>,
    agent_activations: Vec<AgentActivationOutput>,
    completed_agents: Vec<String>,
    failed_agents: Vec<(String, String)>,
    total_token_usage: TokenUsageStats,
    token_usage_known: bool,
}

impl Default for WorkflowProgress {
    fn default() -> Self {
        Self {
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            completed_agents: Vec::new(),
            failed_agents: Vec::new(),
            total_token_usage: TokenUsageStats::default(),
            // A run that never dispatches a provider is exactly known zero.
            token_usage_known: true,
        }
    }
}

impl WorkflowProgress {
    fn from_checkpoint(checkpoint: &crate::automation_runs::Checkpoint) -> Self {
        Self {
            agent_outputs: checkpoint.agent_outputs.clone(),
            agent_activations: checkpoint.agent_activations.clone(),
            completed_agents: checkpoint.completed_agents.clone(),
            failed_agents: checkpoint.failed_agents.clone(),
            total_token_usage: checkpoint.total_token_usage.clone(),
            token_usage_known: checkpoint.token_usage_known,
        }
    }

    fn record_agent_success(
        &mut self,
        activation_id: String,
        agent_id: &str,
        output: AgentOutput,
        token_usage_known: bool,
    ) {
        self.total_token_usage.merge(&output.token_usage);
        self.token_usage_known &= token_usage_known;
        self.agent_outputs
            .push((agent_id.to_string(), output.clone()));
        self.agent_activations.push(AgentActivationOutput {
            activation_id,
            agent_id: agent_id.to_string(),
            output,
        });
    }

    fn merge_workflow(&mut self, prefix: &str, output: WorkflowOutput) {
        self.total_token_usage.merge(&output.total_token_usage);
        self.token_usage_known &= output.token_usage_known;
        self.agent_outputs.extend(output.agent_outputs);
        self.agent_activations
            .extend(output.agent_activations.into_iter().map(|mut activation| {
                activation.activation_id = format!("{prefix}/{}", activation.activation_id);
                activation
            }));
        self.completed_agents.extend(
            output
                .completed_agents
                .into_iter()
                .map(|identity| format!("{prefix}/{identity}")),
        );
        self.failed_agents.extend(
            output
                .failed_agents
                .into_iter()
                .map(|(identity, error)| (format!("{prefix}/{identity}"), error)),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_automation_from_state<'a>(
    daemon: &'a AutomationExecutionContext,
    automation: &'a Automation,
    trigger_input: &'a str,
    text_inputs: &'a HashMap<String, String>,
    run_id: &'a str,
    depth: usize,
    recovered: Option<RecoveredInterruptState>,
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

        // Restore the executor's durable state when this is a continuation.
        // Completed nodes are represented by output keys; active edges retain
        // the exact branch decisions already made before the Interrupt.
        let mut progress = recovered
            .as_ref()
            .map(|state| WorkflowProgress::from_checkpoint(&state.checkpoint))
            .unwrap_or_default();
        let mut active: HashSet<(String, String)> = match recovered.as_ref() {
            Some(state) => restore_active_edges(&state.checkpoint.active_edges)
                .map_err(|error| workflow_error_with_progress(error, &progress))?,
            None => HashSet::new(),
        };
        let mut outputs: HashMap<String, String> = recovered
            .as_ref()
            .map(|state| state.checkpoint.outputs.clone())
            .unwrap_or_default();
        let mut step_idx: usize = recovered
            .as_ref()
            .map(|state| state.checkpoint.step_idx)
            .unwrap_or(0);

        // Complete the already-parked Interrupt before walking the remaining
        // graph. This is independent of topological ordering (which may place
        // unrelated roots differently in a new process) and makes the node's
        // outgoing edge available to its downstream continuation.
        if let Some(state) = recovered {
            let node_id = state.checkpoint.node_id;
            let node = automation
                .nodes
                .iter()
                .find(|node| node.id == node_id)
                .ok_or_else(|| {
                    workflow_error_with_progress(
                        DaemonError::WorkflowExecution(format!(
                            "run '{run_id}' is parked at missing interrupt '{node_id}'"
                        )),
                        &progress,
                    )
                })?;
            let AutomationNodeKind::Interrupt {
                resume_strategy, ..
            } = &node.kind
            else {
                return Err(workflow_error_with_progress(
                    DaemonError::WorkflowExecution(format!(
                        "run '{run_id}' is parked at node '{node_id}', which is no longer an Interrupt"
                    )),
                    &progress,
                ));
            };
            let resolved_input = resolve_node_input(node, trigger_input, &outputs);
            let final_out = if state.cancelled {
                String::new()
            } else {
                match resume_strategy {
                    ResumeStrategy::Replace => state.resume_value,
                    ResumeStrategy::Append => {
                        format!("{resolved_input}\n\n{}", state.resume_value)
                    }
                }
            };
            outputs.insert(node_id.clone(), final_out);
            progress
                .completed_agents
                .push(format!("interrupt:{node_id}"));
            activate_all_outgoing(automation, &node_id, &mut active);
            transition_with_checkpoint(
                daemon,
                &automation.id,
                run_id,
                crate::automation_runs::RunStatus::Running,
                step_idx,
                &node_id,
                crate::automation_runs::CheckpointEvent::InterruptResumed,
                &outputs,
                &active,
                &progress,
            )
            .await
            .map_err(|error| workflow_error_with_progress(error, &progress))?;
            emit_event(
                daemon,
                &automation.id,
                None,
                &node_id,
                if state.cancelled {
                    "Cancelled"
                } else {
                    "Resumed"
                },
                None,
                None,
            );
            step_idx += 1;
        }

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

            // A parked checkpoint contains every node that completed before
            // the Interrupt. Do not replay providers, tools, or side effects.
            if outputs.contains_key(node_id) {
                continue;
            }

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
            let failure_count_before_node = progress.failed_agents.len();

            match &node.kind {
                AutomationNodeKind::Agent { agent_id, .. } => {
                    match run_agent_node(daemon, &automation.id, node_id, agent_id, &resolved_input)
                        .await
                    {
                        Ok(measured) => {
                            outputs.insert(node_id.clone(), measured.output.content.clone());
                            progress.record_agent_success(
                                node_id.clone(),
                                agent_id,
                                measured.output,
                                measured.token_usage_known,
                            );
                            progress.completed_agents.push(agent_id.clone());
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                        Err(e) => {
                            record_failure(&mut progress, &mut outputs, node_id, agent_id, e);
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
                            progress.completed_agents.push(format!("tool:{tool_id}"));
                            activate_all_outgoing(automation, node_id, &mut active);
                            // Preserve a TaskCompleted-style observability frame
                            // for compatibility consumers of Automation events.
                            emit_event(
                                daemon,
                                &automation.id,
                                None, // tool nodes have no agent identity
                                node_id,
                                "TaskCompleted",
                                Some(&output_str),
                                None,
                            );
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            progress
                                .failed_agents
                                .push((format!("tool:{tool_id}"), msg.clone()));
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
                        let message = format!(
                            "automation '{}' Map node '{}' references unknown body '{body_node}'",
                            automation.id, node_id
                        );
                        tracing::warn!("{message}");
                        progress
                            .failed_agents
                            .push((format!("map:{node_id}"), message.clone()));
                        outputs.insert(node_id.clone(), "[]".to_string());
                        activate_all_outgoing(automation, node_id, &mut active);
                        emit_event(
                            daemon,
                            &automation.id,
                            None,
                            node_id,
                            "NodeFailed",
                            Some(&message),
                            None,
                        );
                        write_checkpoint(
                            daemon,
                            &automation.id,
                            run_id,
                            step_idx,
                            node_id,
                            crate::automation_runs::CheckpointEvent::NodeFailed,
                            Some(&message),
                            &outputs,
                            &active,
                            &progress,
                        )
                        .await;
                        step_idx += 1;
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
                        let activation_id = format!("{node_id}#{idx}");
                        match &body.kind {
                            AutomationNodeKind::Agent { agent_id, .. } => {
                                match run_agent_node(
                                    daemon,
                                    &automation.id,
                                    &activation_id,
                                    agent_id,
                                    &body_input,
                                )
                                .await
                                {
                                    Ok(measured) => {
                                        collected.push(measured.output.content.clone());
                                        progress.record_agent_success(
                                            activation_id.clone(),
                                            agent_id,
                                            measured.output,
                                            measured.token_usage_known,
                                        );
                                        progress
                                            .completed_agents
                                            .push(format!("{activation_id}/{agent_id}"));
                                    }
                                    Err(error) => {
                                        tracing::warn!("Map iteration {idx} failed: {error}");
                                        collected.push(String::new());
                                        record_failure(
                                            &mut progress,
                                            &mut outputs,
                                            &activation_id,
                                            &format!("map:{activation_id}/{agent_id}"),
                                            error,
                                        );
                                    }
                                }
                            }
                            AutomationNodeKind::Tool { tool_id, .. } => {
                                match run_tool_node(daemon, tool_id, &body_input).await {
                                    Ok(output) => collected.push(output),
                                    Err(error) => {
                                        tracing::warn!("Map iteration {idx} failed: {error}");
                                        collected.push(String::new());
                                        merge_nested_workflow_failure(&mut progress, &error);
                                        progress.failed_agents.push((
                                            format!("map:{activation_id}/tool:{tool_id}"),
                                            error.to_string(),
                                        ));
                                    }
                                }
                            }
                            AutomationNodeKind::Subgraph { automation_id, .. } => {
                                match run_subgraph_node(
                                    daemon,
                                    automation_id,
                                    &body_input,
                                    depth + 1,
                                )
                                .await
                                {
                                    Ok(output) => {
                                        let failed = !output.failed_agents.is_empty();
                                        let content = output.final_content.clone();
                                        progress.merge_workflow(
                                            &format!("{activation_id}/subgraph:{automation_id}"),
                                            output,
                                        );
                                        collected.push(if failed {
                                            String::new()
                                        } else {
                                            content
                                        });
                                    }
                                    Err(error) => {
                                        tracing::warn!("Map iteration {idx} failed: {error}");
                                        collected.push(String::new());
                                        merge_nested_workflow_failure(&mut progress, &error);
                                        progress.failed_agents.push((
                                            format!("map:{activation_id}/subgraph:{automation_id}"),
                                            error.to_string(),
                                        ));
                                    }
                                }
                            }
                            _ => {
                                let error = DaemonError::WorkflowExecution(format!(
                                    "Map body node '{body_node}' has unsupported kind for iteration"
                                ));
                                collected.push(String::new());
                                progress
                                    .failed_agents
                                    .push((format!("map:{activation_id}"), error.to_string()));
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
                    progress
                        .completed_agents
                        .push(format!("map:{node_id} ({} items)", items.len()));
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
                            let failed = !out.failed_agents.is_empty();
                            let content = out.final_content.clone();
                            progress.merge_workflow(
                                &format!("{node_id}/subgraph:{automation_id}"),
                                out,
                            );
                            outputs.insert(
                                node_id.clone(),
                                if failed { String::new() } else { content },
                            );
                            if !failed {
                                progress
                                    .completed_agents
                                    .push(format!("subgraph:{automation_id}"));
                            }
                            activate_all_outgoing(automation, node_id, &mut active);
                        }
                        Err(e) => {
                            merge_nested_workflow_failure(&mut progress, &e);
                            progress
                                .failed_agents
                                .push((format!("subgraph:{automation_id}"), e.to_string()));
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
                    progress.completed_agents.push(format!("input:{node_id}"));
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
                    // Persist the complete continuation state before exposing
                    // the interrupt. A new daemon can reconstruct the pending
                    // prompt from this checkpoint without replaying prior work.
                    let parked = transition_with_checkpoint(
                        daemon,
                        &automation.id,
                        run_id,
                        crate::automation_runs::RunStatus::Interrupted,
                        step_idx,
                        node_id,
                        crate::automation_runs::CheckpointEvent::InterruptParked,
                        &outputs,
                        &active,
                        &progress,
                    )
                    .await;
                    if let Err(error) = parked {
                        if depth == 0 {
                            return Err(workflow_error_with_progress(error, &progress));
                        }
                        // Subgraphs currently execute inside their parent's
                        // run without their own Run record. Preserve their live
                        // HITL behavior, but do not claim restart durability.
                        tracing::warn!(automation = %automation.id, run = %run_id, error = %error, "nested Automation Interrupt is process-local");
                    }
                    let pi =
                        park_interrupt(daemon, &automation.id, run_id, node_id, &resolved_input)
                            .await;
                    pi.notify.notified().await;
                    let cancelled = pi.cancelled.load(std::sync::atomic::Ordering::SeqCst);
                    let resume_value = pi.resume_value.lock().await.clone().unwrap_or_default();
                    // A cancelled interrupt carries no operator input — the
                    // node's output is empty and the run proceeds, but we surface
                    // it as a distinct "Cancelled" event, not a normal resume.
                    let final_out = if cancelled {
                        String::new()
                    } else {
                        match resume_strategy {
                            ResumeStrategy::Replace => resume_value,
                            ResumeStrategy::Append => format!("{resolved_input}\n\n{resume_value}"),
                        }
                    };
                    daemon
                        .pending_interrupts
                        .write()
                        .await
                        .remove(&format!("{}:{}:{}", automation.id, run_id, node_id));
                    outputs.insert(node_id.clone(), final_out);
                    progress
                        .completed_agents
                        .push(format!("interrupt:{node_id}"));
                    activate_all_outgoing(automation, node_id, &mut active);
                    let resumed = transition_with_checkpoint(
                        daemon,
                        &automation.id,
                        run_id,
                        crate::automation_runs::RunStatus::Running,
                        step_idx,
                        node_id,
                        crate::automation_runs::CheckpointEvent::InterruptResumed,
                        &outputs,
                        &active,
                        &progress,
                    )
                    .await;
                    if let Err(error) = resumed {
                        if depth == 0 {
                            return Err(workflow_error_with_progress(error, &progress));
                        }
                        tracing::warn!(automation = %automation.id, run = %run_id, error = %error, "nested Automation Interrupt resume is process-local");
                    }
                    emit_event(
                        daemon,
                        &automation.id,
                        None,
                        node_id,
                        if cancelled { "Cancelled" } else { "Resumed" },
                        None,
                        None,
                    );
                    step_idx += 1;
                    continue;
                }
            }

            // Standard checkpoint after Agent / Tool / Conditional / Map / Subgraph.
            // Interrupt has its own checkpointing above (parked + resumed).
            let node_failures = &progress.failed_agents[failure_count_before_node..];
            let (event, failure_detail) = if node_failures.is_empty() {
                (crate::automation_runs::CheckpointEvent::NodeCompleted, None)
            } else {
                let failure_detail = node_failures
                    .iter()
                    .map(|(subject, error)| format!("{subject}: {error}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let agent_id = match &node.kind {
                    AutomationNodeKind::Agent { agent_id, .. } => Some(agent_id.as_str()),
                    _ => None,
                };
                emit_event(
                    daemon,
                    &automation.id,
                    agent_id,
                    node_id,
                    "NodeFailed",
                    Some(&failure_detail),
                    None,
                );
                (
                    crate::automation_runs::CheckpointEvent::NodeFailed,
                    Some(failure_detail),
                )
            };
            write_checkpoint(
                daemon,
                &automation.id,
                run_id,
                step_idx,
                node_id,
                event,
                failure_detail.as_deref(),
                &outputs,
                &active,
                &progress,
            )
            .await;
            step_idx += 1;
        }

        let final_content = terminal_output(automation, &outputs, &active);

        Ok(WorkflowOutput {
            workflow_id: automation.id.clone(),
            agent_outputs: progress.agent_outputs,
            agent_activations: progress.agent_activations,
            final_content,
            total_token_usage: progress.total_token_usage,
            token_usage_known: progress.token_usage_known,
            completed_agents: progress.completed_agents,
            failed_agents: progress.failed_agents,
        })
    })
}

fn workflow_error_with_progress(error: DaemonError, progress: &WorkflowProgress) -> DaemonError {
    DaemonError::workflow_execution_measured(
        error,
        progress.total_token_usage.clone(),
        progress.token_usage_known,
    )
}

fn merge_nested_workflow_failure(progress: &mut WorkflowProgress, error: &DaemonError) {
    if let Some((usage, known)) = error.workflow_token_usage() {
        progress.total_token_usage.merge(usage);
        progress.token_usage_known &= known;
    }
}

/// Return the outputs of the nodes where this execution actually terminated.
///
/// A runtime sink is an executed node with no activated edge to another
/// executed node. This differs from "last agent": a Tool, Map, Subgraph,
/// TextInput, or Conditional can be the terminal step, and an unselected
/// Conditional edge does not make its source non-terminal. Multiple sinks are
/// concatenated in Automation declaration order so disconnected DAGs retain
/// every terminal result without depending on `HashMap` or topological-queue
/// iteration order.
fn terminal_output(
    automation: &Automation,
    outputs: &HashMap<String, String>,
    active: &HashSet<(String, String)>,
) -> String {
    automation
        .nodes
        .iter()
        .filter_map(|node| {
            let output = outputs.get(&node.id)?;
            let has_executed_successor = active
                .iter()
                .any(|(from, to)| from == &node.id && outputs.contains_key(to));
            (!has_executed_successor).then_some(output.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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
    daemon: &AutomationExecutionContext,
    automation_id: &str,
    input: &str,
    depth: usize,
) -> Result<WorkflowOutput, DaemonError> {
    let inner = daemon.get_automation(automation_id).await.ok_or_else(|| {
        DaemonError::WorkflowExecution(format!(
            "subgraph references unknown automation '{automation_id}'"
        ))
    })?;
    let run_id = uuid::Uuid::new_v4().to_string();
    execute_automation_inner(daemon, &inner, input, &run_id, depth).await
}

/// Park a HITL interrupt and return the handle the caller awaits on.
async fn park_interrupt(
    daemon: &AutomationExecutionContext,
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
        recovered: false,
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

fn restore_active_edges(
    flattened: &HashSet<String>,
) -> Result<HashSet<(String, String)>, DaemonError> {
    flattened
        .iter()
        .map(|edge| {
            edge.split_once('→')
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .ok_or_else(|| {
                    DaemonError::WorkflowExecution(format!(
                        "invalid active edge in Automation checkpoint: '{edge}'"
                    ))
                })
        })
        .collect()
}

/// Snapshot the executor's current state to the run store. Called after
/// each node finishes. Non-fatal on error — execution proceeds.
///
/// The arguments are cohesive — the checkpoint destination (`daemon`,
/// `automation_id`, `run_id`) plus the state to snapshot — so a context struct
/// would just thread the same run-scoped values through the executor loop for
/// no real gain. `clippy::too_many_arguments` is suppressed deliberately.
#[allow(clippy::too_many_arguments)]
async fn write_checkpoint(
    daemon: &AutomationExecutionContext,
    automation_id: &str,
    run_id: &str,
    step_idx: usize,
    node_id: &str,
    event: crate::automation_runs::CheckpointEvent,
    failure_detail: Option<&str>,
    outputs: &HashMap<String, String>,
    active: &HashSet<(String, String)>,
    progress: &WorkflowProgress,
) {
    if let Err(error) = write_checkpoint_durable(
        daemon,
        automation_id,
        run_id,
        step_idx,
        node_id,
        event,
        failure_detail,
        outputs,
        active,
        progress,
    )
    .await
    {
        tracing::warn!("checkpoint write failed: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_checkpoint_durable(
    daemon: &AutomationExecutionContext,
    automation_id: &str,
    run_id: &str,
    step_idx: usize,
    node_id: &str,
    event: crate::automation_runs::CheckpointEvent,
    failure_detail: Option<&str>,
    outputs: &HashMap<String, String>,
    active: &HashSet<(String, String)>,
    progress: &WorkflowProgress,
) -> Result<(), DaemonError> {
    let checkpoint = checkpoint_snapshot(
        step_idx,
        node_id,
        event,
        failure_detail,
        outputs,
        active,
        progress,
    );
    daemon
        .run_store
        .checkpoint(automation_id, run_id, checkpoint)
        .await
        .map_err(|error| {
            DaemonError::WorkflowExecution(format!(
                "could not persist checkpoint for run '{run_id}': {error}"
            ))
        })
}

#[allow(clippy::too_many_arguments)]
async fn transition_with_checkpoint(
    daemon: &AutomationExecutionContext,
    automation_id: &str,
    run_id: &str,
    status: crate::automation_runs::RunStatus,
    step_idx: usize,
    node_id: &str,
    event: crate::automation_runs::CheckpointEvent,
    outputs: &HashMap<String, String>,
    active: &HashSet<(String, String)>,
    progress: &WorkflowProgress,
) -> Result<(), DaemonError> {
    let checkpoint = checkpoint_snapshot(step_idx, node_id, event, None, outputs, active, progress);
    daemon
        .run_store
        .transition_with_checkpoint(automation_id, run_id, status, checkpoint)
        .await
        .map_err(|error| {
            DaemonError::WorkflowExecution(format!(
                "could not persist state transition for run '{run_id}': {error}"
            ))
        })
}

fn checkpoint_snapshot(
    step_idx: usize,
    node_id: &str,
    event: crate::automation_runs::CheckpointEvent,
    failure_detail: Option<&str>,
    outputs: &HashMap<String, String>,
    active: &HashSet<(String, String)>,
    progress: &WorkflowProgress,
) -> crate::automation_runs::Checkpoint {
    let flat: HashSet<String> = active.iter().map(|(a, b)| format!("{a}→{b}")).collect();
    crate::automation_runs::Checkpoint {
        step_idx,
        node_id: node_id.to_string(),
        event,
        failure_detail: failure_detail.map(str::to_string),
        outputs: outputs.clone(),
        active_edges: flat,
        agent_outputs: progress.agent_outputs.clone(),
        agent_activations: progress.agent_activations.clone(),
        completed_agents: progress.completed_agents.clone(),
        failed_agents: progress.failed_agents.clone(),
        total_token_usage: progress.total_token_usage.clone(),
        token_usage_known: progress.token_usage_known,
        at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

fn record_failure(
    progress: &mut WorkflowProgress,
    outputs: &mut HashMap<String, String>,
    node_id: &str,
    agent_id: &str,
    failure: AutomationAgentFailure,
) {
    progress.total_token_usage.merge(&failure.token_usage);
    progress.token_usage_known &= failure.token_usage_known;
    let msg = failure.error.to_string();
    progress
        .failed_agents
        .push((agent_id.to_string(), msg.clone()));
    outputs.insert(node_id.to_string(), String::new());
    tracing::warn!(
        "automation node '{}' (agent {}) failed: {}",
        node_id,
        agent_id,
        msg
    );
}

#[derive(Debug)]
struct MeasuredAutomationAgentOutput {
    output: AgentOutput,
    token_usage_known: bool,
}

#[derive(Debug)]
struct AutomationAgentFailure {
    error: DaemonError,
    token_usage: TokenUsageStats,
    token_usage_known: bool,
}

impl AutomationAgentFailure {
    fn before_dispatch(error: DaemonError) -> Self {
        Self {
            error,
            token_usage: TokenUsageStats::default(),
            token_usage_known: true,
        }
    }

    fn from_execution(error: axocoatl_actor::AgentExecutionFailure) -> Self {
        let message = error.to_string();
        let token_usage = error.token_usage;
        Self {
            error: DaemonError::AgentSpawn(message),
            token_usage: token_usage.usage,
            token_usage_known: token_usage.complete,
        }
    }
}

impl std::fmt::Display for AutomationAgentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
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
    daemon: &AutomationExecutionContext,
    automation_id: &str,
    node_id: &str,
    agent_id: &str,
    input: &str,
) -> Result<MeasuredAutomationAgentOutput, AutomationAgentFailure> {
    let actor = daemon
        .agent_registry
        .get(&axocoatl_core::AgentId::new(agent_id))
        .await
        .ok_or_else(|| {
            AutomationAgentFailure::before_dispatch(DaemonError::AgentSpawn(format!(
                "automation '{automation_id}' references unknown agent '{agent_id}'"
            )))
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

    let measured =
        axocoatl_actor::execute_agent_measured(&actor, axocoatl_core::AgentInput::text(input))
            .await
            .map_err(AutomationAgentFailure::from_execution)?;
    let token_usage_known = measured.token_usage.complete;
    let token_usage = measured.token_usage.usage.clone();
    let mut out = measured.outcome.into_output();
    out.token_usage = token_usage;

    emit_event(
        daemon,
        automation_id,
        Some(agent_id),
        node_id,
        "TaskCompleted",
        Some(&out.content),
        Some(out.token_usage.total() as u64),
    );

    Ok(MeasuredAutomationAgentOutput {
        output: out,
        token_usage_known,
    })
}

/// Run a registered tool. The node's resolved input is parsed as JSON; if
/// that fails we wrap it as `{"input": "<raw>"}` — common-case ergonomics
/// so users don't have to JSON-encode every literal string.
async fn run_tool_node(
    daemon: &AutomationExecutionContext,
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

/// Emit an Automation observability event. `agent_id` populates `frame.agent`
/// for agent-backed nodes, while `node_id` populates `frame.task` for consumers
/// that correlate the event with an Automation node. For non-agent node kinds
/// (Tool/Conditional/etc), pass `None` for `agent_id`.
fn emit_event(
    daemon: &AutomationExecutionContext,
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
    use axocoatl_actor::{AgentActor, AgentBehavior, AgentError};
    use axocoatl_config::{
        AutomationEdge, AutomationNodeKind as Kind, AutomationTrigger, NodeInput, ResumeStrategy,
    };
    use axocoatl_core::{AgentConfig, AgentId, AgentInput};
    use axocoatl_tools::{BuiltinTool, ToolError};
    use ractor::Actor;

    struct MeasuredFailureBehavior {
        token_usage: axocoatl_core::MeasuredTokenUsage,
    }

    #[async_trait::async_trait]
    impl AgentBehavior for MeasuredFailureBehavior {
        async fn on_start(&mut self, _: &AgentConfig) -> Result<(), AgentError> {
            Ok(())
        }

        async fn execute(&mut self, _: AgentInput) -> Result<AgentOutput, AgentError> {
            Err(AgentError::Provider(
                "scripted provider failure".to_string(),
            ))
        }

        fn last_execution_token_usage_measurement(
            &self,
        ) -> Option<axocoatl_core::MeasuredTokenUsage> {
            Some(self.token_usage.clone())
        }

        async fn on_stop(&mut self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    /// Test tool for the Automation executor's documented raw-input fallback.
    /// Production Echo intentionally accepts only its declared `text` field.
    struct InputEchoTool;

    #[async_trait::async_trait]
    impl BuiltinTool for InputEchoTool {
        fn description(&self) -> &str {
            "echo the Automation raw-input fallback"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "input": { "type": "string" } },
                "required": ["input"]
            })
        }

        async fn execute(
            &self,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            arguments
                .get("input")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: "input_echo".to_string(),
                    reason: "expected string field 'input'".to_string(),
                })?;
            Ok(arguments)
        }
    }

    fn tmpdir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("axo-{label}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn interrupt_automation() -> Automation {
        Automation {
            id: "durable-interrupt".into(),
            name: "Durable interrupt".into(),
            description: None,
            nodes: vec![
                AutomationNode {
                    id: "prompt".into(),
                    kind: Kind::TextInput {
                        label: "Prompt".into(),
                        default_value: Some("default prompt".into()),
                        placeholder: None,
                        multiline: false,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "approval".into(),
                    kind: Kind::Interrupt {
                        input: NodeInput::FromUpstream {
                            nodes: vec!["prompt".into()],
                        },
                        resume_strategy: ResumeStrategy::Append,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "after".into(),
                    kind: Kind::Tool {
                        tool_id: "input_echo".into(),
                        input: NodeInput::FromUpstream {
                            nodes: vec!["approval".into()],
                        },
                    },
                    position: None,
                },
            ],
            edges: vec![
                AutomationEdge {
                    from: "prompt".into(),
                    to: "approval".into(),
                    label: None,
                },
                AutomationEdge {
                    from: "approval".into(),
                    to: "after".into(),
                    label: None,
                },
            ],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        }
    }

    fn test_context(
        automation_store: Arc<tokio::sync::RwLock<crate::automation_store::AutomationStore>>,
        run_store: Arc<crate::automation_runs::AutomationRunStore>,
        pending_interrupts: HashMap<String, PendingInterrupt>,
    ) -> AutomationExecutionContext {
        let mut tools = axocoatl_tools::ToolExecutor::new();
        tools.register_builtin("echo", Arc::new(axocoatl_tools::EchoTool));
        tools.register_builtin("input_echo", Arc::new(InputEchoTool));
        let stream_bus = crate::stream::StreamBus::new(16);
        AutomationExecutionContext {
            agent_registry: axocoatl_actor::AgentRegistry::new(),
            automation_store,
            pending_interrupts: Arc::new(tokio::sync::RwLock::new(pending_interrupts)),
            run_store,
            tool_executor: Arc::new(tools),
            stream_bus,
            schedule_table: Arc::new(std::sync::Mutex::new(Vec::new())),
            proactive_table: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    async fn register_measured_failure_agent(
        context: &AutomationExecutionContext,
        agent_id: &str,
        token_usage: axocoatl_core::MeasuredTokenUsage,
    ) {
        let id = AgentId::new(agent_id);
        let (actor, handle) = AgentActor::spawn(
            Some(format!(
                "automation-test-{agent_id}-{}",
                uuid::Uuid::new_v4()
            )),
            AgentActor,
            (
                AgentConfig {
                    id: id.clone(),
                    ..AgentConfig::default()
                },
                Box::new(MeasuredFailureBehavior { token_usage }) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .unwrap();
        context.agent_registry.register(id, actor).await;
        // The actor terminates itself after the scripted Execute error. Its
        // registry reference remains long enough for the Automation dispatch.
        drop(handle);
    }

    fn measured_usage() -> TokenUsageStats {
        TokenUsageStats {
            input_tokens: 11,
            output_tokens: 7,
            reasoning_tokens: Some(3),
        }
    }

    fn one_agent_automation(id: &str, agent_id: &str) -> Automation {
        Automation {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            nodes: vec![AutomationNode {
                id: "agent-node".into(),
                kind: Kind::Agent {
                    agent_id: agent_id.to_string(),
                    input: NodeInput::FromTrigger,
                },
                position: None,
            }],
            edges: Vec::new(),
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        }
    }

    #[tokio::test]
    async fn failed_agent_usage_and_completeness_reach_output_and_checkpoint() {
        for (suffix, measurement, expected_known, expected_total) in [
            (
                "known",
                axocoatl_core::MeasuredTokenUsage::known(measured_usage()),
                true,
                21,
            ),
            (
                "lower-bound",
                axocoatl_core::MeasuredTokenUsage::lower_bound(measured_usage()),
                false,
                21,
            ),
            (
                "unknown",
                axocoatl_core::MeasuredTokenUsage::lower_bound(TokenUsageStats::default()),
                false,
                0,
            ),
        ] {
            let root = tmpdir(&format!("automation-agent-failure-{suffix}"));
            let automation_store = Arc::new(tokio::sync::RwLock::new(
                crate::automation_store::AutomationStore::open(root.join("automations.json"))
                    .unwrap(),
            ));
            let run_store = Arc::new(
                crate::automation_runs::AutomationRunStore::open(root.join("runs")).unwrap(),
            );
            let context = test_context(automation_store, run_store.clone(), HashMap::new());
            let agent_id = format!("failing-{suffix}");
            register_measured_failure_agent(&context, &agent_id, measurement).await;
            let automation = one_agent_automation(&format!("failure-{suffix}"), &agent_id);

            let output = execute_automation_with_inputs_in_context(
                &context,
                &automation,
                "work",
                &HashMap::new(),
            )
            .await
            .unwrap();

            assert_eq!(output.total_token_usage.total(), expected_total);
            assert_eq!(output.token_usage_known, expected_known);
            assert_eq!(output.failed_agents.len(), 1);
            let run = run_store.list(&automation.id).await.unwrap().pop().unwrap();
            let checkpoint = run.checkpoints.last().unwrap();
            assert_eq!(checkpoint.total_token_usage.total(), expected_total);
            assert_eq!(checkpoint.token_usage_known, expected_known);
        }
    }

    #[tokio::test]
    async fn map_and_subgraph_preserve_failed_agent_usage() {
        let root = tmpdir("automation-nested-failure-usage");
        let automation_path = root.join("automations.json");
        let mut store = crate::automation_store::AutomationStore::open(&automation_path).unwrap();
        let inner = one_agent_automation("inner-failure", "subgraph-agent");
        store.create(inner).unwrap();
        let automation_store = Arc::new(tokio::sync::RwLock::new(store));
        let run_store =
            Arc::new(crate::automation_runs::AutomationRunStore::open(root.join("runs")).unwrap());
        let context = test_context(automation_store, run_store.clone(), HashMap::new());
        register_measured_failure_agent(
            &context,
            "map-agent",
            axocoatl_core::MeasuredTokenUsage::lower_bound(measured_usage()),
        )
        .await;
        register_measured_failure_agent(
            &context,
            "subgraph-agent",
            axocoatl_core::MeasuredTokenUsage::lower_bound(measured_usage()),
        )
        .await;

        let map = Automation {
            id: "map-failure".into(),
            name: "Map failure".into(),
            description: None,
            nodes: vec![
                AutomationNode {
                    id: "body".into(),
                    kind: Kind::Agent {
                        agent_id: "map-agent".into(),
                        input: NodeInput::FromMapItem,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "map".into(),
                    kind: Kind::Map {
                        input: NodeInput::Literal {
                            value: r#"["one"]"#.into(),
                        },
                        body_node: "body".into(),
                    },
                    position: None,
                },
            ],
            edges: Vec::new(),
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };
        let map_output =
            execute_automation_with_inputs_in_context(&context, &map, "unused", &HashMap::new())
                .await
                .unwrap();
        assert_eq!(map_output.total_token_usage.total(), 21);
        assert!(!map_output.token_usage_known);
        let map_run = run_store.list(&map.id).await.unwrap().pop().unwrap();
        let map_checkpoint = map_run.checkpoints.last().unwrap();
        assert_eq!(map_checkpoint.total_token_usage.total(), 21);
        assert!(!map_checkpoint.token_usage_known);

        let outer = Automation {
            id: "outer-failure".into(),
            name: "Outer failure".into(),
            description: None,
            nodes: vec![AutomationNode {
                id: "nested".into(),
                kind: Kind::Subgraph {
                    automation_id: "inner-failure".into(),
                    input: NodeInput::FromTrigger,
                },
                position: None,
            }],
            edges: Vec::new(),
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };
        let subgraph_output =
            execute_automation_with_inputs_in_context(&context, &outer, "unused", &HashMap::new())
                .await
                .unwrap();
        assert_eq!(subgraph_output.total_token_usage.total(), 21);
        assert!(!subgraph_output.token_usage_known);
        assert!(!subgraph_output.failed_agents.is_empty());
        let outer_run = run_store.list(&outer.id).await.unwrap().pop().unwrap();
        let outer_checkpoint = outer_run.checkpoints.last().unwrap();
        assert_eq!(outer_checkpoint.total_token_usage.total(), 21);
        assert!(!outer_checkpoint.token_usage_known);
    }

    #[test]
    fn execution_context_is_owned_send_sync_and_static() {
        fn assert_owned<T: Clone + Send + Sync + 'static>() {}
        assert_owned::<AutomationExecutionContext>();
    }

    #[tokio::test]
    async fn failed_nodes_mark_checkpoints_and_run_failed_while_downstream_continues() {
        let root = tmpdir("automation-failure-truth");
        let automation_path = root.join("automations.json");
        let runs_path = root.join("runs");
        let automation_store = Arc::new(tokio::sync::RwLock::new(
            crate::automation_store::AutomationStore::open(&automation_path).unwrap(),
        ));
        let run_store =
            Arc::new(crate::automation_runs::AutomationRunStore::open(&runs_path).unwrap());
        let context = test_context(automation_store, run_store.clone(), HashMap::new());
        let mut frames = context.stream_bus.subscribe();
        let automation = Automation {
            id: "failure-truth".into(),
            name: "Failure truth".into(),
            description: None,
            nodes: vec![
                AutomationNode {
                    id: "broken-agent".into(),
                    kind: Kind::Agent {
                        agent_id: "missing-agent".into(),
                        input: NodeInput::FromTrigger,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "broken-tool".into(),
                    kind: Kind::Tool {
                        tool_id: "missing-tool".into(),
                        input: NodeInput::FromTrigger,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "after".into(),
                    kind: Kind::TextInput {
                        label: "After".into(),
                        default_value: Some("continued".into()),
                        placeholder: None,
                        multiline: false,
                    },
                    position: None,
                },
            ],
            edges: vec![
                AutomationEdge {
                    from: "broken-agent".into(),
                    to: "broken-tool".into(),
                    label: None,
                },
                AutomationEdge {
                    from: "broken-tool".into(),
                    to: "after".into(),
                    label: None,
                },
            ],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };

        let output = execute_automation_with_inputs_in_context(
            &context,
            &automation,
            "start",
            &HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(output.failed_agents.len(), 2);
        assert_eq!(output.failed_agents[0].0, "missing-agent");
        assert_eq!(output.failed_agents[1].0, "tool:missing-tool");
        assert!(output
            .completed_agents
            .iter()
            .any(|subject| subject == "input:after"));

        let runs = run_store.list(&automation.id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, crate::automation_runs::RunStatus::Failed);
        assert!(runs[0].finished_at_unix.is_some());
        assert_eq!(
            runs[0]
                .checkpoints
                .iter()
                .map(|checkpoint| (checkpoint.node_id.as_str(), checkpoint.event))
                .collect::<Vec<_>>(),
            vec![
                (
                    "broken-agent",
                    crate::automation_runs::CheckpointEvent::NodeFailed,
                ),
                (
                    "broken-tool",
                    crate::automation_runs::CheckpointEvent::NodeFailed,
                ),
                (
                    "after",
                    crate::automation_runs::CheckpointEvent::NodeCompleted,
                ),
            ]
        );

        let mut failure_frames = HashMap::new();
        while let Ok(frame) = frames.try_recv() {
            if let crate::stream::StreamFrame::Event {
                event_type,
                agent,
                task,
                output,
                workflow,
                ..
            } = frame
            {
                if event_type == "NodeFailed" {
                    assert_eq!(workflow.as_deref(), Some("failure-truth"));
                    failure_frames.insert(task.unwrap(), (agent, output.unwrap()));
                }
            }
        }
        assert_eq!(failure_frames.len(), 2);
        assert_eq!(
            failure_frames
                .get("broken-agent")
                .and_then(|(agent, _)| agent.as_deref()),
            Some("missing-agent")
        );
        assert!(failure_frames["broken-agent"].1.contains("unknown agent"));
        assert_eq!(failure_frames["broken-tool"].0, None);
        assert!(failure_frames["broken-tool"].1.contains("missing-tool"));

        let failure_details: HashMap<&str, &str> = runs[0]
            .checkpoints
            .iter()
            .filter_map(|checkpoint| {
                checkpoint
                    .failure_detail
                    .as_deref()
                    .map(|detail| (checkpoint.node_id.as_str(), detail))
            })
            .collect();
        assert!(failure_details["broken-agent"].contains("unknown agent"));
        assert!(failure_details["broken-tool"].contains("missing-tool"));

        let reopened = crate::automation_runs::AutomationRunStore::open(&runs_path).unwrap();
        assert!(reopened
            .load(&automation.id, &runs[0].run_id)
            .unwrap()
            .checkpoints
            .iter()
            .filter_map(|checkpoint| checkpoint.failure_detail.as_deref())
            .any(|detail| detail.contains("missing-tool")));
    }

    #[test]
    fn recovered_run_retains_failure_status_from_prior_checkpoint() {
        let output = WorkflowOutput {
            workflow_id: "recovered".into(),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            final_content: "continued".into(),
            total_token_usage: TokenUsageStats::default(),
            token_usage_known: true,
            completed_agents: vec!["after".into()],
            failed_agents: Vec::new(),
        };

        assert_eq!(
            final_run_status(&Ok(output), true),
            crate::automation_runs::RunStatus::Failed
        );
    }

    #[tokio::test]
    async fn whole_run_rerun_persists_text_inputs_and_source_ancestry() {
        let root = tmpdir("automation-rerun-truth");
        let automation_store = Arc::new(tokio::sync::RwLock::new(
            crate::automation_store::AutomationStore::open(root.join("automations.json")).unwrap(),
        ));
        let run_store =
            Arc::new(crate::automation_runs::AutomationRunStore::open(root.join("runs")).unwrap());
        let context = test_context(automation_store, run_store.clone(), HashMap::new());
        let automation = Automation {
            id: "rerun-inputs".into(),
            name: "Rerun inputs".into(),
            description: None,
            nodes: vec![AutomationNode {
                id: "prompt".into(),
                kind: Kind::TextInput {
                    label: "Prompt".into(),
                    default_value: Some("default".into()),
                    placeholder: None,
                    multiline: true,
                },
                position: None,
            }],
            edges: Vec::new(),
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };
        let text_inputs = HashMap::from([("prompt".to_string(), "original value".to_string())]);
        let run_id = start_automation_run_in_context(
            &context,
            &automation,
            "original trigger",
            &text_inputs,
            Some(crate::automation_runs::ForkSource {
                source_run_id: "source-run".into(),
                from_start: true,
                from_step: 0,
            }),
        )
        .await
        .unwrap();

        let output = execute_started_automation_run_in_context(
            &context,
            &automation,
            "original trigger",
            &text_inputs,
            &run_id,
        )
        .await
        .unwrap();
        assert_eq!(output.final_content, "original value");

        let persisted = run_store.load(&automation.id, &run_id).unwrap();
        assert_eq!(
            persisted.status,
            crate::automation_runs::RunStatus::Completed
        );
        assert_eq!(persisted.trigger_input, "original trigger");
        assert_eq!(
            persisted.text_inputs.get("prompt").map(String::as_str),
            Some("original value")
        );
        let ancestry = persisted.forked_from.expect("rerun ancestry must persist");
        assert_eq!(ancestry.source_run_id, "source-run");
        assert!(ancestry.from_start);
        assert_eq!(ancestry.from_step, 0);
        assert_eq!(
            persisted.checkpoints[0]
                .outputs
                .get("prompt")
                .map(String::as_str),
            Some("original value")
        );
    }

    #[tokio::test]
    async fn final_content_uses_all_runtime_sinks_in_declaration_order() {
        let root = tmpdir("automation-terminal-output");
        let automation_store = Arc::new(tokio::sync::RwLock::new(
            crate::automation_store::AutomationStore::open(root.join("automations.json")).unwrap(),
        ));
        let run_store =
            Arc::new(crate::automation_runs::AutomationRunStore::open(root.join("runs")).unwrap());
        let context = test_context(automation_store, run_store.clone(), HashMap::new());
        let automation = Automation {
            id: "terminal-output".into(),
            name: "Terminal output".into(),
            description: None,
            nodes: vec![
                AutomationNode {
                    id: "independent".into(),
                    kind: Kind::TextInput {
                        label: "Independent".into(),
                        default_value: Some("independent sink".into()),
                        placeholder: None,
                        multiline: false,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "source".into(),
                    kind: Kind::TextInput {
                        label: "Source".into(),
                        default_value: Some("tool sink".into()),
                        placeholder: None,
                        multiline: false,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "terminal-tool".into(),
                    kind: Kind::Tool {
                        tool_id: "echo".into(),
                        input: NodeInput::Template {
                            template: r#"{"text":"{{node:source}}"}"#.into(),
                        },
                    },
                    position: None,
                },
            ],
            edges: vec![AutomationEdge {
                from: "source".into(),
                to: "terminal-tool".into(),
                label: None,
            }],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };

        let output = execute_automation_with_inputs_in_context(
            &context,
            &automation,
            "unused",
            &HashMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            output.final_content,
            "independent sink\n\n{\"text\":\"tool sink\"}"
        );
        assert!(output.agent_outputs.is_empty());
        let persisted = run_store.list(&automation.id).await.unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].final_content.as_deref(),
            Some("independent sink\n\n{\"text\":\"tool sink\"}")
        );
    }

    #[tokio::test]
    async fn final_content_uses_terminal_map_output() {
        let root = tmpdir("automation-map-output");
        let automation_store = Arc::new(tokio::sync::RwLock::new(
            crate::automation_store::AutomationStore::open(root.join("automations.json")).unwrap(),
        ));
        let run_store =
            Arc::new(crate::automation_runs::AutomationRunStore::open(root.join("runs")).unwrap());
        let context = test_context(automation_store, run_store, HashMap::new());
        let automation = Automation {
            id: "map-output".into(),
            name: "Map output".into(),
            description: None,
            nodes: vec![
                AutomationNode {
                    id: "body".into(),
                    kind: Kind::Tool {
                        tool_id: "input_echo".into(),
                        input: NodeInput::FromMapItem,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "items".into(),
                    kind: Kind::TextInput {
                        label: "Items".into(),
                        default_value: Some(r#"["one","two"]"#.into()),
                        placeholder: None,
                        multiline: false,
                    },
                    position: None,
                },
                AutomationNode {
                    id: "map".into(),
                    kind: Kind::Map {
                        input: NodeInput::FromUpstream {
                            nodes: vec!["items".into()],
                        },
                        body_node: "body".into(),
                    },
                    position: None,
                },
            ],
            edges: vec![AutomationEdge {
                from: "items".into(),
                to: "map".into(),
                label: None,
            }],
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };

        let output = execute_automation_with_inputs_in_context(
            &context,
            &automation,
            "unused",
            &HashMap::new(),
        )
        .await
        .unwrap();
        let collected: Vec<String> = serde_json::from_str(&output.final_content).unwrap();
        assert_eq!(collected, vec![r#"{"input":"one"}"#, r#"{"input":"two"}"#]);
        assert!(output.agent_outputs.is_empty());
    }

    #[tokio::test]
    async fn final_content_uses_terminal_subgraph_output() {
        let root = tmpdir("automation-subgraph-output");
        let automation_path = root.join("automations.json");
        let mut store = crate::automation_store::AutomationStore::open(&automation_path).unwrap();
        store
            .create(Automation {
                id: "inner".into(),
                name: "Inner".into(),
                description: None,
                nodes: vec![AutomationNode {
                    id: "echo".into(),
                    kind: Kind::Tool {
                        tool_id: "echo".into(),
                        input: NodeInput::FromTrigger,
                    },
                    position: None,
                }],
                edges: Vec::new(),
                trigger: AutomationTrigger::Manual,
                enabled: true,
                folder: None,
            })
            .unwrap();
        let automation_store = Arc::new(tokio::sync::RwLock::new(store));
        let run_store =
            Arc::new(crate::automation_runs::AutomationRunStore::open(root.join("runs")).unwrap());
        let context = test_context(automation_store, run_store, HashMap::new());
        let outer = Automation {
            id: "outer".into(),
            name: "Outer".into(),
            description: None,
            nodes: vec![AutomationNode {
                id: "nested".into(),
                kind: Kind::Subgraph {
                    automation_id: "inner".into(),
                    input: NodeInput::Literal {
                        value: r#"{"text":"from subgraph"}"#.into(),
                    },
                },
                position: None,
            }],
            edges: Vec::new(),
            trigger: AutomationTrigger::Manual,
            enabled: true,
            folder: None,
        };

        let output =
            execute_automation_with_inputs_in_context(&context, &outer, "unused", &HashMap::new())
                .await
                .unwrap();
        assert_eq!(output.final_content, r#"{"text":"from subgraph"}"#);
        assert!(output.agent_outputs.is_empty());
    }

    #[tokio::test]
    async fn interrupted_run_rehydrates_and_continues_without_replaying_completed_nodes() {
        let root = tmpdir("interrupt-restart");
        let automation_path = root.join("automations.json");
        let runs_path = root.join("runs");
        let automation = interrupt_automation();

        let mut first_store =
            crate::automation_store::AutomationStore::open(&automation_path).unwrap();
        first_store.create(automation.clone()).unwrap();
        let first_store = Arc::new(tokio::sync::RwLock::new(first_store));
        let first_runs =
            Arc::new(crate::automation_runs::AutomationRunStore::open(&runs_path).unwrap());
        let first_context = test_context(first_store, first_runs, HashMap::new());
        let mut text_inputs = HashMap::new();
        text_inputs.insert("prompt".to_string(), "operator prompt".to_string());
        let run_context = first_context.clone();
        let run_automation = automation.clone();
        let task = tokio::spawn(async move {
            execute_automation_with_inputs_in_context(
                &run_context,
                &run_automation,
                "trigger",
                &text_inputs,
            )
            .await
        });

        let parked = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(interrupt) = first_context
                    .pending_interrupts
                    .read()
                    .await
                    .values()
                    .next()
                    .cloned()
                {
                    break interrupt;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run should park");
        assert_eq!(parked.message, "operator prompt");
        let run_id = parked.run_id.clone();

        // Losing this task and every process-local Notify simulates the daemon
        // going away while the operator prompt is open.
        task.abort();
        let _ = task.await;
        drop(first_context);

        // Strip the new immutable inputs to reproduce the exact on-disk shape
        // written by older daemons (including the live demo run that exposed
        // this bug). Recovery must validate and use the current Automation.
        let run_path = runs_path
            .with_file_name(".axocoatl-runs-v1")
            .join("durable-interrupt")
            .join(format!("{run_id}.json"));
        let mut legacy_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&run_path).unwrap()).unwrap();
        let persisted =
            serde_json::from_value::<crate::automation_runs::Run>(legacy_json.clone()).unwrap();
        assert!(persisted.automation_snapshot.is_some());
        assert_eq!(
            persisted.text_inputs.get("prompt").map(String::as_str),
            Some("operator prompt")
        );
        let legacy_object = legacy_json.as_object_mut().unwrap();
        legacy_object.remove("automation_snapshot");
        legacy_object.remove("text_inputs");
        std::fs::write(&run_path, serde_json::to_vec_pretty(&legacy_json).unwrap()).unwrap();

        let second_store = Arc::new(tokio::sync::RwLock::new(
            crate::automation_store::AutomationStore::open(&automation_path).unwrap(),
        ));
        let second_runs =
            Arc::new(crate::automation_runs::AutomationRunStore::open(&runs_path).unwrap());
        let rehydrated = rehydrate_pending_interrupts(&second_store, &second_runs).await;
        let key = format!("durable-interrupt:{run_id}:approval");
        assert_eq!(rehydrated.len(), 1);
        assert_eq!(rehydrated.get(&key).unwrap().message, "operator prompt");
        assert!(rehydrated.get(&key).unwrap().recovered);

        let second_context = test_context(second_store, second_runs.clone(), rehydrated);
        let resolution = resolve_pending_interrupt(
            &second_context,
            "durable-interrupt",
            &run_id,
            "approval",
            "approved".into(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(resolution, InterruptResolution::Recovered);

        let completed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let run = second_runs.load("durable-interrupt", &run_id).unwrap();
                if run.status == crate::automation_runs::RunStatus::Completed {
                    break run;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovered continuation should finish");
        assert!(completed.automation_snapshot.is_none());
        assert!(completed.text_inputs.is_empty());
        assert_eq!(
            completed
                .checkpoints
                .iter()
                .filter(|checkpoint| checkpoint.node_id == "prompt")
                .count(),
            1,
            "the completed TextInput must not replay"
        );
        assert_eq!(
            completed
                .checkpoints
                .iter()
                .map(|checkpoint| checkpoint.event)
                .collect::<Vec<_>>(),
            vec![
                crate::automation_runs::CheckpointEvent::NodeCompleted,
                crate::automation_runs::CheckpointEvent::InterruptParked,
                crate::automation_runs::CheckpointEvent::InterruptResumed,
                crate::automation_runs::CheckpointEvent::NodeCompleted,
            ]
        );
        let final_checkpoint = completed.checkpoints.last().unwrap();
        assert_eq!(
            final_checkpoint.outputs.get("approval").map(String::as_str),
            Some("operator prompt\n\napproved")
        );
        assert!(final_checkpoint
            .outputs
            .get("after")
            .is_some_and(|output| output.contains("approved")));
        assert!(second_context.pending_interrupts.read().await.is_empty());
    }

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
