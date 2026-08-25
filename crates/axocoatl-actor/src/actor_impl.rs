use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio::sync::oneshot;

use axocoatl_core::{
    AgentConfig, AgentInput, AgentOutput, AgentStatus, MeasuredTokenUsage, TokenUsageStats,
};

use crate::behavior::AgentBehavior;
use crate::run_control::{AgentRunControl, AgentRunOutcome};

/// Messages that can be sent to an agent actor.
pub enum AgentMessage {
    /// Execute a task.
    Execute {
        input: AgentInput,
        reply: oneshot::Sender<Result<MeasuredAgentRunOutcome, AgentExecutionFailure>>,
        /// Optional sink — when present, the agent's output is streamed to it
        /// chunk-by-chunk as the LLM generates.
        sink: Option<crate::behavior::StreamSink>,
        /// Optional caller-owned identity and cooperative cancellation handle.
        /// Legacy execution helpers leave this unset.
        control: Option<AgentRunControl>,
    },
    /// Query current status.
    GetStatus(oneshot::Sender<AgentStatus>),
    /// Get cumulative token usage.
    GetTokenUsage(oneshot::Sender<TokenUsageStats>),
    /// Get cumulative usage together with sticky completeness.
    GetMeasuredTokenUsage(oneshot::Sender<MeasuredTokenUsage>),
    /// Run a background consolidation pass — but only if the agent has been idle
    /// for at least `idle_threshold_secs` (the actor decides, so the daemon never
    /// triggers the LLM pass in the gap between a user's two messages).
    Consolidate {
        idle_threshold_secs: u64,
        reply: oneshot::Sender<Result<crate::behavior::ConsolidationReport, String>>,
    },
}

/// A failed Execute with the exact provider usage incurred by that activation.
///
/// Simple callers may continue to render this through [`Display`]. Accounting
/// callers (for example Automation) can use `token_usage` without a racy pair
/// of actor-lifetime usage queries around the execution.
#[derive(Debug, Clone)]
pub struct AgentExecutionFailure {
    pub message: String,
    /// The numeric known subtotal and whether it covers every provider call in
    /// the activation. An incomplete measurement remains useful as a lower
    /// bound; it must not be flattened to zero merely because a later call did
    /// not return terminal usage.
    pub token_usage: MeasuredTokenUsage,
}

impl AgentExecutionFailure {
    pub fn new(message: impl Into<String>, token_usage: MeasuredTokenUsage) -> Self {
        Self {
            message: message.into(),
            token_usage,
        }
    }
}

impl std::fmt::Display for AgentExecutionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentExecutionFailure {}

/// A completed or cooperatively-cancelled Execute plus completeness-aware
/// usage for that activation.
#[derive(Debug, Clone)]
pub struct MeasuredAgentRunOutcome {
    pub outcome: AgentRunOutcome,
    /// Numeric known subtotal plus whether the tracked total covers every
    /// provider call. Provider usage is used when reported and a completed
    /// response may be estimated locally when it is not.
    pub token_usage: MeasuredTokenUsage,
}

// ractor requires Message: Send + 'static
// We can't derive Debug because oneshot::Sender doesn't impl Debug nicely,
// but ractor only needs Send + 'static.
impl std::fmt::Debug for AgentMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentMessage::Execute { .. } => write!(f, "AgentMessage::Execute"),
            AgentMessage::GetStatus(_) => write!(f, "AgentMessage::GetStatus"),
            AgentMessage::GetTokenUsage(_) => write!(f, "AgentMessage::GetTokenUsage"),
            AgentMessage::GetMeasuredTokenUsage(_) => {
                write!(f, "AgentMessage::GetMeasuredTokenUsage")
            }
            AgentMessage::Consolidate { .. } => write!(f, "AgentMessage::Consolidate"),
        }
    }
}

/// Persistent state held by each agent actor between messages.
pub struct AgentActorState {
    pub config: AgentConfig,
    pub status: AgentStatus,
    pub behavior: Box<dyn AgentBehavior>,
    pub token_usage: MeasuredTokenUsage,
    /// When this agent last processed a turn — drives the consolidation idle gate.
    pub last_active: std::time::Instant,
}

/// The ractor Actor wrapper for Axocoatl agents.
pub struct AgentActor;

impl Actor for AgentActor {
    type Msg = AgentMessage;
    type State = AgentActorState;
    type Arguments = (AgentConfig, Box<dyn AgentBehavior>);

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        (config, mut behavior): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        behavior
            .on_start(&config)
            .await
            .map_err(|e| ActorProcessingErr::from(e.to_string()))?;

