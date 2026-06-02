//! Background activation loop for stigmergic multi-agent coordination.
//!
//! When an event is published to the lattice and agents cross their activation
//! threshold, `ActivationRequest`s are sent through an mpsc channel. This loop
//! drains the channel, executes agents, publishes completion events, and sends
//! any newly activated agents back through the channel — creating a self-sustaining
//! cascade until the workflow completes.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use axocoatl_actor::AgentRegistry;
use axocoatl_config::AgentConfigYaml;
use axocoatl_coordination::{EventId, EventType, LatticeEvent};
use axocoatl_core::AgentId;

use crate::workflow::WorkflowExecution;

/// A request to activate an agent within a workflow.
pub struct ActivationRequest {
    pub agent_id: AgentId,
    pub triggering_event: LatticeEvent,
    pub workflow_exec: Arc<WorkflowExecution>,
}

fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Run the activation loop as a background task.
///
/// Drains `activation_rx` for agent activation requests. For each request:
/// 1. Checks the workflow's cycle guard
/// 2. Builds agent input from upstream results
/// 3. Executes the agent via the registry
/// 4. Publishes a TaskCompleted or AgentFailed event
/// 5. Sends any newly activated agents back through `activation_tx`
pub async fn run_activation_loop(
    mut activation_rx: mpsc::UnboundedReceiver<ActivationRequest>,
    agent_registry: AgentRegistry,
    event_lattice: Arc<axocoatl_coordination::EventLattice>,
    activation_tx: mpsc::UnboundedSender<ActivationRequest>,
    config_agents: Vec<AgentConfigYaml>,
    stream_bus: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
) {
    // Index agent configs by ID for quick lookup
    let agent_configs: std::collections::HashMap<String, AgentConfigYaml> = config_agents
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect();

    while let Some(req) = activation_rx.recv().await {
        let agent_id = req.agent_id.clone();
        let workflow_exec = req.workflow_exec.clone();
        let lattice = event_lattice.clone();
        let tx = activation_tx.clone();
        let registry = agent_registry.clone();
        let _configs = agent_configs.clone();
        let bus = stream_bus.clone();

        // Check cycle guard before proceeding
        if !workflow_exec.check_cycle_guard() {
            tracing::warn!(
                agent = %agent_id,
                workflow = %workflow_exec.workflow_id,
                "Activation dropped — cycle guard exceeded"
            );
            continue;
        }

        // Skip if this agent already has a result
        if workflow_exec.results.contains_key(&agent_id) {
            tracing::debug!(agent = %agent_id, "Agent already has a result, skipping");
            continue;
        }

        // Spawn execution as a concurrent task so multiple agents can run in parallel
        tokio::spawn(async move {
            let input_text = workflow_exec.build_agent_input(&agent_id);

            tracing::info!(
                agent = %agent_id,
                workflow = %workflow_exec.workflow_id,
                "Activating agent in workflow"
            );

            // Telemetry only — tell observers (the dashboard SSE stream) that
            // this agent is starting. `notify_observers` does NOT touch the
            // stigmergic signal math, so it can't perturb the cascade.
            lattice.notify_observers(&LatticeEvent {
                id: EventId::random(),
                event_type: EventType::AgentActivated {
                    agent_id: agent_id.to_string(),
                },
                payload: serde_json::json!({
                    "workflow_id": workflow_exec.workflow_id,
                }),
                produced_by: agent_id.to_string(),
                timestamp: now_timestamp(),
            });

            // Execute the agent
            let actor = match registry.get(&agent_id).await {
                Some(actor) => actor,
                None => {
                    let err = format!("Agent '{}' not found in registry", agent_id);
                    tracing::error!(%err);
                    workflow_exec.record_result(agent_id.clone(), Err(err.clone()));

                    let event = LatticeEvent {
                        id: EventId::random(),
                        event_type: EventType::AgentFailed {
                            agent_id: agent_id.to_string(),
                            error: err,
                        },
                        payload: serde_json::json!({
                            "workflow_id": workflow_exec.workflow_id,
                        }),
                        produced_by: agent_id.to_string(),
                        timestamp: now_timestamp(),
                    };
                    let _ = lattice.publish(event);
                    return;
                }
            };

            let agent_input = axocoatl_core::AgentInput::text(&input_text);

            // Stream this agent's output onto the bus token-by-token. The
            // forwarder ends when execution finishes and the sink drops.
            let (sink_tx, mut sink_rx) =
                mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
            {
                let bus = bus.clone();
                let wf = workflow_exec.workflow_id.clone();
                let aid = agent_id.to_string();
                tokio::spawn(async move {
                    while let Some(chunk) = sink_rx.recv().await {
                        let frame = match chunk {
                            axocoatl_actor::AgentStreamChunk::Text(delta) => {
                                crate::stream::StreamFrame::Token {
                                    workflow: wf.clone(),
                                    agent: aid.clone(),
                                    delta,
                                }
                            }
                            axocoatl_actor::AgentStreamChunk::Reasoning(delta) => {
                                crate::stream::StreamFrame::Reasoning {
                                    workflow: wf.clone(),
                                    agent: aid.clone(),
                                    delta,
                                }
                            }
                            axocoatl_actor::AgentStreamChunk::ToolCallStarted {
                                id,
                                name,
                                arguments,
                            } => crate::stream::StreamFrame::ToolCall {
                                workflow: wf.clone(),
                                agent: aid.clone(),
                                call_id: id,
                                name,
                                phase: "start".to_string(),
                                arguments: Some(arguments),
                                result: None,
                                is_error: false,
                            },
                            axocoatl_actor::AgentStreamChunk::ToolCallResult {
                                id,
                                name,
                                result,
                                is_error,
                            } => crate::stream::StreamFrame::ToolCall {
                                workflow: wf.clone(),
                                agent: aid.clone(),
                                call_id: id,
                                name,
                                phase: "result".to_string(),
                                arguments: None,
                                result: Some(result),
                                is_error,
                            },
                        };
                        let _ = bus.send(frame);
                    }
                });
            }
            let result =
                axocoatl_actor::execute_agent_streaming(&actor, agent_input, sink_tx).await;

            match result {
                Ok(output) => {
                    tracing::info!(
                        agent = %agent_id,
                        workflow = %workflow_exec.workflow_id,
                        tokens = output.token_usage.total(),
                        "Agent completed in workflow"
                    );

                    // Carry a bounded slice of the agent's output so observers
                    // can show "what the AI did" live as the run streams.
                    let preview: String = output.content.chars().take(12_000).collect();
                    let total_tokens = output.token_usage.total();

                    workflow_exec.record_result(agent_id.clone(), Ok(output));

                    // Publish TaskCompleted event
                    let event = LatticeEvent {
                        id: EventId::random(),
                        event_type: EventType::TaskCompleted {
                            task_id: agent_id.to_string(),
                        },
                        payload: serde_json::json!({
                            "agent_id": agent_id.to_string(),
                            "workflow_id": workflow_exec.workflow_id,
                            "output": preview,
                            "tokens": total_tokens,
                        }),
                        produced_by: agent_id.to_string(),
                        timestamp: now_timestamp(),
                    };

                    let activated = lattice.publish(event);

                    // Send newly activated agents through the channel
                    for activated_id in activated {
                        if workflow_exec.expected_agents.contains(&activated_id) {
                            let trigger = LatticeEvent {
                                id: EventId::random(),
                                event_type: EventType::TaskCompleted {
                                    task_id: agent_id.to_string(),
                                },
                                payload: serde_json::json!({}),
                                produced_by: agent_id.to_string(),
                                timestamp: now_timestamp(),
                            };

                            let _ = tx.send(ActivationRequest {
                                agent_id: activated_id,
                                triggering_event: trigger,
                                workflow_exec: workflow_exec.clone(),
                            });
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(
                        agent = %agent_id,
                        workflow = %workflow_exec.workflow_id,
                        error = %err,
                        "Agent failed in workflow"
                    );

                    workflow_exec.record_result(agent_id.clone(), Err(err.clone()));

                    let event = LatticeEvent {
                        id: EventId::random(),
                        event_type: EventType::AgentFailed {
                            agent_id: agent_id.to_string(),
                            error: err,
                        },
                        payload: serde_json::json!({
                            "workflow_id": workflow_exec.workflow_id,
                        }),
                        produced_by: agent_id.to_string(),
                        timestamp: now_timestamp(),
                    };
                    let _ = lattice.publish(event);
                }
            }
        });
    }
}