        tracing::info!(agent_id = %config.id, "Agent started");

        let token_usage = behavior
            .cumulative_token_usage_measurement()
            .unwrap_or_else(|| MeasuredTokenUsage::known(TokenUsageStats::default()));

        Ok(AgentActorState {
            config,
            status: AgentStatus::Idle,
            behavior,
            token_usage,
            last_active: std::time::Instant::now(),
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            AgentMessage::Execute {
                input,
                reply,
                sink,
                control,
            } => {
                state.status = AgentStatus::Running;

                // Attach the streaming sink for this execution, then clear it
                // afterwards so a later non-streaming call doesn't reuse it.
                let streaming = sink.is_some();
                if streaming {
                    state.behavior.set_stream_sink(sink);
                }
                let result = match control {
                    Some(control) => state.behavior.execute_controlled(input, control).await,
                    None => state
                        .behavior
                        .execute(input)
                        .await
                        .map(AgentRunOutcome::Completed),
                };
                if streaming {
                    state.behavior.set_stream_sink(None);
                }
                state.last_active = std::time::Instant::now();
                let cumulative_usage = state.behavior.cumulative_token_usage_measurement();
                let execution_usage = state.behavior.last_execution_token_usage_measurement();

                match result {
                    Ok(outcome) => {
                        let execution_usage = execution_usage.unwrap_or_else(|| {
                            MeasuredTokenUsage::known(outcome.output().token_usage.clone())
                        });
                        if let Some(usage) = cumulative_usage {
                            state.token_usage = usage;
                        } else {
                            state.token_usage.usage.merge(&execution_usage.usage);
                            state.token_usage.complete &= execution_usage.complete;
                        }
                        state.status = AgentStatus::Idle;
                        tracing::debug!(
                            agent_id = %state.config.id,
                            cancelled = outcome.is_cancelled(),
                            "Execution reached a safe boundary"
                        );
                        let _ = reply.send(Ok(MeasuredAgentRunOutcome {
                            token_usage: execution_usage,
                            outcome,
                        }));
                    }
                    Err(e) => {
                        let execution_usage = execution_usage.unwrap_or_else(|| {
                            MeasuredTokenUsage::lower_bound(TokenUsageStats::default())
                        });
                        if let Some(usage) = cumulative_usage {
                            state.token_usage = usage;
                        } else {
                            state.token_usage.usage.merge(&execution_usage.usage);
                            state.token_usage.complete &= execution_usage.complete;
                        }
                        let err_msg = e.to_string();
                        state.status = AgentStatus::Failed {
                            error: err_msg.clone(),
                            restarts: 0,
                        };
                        let _ = reply.send(Err(AgentExecutionFailure::new(
                            err_msg.clone(),
                            execution_usage,
                        )));
                        return Err(ActorProcessingErr::from(err_msg));
                    }
                }
            }
            AgentMessage::GetStatus(reply) => {
                let _ = reply.send(state.status.clone());
            }
            AgentMessage::GetTokenUsage(reply) => {
                let _ = reply.send(state.token_usage.usage.clone());
            }
            AgentMessage::GetMeasuredTokenUsage(reply) => {
                let _ = reply.send(state.token_usage.clone());
            }
            AgentMessage::Consolidate {
                idle_threshold_secs,
                reply,
            } => {
                if state.last_active.elapsed() < std::time::Duration::from_secs(idle_threshold_secs)
                {
                    // Active too recently — skip cheaply (no LLM call).
                    let _ = reply.send(Ok(crate::behavior::ConsolidationReport::skipped()));
                } else {
                    state.status = AgentStatus::Running;
                    let result = state
                        .behavior
                        .on_consolidate()
                        .await
                        .map_err(|e| e.to_string());
                    if let Some(usage) = state.behavior.cumulative_token_usage_measurement() {
                        state.token_usage = usage;
                    }
                    state.status = AgentStatus::Idle;
                    let _ = reply.send(result);
                }
            }
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        msg: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match msg {
            SupervisionEvent::ActorFailed(dead_actor, err) => {
                tracing::warn!(
                    supervisor = %state.config.id,
                    failed_child = %dead_actor.get_name().unwrap_or("unknown".to_string()),
                    error = %err,
                    "Child agent failed"
                );
            }
            SupervisionEvent::ActorTerminated(actor_cell, _, _) => {
                tracing::info!(
                    actor = %actor_cell.get_name().unwrap_or("unknown".to_string()),
                    "Child actor terminated normally"
                );
            }
            _ => {}
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state
            .behavior
            .on_stop()
            .await
            .map_err(|e| ActorProcessingErr::from(e.to_string()))?;
        tracing::info!(agent_id = %state.config.id, "Agent stopped");
        Ok(())
    }
}

/// Helper: send Execute message and await the response.
pub async fn execute_agent(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
) -> Result<AgentOutput, String> {
    execute_agent_measured(actor, input)
        .await
        .map(|measured| measured.outcome.into_output())
        .map_err(|error| error.to_string())
}

/// Execute an agent and retain exact per-activation usage on failure.
pub async fn execute_agent_measured(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
) -> Result<MeasuredAgentRunOutcome, AgentExecutionFailure> {
    execute_agent_outcome_measured(actor, input, None, None).await
}

async fn execute_agent_outcome_measured(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    sink: Option<crate::behavior::StreamSink>,
    control: Option<AgentRunControl>,
) -> Result<MeasuredAgentRunOutcome, AgentExecutionFailure> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::Execute {
            input,
            reply: tx,
            sink,
            control,
        })
        .map_err(|e| {
            AgentExecutionFailure::new(
                format!("Failed to send to agent: {e}"),
                MeasuredTokenUsage::known(TokenUsageStats::default()),
            )
        })?;
    rx.await.map_err(|_| {
        AgentExecutionFailure::new(
            "Agent dropped reply channel",
            MeasuredTokenUsage::lower_bound(TokenUsageStats::default()),
        )
    })?
}

/// Execute an agent with cooperative cancellation and no streaming observer.
pub async fn execute_agent_controlled(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    control: AgentRunControl,
) -> Result<AgentRunOutcome, String> {
    execute_agent_controlled_measured(actor, input, control)
        .await
        .map(|measured| measured.outcome)
        .map_err(|error| error.to_string())
}

/// Controlled Execute retaining exact per-activation usage on failure.
pub async fn execute_agent_controlled_measured(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    control: AgentRunControl,
) -> Result<MeasuredAgentRunOutcome, AgentExecutionFailure> {
    execute_agent_outcome_measured(actor, input, None, Some(control)).await
}

/// Helper: execute an agent while streaming its output chunks to `sink`.
/// Returns the final `AgentOutput` once generation completes.
pub async fn execute_agent_streaming(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    sink: crate::behavior::StreamSink,
) -> Result<AgentOutput, String> {
    execute_agent_streaming_measured(actor, input, sink)
        .await
        .map(|measured| measured.outcome.into_output())
        .map_err(|error| error.to_string())
}

/// Streaming Execute retaining exact per-activation usage on failure.
pub async fn execute_agent_streaming_measured(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    sink: crate::behavior::StreamSink,
) -> Result<MeasuredAgentRunOutcome, AgentExecutionFailure> {
    execute_agent_outcome_measured(actor, input, Some(sink), None).await
}

/// Execute an agent with a stable run id and cooperative cancellation handle.
///
/// Unlike dropping the future returned by [`execute_agent_streaming`], calling
/// [`AgentRunControl::cancel`] reaches the actor behavior. A cancelled result
/// carries partial output and does not fail or restart the actor.
pub async fn execute_agent_streaming_controlled(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    sink: crate::behavior::StreamSink,
    control: AgentRunControl,
) -> Result<AgentRunOutcome, String> {
    execute_agent_streaming_controlled_measured(actor, input, sink, control)
        .await
        .map(|measured| measured.outcome)
        .map_err(|error| error.to_string())
}

/// Controlled streaming Execute retaining exact usage on failure.
pub async fn execute_agent_streaming_controlled_measured(
    actor: &ActorRef<AgentMessage>,
    input: AgentInput,
    sink: crate::behavior::StreamSink,
    control: AgentRunControl,
) -> Result<MeasuredAgentRunOutcome, AgentExecutionFailure> {
    execute_agent_outcome_measured(actor, input, Some(sink), Some(control)).await
}

/// Helper: query agent status.
pub async fn get_agent_status(actor: &ActorRef<AgentMessage>) -> Result<AgentStatus, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::GetStatus(tx))
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())
}

/// Helper: ask an agent to run a consolidation pass (it self-skips unless it has
/// been idle for at least `idle_threshold_secs`).
pub async fn consolidate_agent(
    actor: &ActorRef<AgentMessage>,
    idle_threshold_secs: u64,
) -> Result<crate::behavior::ConsolidationReport, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::Consolidate {
            idle_threshold_secs,
            reply: tx,
        })
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())?
}

/// Helper: query cumulative token usage for an agent.
pub async fn get_agent_token_usage(
    actor: &ActorRef<AgentMessage>,
) -> Result<TokenUsageStats, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::GetTokenUsage(tx))
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())
}

/// Query cumulative token usage with sticky completeness.
pub async fn get_agent_measured_token_usage(
    actor: &ActorRef<AgentMessage>,
) -> Result<MeasuredTokenUsage, String> {
    let (tx, rx) = oneshot::channel();
    actor
        .cast(AgentMessage::GetMeasuredTokenUsage(tx))
        .map_err(|e| format!("Failed to send to agent: {e}"))?;
    rx.await
        .map_err(|_| "Agent dropped reply channel".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
    /// A simple echo behavior for testing.
    struct EchoBehavior;

    #[async_trait::async_trait]
    impl AgentBehavior for EchoBehavior {
        async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Ok(AgentOutput {
                content: format!("Echo: {}", input.content),
                tool_calls: vec![],
                token_usage: TokenUsageStats::new(10, 5),
            })
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    /// A behavior that fails on every call.
    struct FailBehavior;

    #[async_trait::async_trait]
    impl AgentBehavior for FailBehavior {
        async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, _input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Err(crate::AgentError::Internal(
                "intentional failure".to_string(),
            ))
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    struct IncompleteSubtotalFailureBehavior;

    #[async_trait::async_trait]
    impl AgentBehavior for IncompleteSubtotalFailureBehavior {
        async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }

        async fn execute(&mut self, _input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Err(crate::AgentError::Internal(
                "failed after one reported call".to_string(),
            ))
        }

        fn last_execution_token_usage_measurement(&self) -> Option<MeasuredTokenUsage> {
            Some(MeasuredTokenUsage::lower_bound(TokenUsageStats::new(
                80, 20,
            )))
        }

        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    fn test_config() -> AgentConfig {
        AgentConfig {
            id: AgentId::new("test-agent"),
            name: "Test Agent".to_string(),
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn spawn_and_execute() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-echo".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();

        let output = execute_agent(&actor_ref, AgentInput::text("hello")).await;
        assert!(output.is_ok());
        assert_eq!(output.unwrap().content, "Echo: hello");

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    /// Records the `system_override` it actually receives.
    struct RecordingBehavior {
        seen: std::sync::Arc<std::sync::Mutex<Option<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl AgentBehavior for RecordingBehavior {
        async fn on_start(&mut self, _: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            *self.seen.lock().unwrap() = Some(input.system_override.clone());
            Ok(AgentOutput::text("ok"))
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn execute_agent_round_trip_preserves_system_override() {
        // axocoatl#64: the per-request system_override must survive the full
        // actor message round-trip (execute_agent → cast → handler → execute).
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let behavior = RecordingBehavior { seen: seen.clone() };
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-override".to_string()),
            AgentActor,
            (test_config(), Box::new(behavior)),
        )
        .await
        .unwrap();

        let _ = execute_agent(
            &actor_ref,
            AgentInput::text("hi").with_system_override(Some("OVERRIDE".to_string())),
        )
        .await;

        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(Some("OVERRIDE".to_string())),
            "system_override was dropped in the actor round-trip"
        );

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn get_status_idle() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-status".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();

        let status = get_agent_status(&actor_ref).await.unwrap();
        assert_eq!(status, AgentStatus::Idle);

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn token_usage_accumulates() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-tokens".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();

        // Execute twice
        execute_agent(&actor_ref, AgentInput::text("first"))
            .await
            .unwrap();
        execute_agent(&actor_ref, AgentInput::text("second"))
            .await
            .unwrap();

        // Check accumulated token usage
        let (tx, rx) = oneshot::channel();
        actor_ref.cast(AgentMessage::GetTokenUsage(tx)).unwrap();
        let usage = rx.await.unwrap();
        assert_eq!(usage.input_tokens, 20); // 10 + 10
        assert_eq!(usage.output_tokens, 10); // 5 + 5

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn failed_execution_returns_error() {
        let (actor_ref, handle) = AgentActor::spawn(
            Some("test-fail".to_string()),
            AgentActor,
            (test_config(), Box::new(FailBehavior)),
        )
        .await
        .unwrap();

        let result = execute_agent(&actor_ref, AgentInput::text("trigger failure")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("intentional failure"));

        // Actor may have crashed due to the error — wait for handle
        let _ = handle.await;
    }

    #[tokio::test]
    async fn custom_failure_merges_incomplete_subtotal_into_actor_cumulative_usage() {
        // Invoke the wrapper handler directly so the post-error actor state can
        // be queried deterministically before ractor applies supervision.
        let (dummy_ref, dummy_handle) = AgentActor::spawn(
            Some("custom-subtotal-dummy".to_string()),
            AgentActor,
            (test_config(), Box::new(EchoBehavior)),
        )
        .await
        .unwrap();
        let mut state = AgentActorState {
            config: test_config(),
            status: AgentStatus::Idle,
            behavior: Box::new(IncompleteSubtotalFailureBehavior),
            token_usage: MeasuredTokenUsage::known(TokenUsageStats::default()),
            last_active: std::time::Instant::now(),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let handler_result = AgentActor
            .handle(
                dummy_ref.clone(),
                AgentMessage::Execute {
                    input: AgentInput::text("fail after spending"),
                    reply: reply_tx,
                    sink: None,
                    control: None,
                },
                &mut state,
            )
            .await;
        assert!(handler_result.is_err());
        let failure = reply_rx.await.unwrap().unwrap_err();
        let expected = MeasuredTokenUsage::lower_bound(TokenUsageStats::new(80, 20));
        assert_eq!(failure.token_usage, expected);

        let (usage_tx, usage_rx) = oneshot::channel();
        AgentActor
            .handle(
                dummy_ref.clone(),
                AgentMessage::GetMeasuredTokenUsage(usage_tx),
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(usage_rx.await.unwrap(), expected);

        dummy_ref.stop(None);
        dummy_handle.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_agents_independent() {
        let (ref1, h1) = AgentActor::spawn(
            Some("agent-1".to_string()),
            AgentActor,
            (
                AgentConfig {
                    id: AgentId::new("agent-1"),
                    ..AgentConfig::default()
                },
                Box::new(EchoBehavior),
            ),
        )
        .await
        .unwrap();

        let (ref2, h2) = AgentActor::spawn(
            Some("agent-2".to_string()),
            AgentActor,
            (
                AgentConfig {
                    id: AgentId::new("agent-2"),
                    ..AgentConfig::default()
                },
                Box::new(EchoBehavior),
            ),
        )
        .await
        .unwrap();

        let out1 = execute_agent(&ref1, AgentInput::text("from agent 1")).await;
        let out2 = execute_agent(&ref2, AgentInput::text("from agent 2")).await;

        assert_eq!(out1.unwrap().content, "Echo: from agent 1");
        assert_eq!(out2.unwrap().content, "Echo: from agent 2");

        ref1.stop(None);
        ref2.stop(None);
        h1.await.unwrap();
        h2.await.unwrap();
    }

    /// Records whether `on_consolidate` actually ran.
    struct ConsolidateTracker(std::sync::Arc<std::sync::atomic::AtomicBool>);

    #[async_trait::async_trait]
    impl AgentBehavior for ConsolidateTracker {
        async fn on_start(&mut self, _: &AgentConfig) -> Result<(), crate::AgentError> {
            Ok(())
        }
        async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, crate::AgentError> {
            Ok(AgentOutput {
                content: input.content,
                tool_calls: vec![],
                token_usage: TokenUsageStats::new(1, 1),
            })
        }
        async fn on_consolidate(
            &mut self,
        ) -> Result<crate::behavior::ConsolidationReport, crate::AgentError> {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::behavior::ConsolidationReport {
                promoted: 1,
                ..Default::default()
            })
        }
        async fn on_stop(&mut self) -> Result<(), crate::AgentError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn consolidate_respects_idle_gate() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let (actor, h) = AgentActor::spawn(
            Some("consolidate-gate".to_string()),
            AgentActor,
            (test_config(), Box::new(ConsolidateTracker(ran.clone()))),
        )
        .await
        .unwrap();

        // Just spawned → not idle for an hour → skipped, on_consolidate not run.
        let r = consolidate_agent(&actor, 3600).await.unwrap();
        assert!(r.skipped);
        assert!(!ran.load(Ordering::SeqCst));

        // Threshold 0 → idle "long enough" → on_consolidate runs.
        let r2 = consolidate_agent(&actor, 0).await.unwrap();
        assert!(!r2.skipped);
        assert!(ran.load(Ordering::SeqCst));

        actor.stop(None);
        let _ = h.await;
    }
}
